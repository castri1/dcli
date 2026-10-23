//! Reading Cost Explorer. Every request is counted ([`super::settings::count_requests`]) and
//! sent at most once more, only when AWS throttled it: AWS bills each one.
//!
//! A reading ([`read`]) takes [`REQUESTS`] requests when every answer fits in one page (each
//! further page is one more):
//! 1. GetCostAndUsage, daily from the first day of last month, by service: the totals, this
//!    month against last, the daily series;
//! 2. GetCostAndUsage, the last 30 days by service and usage type, cost and quantity: the
//!    account's effective rates;
//! 3. GetCostForecast, from today to the end of the month;
//! 4. GetSavingsPlansCoverage, the last 30 days by service;
//! 5. GetSavingsPlansUtilizationDetails, the last 30 days, plan by plan;
//! 6. GetCostAndUsage, the last 30 days by service and purchase type: the share reservations
//!    cover;
//! 7. GetReservationUtilization, the last 30 days, reservation by reservation (RDS, Redshift,
//!    ElastiCache, OpenSearch);
//! 8. GetSavingsPlansPurchaseRecommendation (Compute, one year, no upfront, last 30 days).
//!
//! DescribeSavingsPlans (each plan's end and what it covers) and STS GetCallerIdentity (whose
//! account) are other services, not billed per request.

use std::cell::Cell;
use std::collections::BTreeMap;
use std::sync::Arc;

use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use super::{Commitment, Coverage, Recommendation, ServiceCost, Snapshot, UsageCost, settings};
use crate::aws::profile::AwsFiles;
use crate::aws::{self, Aws, AwsError, ce, credentials, savingsplans, sts};
use crate::util::{parse_rfc3339, sanitize, utc_parts};

/// Cost Explorer answers from us-east-1.
pub const REGION: &str = "us-east-1";

/// Cost Explorer requests in a reading whose answers each fit in one page.
pub const REQUESTS: u64 = 8;

/// Pages read at most per request kind.
const MAX_PAGES: usize = 10;

const DAY: f64 = 86_400.0;

/// Days the usage types, coverage and commitments look back over.
pub const WINDOW_DAYS: f64 = 30.0;

/// The services whose reservations a reading looks at.
const RESERVED: &[&str] = &["Amazon Relational Database Service", "Amazon Redshift", "Amazon ElastiCache", "Amazon OpenSearch Service"];

/// One Cost Explorer request, counted.
pub fn call<T: DeserializeOwned>(aws: &Aws, operation: &str, input: &Value) -> aws::Result<T> {
    settings::count_requests(1);
    aws.call_once(&ce::SERVICE, operation, input)
}

/// Who reads the costs of an environment whose AWS client is `env_aws`: the profile set in
/// the costs settings (the management account's), else the environment's own; in us-east-1.
pub fn billing(env_aws: &Aws) -> Arc<Aws> {
    match settings::load().profile.filter(|p| !p.is_empty()) {
        Some(profile) => Aws::new(&profile, REGION),
        None => env_aws.in_region(REGION),
    }
}

/// The filter that keeps `account`'s costs, for a reader that sees several (the management
/// account); `None` when the environment reads its own.
pub fn account_filter(account: &str) -> Option<Value> {
    settings::load()
        .profile
        .filter(|p| !p.is_empty())
        .map(|_| json!({ "Dimensions": { "Key": "LINKED_ACCOUNT", "Values": [account] } }))
}

// ---- the account ---------------------------------------------------------------------------

/// The AWS account the environment's `profile` signs in to: the profile's `sso_account_id`,
/// else what STS says (free), remembered in the costs settings. Credentials taken from the
/// environment variables are asked every time: they can change between runs.
pub fn account_of(profile: &str, env_aws: &Aws) -> aws::Result<String> {
    let name = credentials::effective_profile(profile);
    let from_env = profile.is_empty() && std::env::var("AWS_ACCESS_KEY_ID").is_ok_and(|v| !v.is_empty());
    if !from_env {
        let configured = AwsFiles::load().profile(&name).and_then(|p| p.get("sso_account_id").cloned());
        if let Some(account) = configured.filter(|a| sts::is_account_id(a)) {
            settings::remember_account(&name, &account);
            return Ok(account);
        }
        if let Some(account) = settings::load().accounts.get(&name) {
            return Ok(account.clone());
        }
    }
    let account = sts::caller_account(env_aws)?;
    if !from_env {
        settings::remember_account(&name, &account);
    }
    Ok(account)
}

/// Who reads the costs, in words.
pub fn reader_label(settings: &settings::Settings) -> String {
    match settings.reader() {
        Some(p) => format!("the management account (profile {p})"),
        None => "the environment's profile".into(),
    }
}

// ---- dates ---------------------------------------------------------------------------------

/// The start (00:00 UTC) of `t`'s day.
pub fn day_start(t: f64) -> f64 {
    (t / DAY).floor() * DAY
}

/// `2026-10-23`.
pub fn date(t: f64) -> String {
    let (y, m, d, ..) = utc_parts(t as i64);
    format!("{y:04}-{m:02}-{d:02}")
}

pub fn parse_date(s: &str) -> Option<f64> {
    parse_rfc3339(&format!("{}T00:00:00Z", s.get(..10)?))
}

/// The first day of `t`'s month.
pub fn month_start(t: f64) -> f64 {
    let (_, _, d, ..) = utc_parts(t as i64);
    day_start(t) - f64::from(d - 1) * DAY
}

/// The first day of the month after `t`'s.
pub fn next_month(t: f64) -> f64 {
    let first = month_start(t);
    let month = utc_parts(first as i64).1;
    let mut x = first + 28.0 * DAY;
    while utc_parts(x as i64).1 == month {
        x += DAY;
    }
    x
}

/// The days a reading looks at, from `now`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Window {
    pub today: f64,
    pub this_month: f64,
    pub last_month: f64,
    /// 30 days before today: the start of the usage, coverage and commitments period.
    pub since: f64,
}

impl Window {
    pub fn at(now: f64) -> Window {
        let today = day_start(now);
        let this_month = month_start(now);
        Window { today, this_month, last_month: month_start(this_month - DAY), since: today - WINDOW_DAYS * DAY }
    }

    /// The end (exclusive) of last month's days that match this month's so far: the 1st to
    /// the 23rd of both, today included.
    fn last_month_to_date_end(&self) -> f64 {
        (self.last_month + (self.today - self.this_month) + DAY).min(self.this_month)
    }
}

// ---- reading -------------------------------------------------------------------------------

/// The Cost Explorer requests of one reading, counted as they go.
struct Reader<'a> {
    aws: &'a Aws,
    /// Keeps the environment's account when the reader sees several.
    account: Option<Value>,
    requests: Cell<u64>,
}

impl Reader<'_> {
    fn call(&self, operation: &str, input: &Value) -> aws::Result<Value> {
        self.requests.set(self.requests.get() + 1);
        call(self.aws, operation, input)
    }

    /// Every page of `operation`; `(asked, answered)` name its page token in the request and
    /// in the answer. Each page is a request.
    fn pages(&self, operation: &str, mut input: Value, (asked, answered): (&str, &str)) -> aws::Result<Vec<Value>> {
        let mut out = Vec::new();
        for _ in 0..MAX_PAGES {
            let page = self.call(operation, &input)?;
            let next = page.get(answered).and_then(Value::as_str).filter(|t| !t.is_empty()).map(str::to_string);
            out.push(page);
            match next {
                Some(t) => input[asked] = json!(t),
                None => break,
            }
        }
        Ok(out)
    }

    /// `input` with `extra` and the account filter, when there are any.
    fn filtered(&self, mut input: Value, extra: Option<Value>) -> Value {
        let filter = match (self.account.clone(), extra) {
            (Some(a), Some(b)) => Some(json!({ "And": [a, b] })),
            (a, b) => a.or(b),
        };
        if let Some(f) = filter {
            input["Filter"] = f;
        }
        input
    }
}

/// Bill lines other than credits and refunds: totals and the forecast leave those out.
fn no_credits() -> Value {
    json!({ "Not": { "Dimensions": { "Key": "RECORD_TYPE", "Values": ["Credit", "Refund"] } } })
}

/// Usage lines, with what commitments cover of them: what effective rates come from.
fn usage_lines() -> Value {
    json!({ "Dimensions": { "Key": "RECORD_TYPE", "Values": ["Usage", "DiscountedUsage", "SavingsPlanCoveredUsage"] } })
}

fn period(from: f64, to: f64) -> Value {
    json!({ "Start": date(from), "End": date(to) })
}

/// A metric's amount in a Cost Explorer answer (`{"AmortizedCost": {"Amount": "1.2"}}`).
fn amount(metrics: &Value, metric: &str) -> f64 {
    number(metrics.get(metric).and_then(|m| m.get("Amount")))
}

fn number(v: Option<&Value>) -> f64 {
    match v {
        Some(Value::String(s)) => s.trim().parse().unwrap_or(0.0),
        Some(Value::Number(n)) => n.as_f64().unwrap_or(0.0),
        _ => 0.0,
    }
}

fn unit(metrics: &Value, metric: &str) -> String {
    sanitize(metrics.get(metric).and_then(|m| m.get("Unit")).and_then(Value::as_str).unwrap_or_default())
}

/// A group's key `i` (`Keys: ["AWS Lambda", "USE1-Lambda-GB-Second"]`).
fn key(group: &Value, i: usize) -> String {
    sanitize(group.get("Keys").and_then(|k| k.get(i)).and_then(Value::as_str).unwrap_or_default())
}

fn each<'a>(pages: &'a [Value], list: &'a str) -> impl Iterator<Item = &'a Value> {
    pages.iter().flat_map(move |p| p.get(list).and_then(Value::as_array).into_iter().flatten())
}

/// Reads `env`'s costs from Cost Explorer as of `now`, keeps them on disk and returns them.
/// `profile` is the environment's (it says which account is the environment's); `progress`
/// hears each step. Only the daily costs are needed: a part that fails is noted in
/// [`Snapshot::missing`] and the rest is kept.
pub fn read(env: &str, profile: &str, env_aws: &Aws, now: f64, progress: &dyn Fn(&str)) -> aws::Result<Snapshot> {
    progress("finding the account");
    let account = account_of(profile, env_aws)?;
    let settings = settings::load();
    if let Some(mut recent) = recent_reading(env, &account, &reader_label(&settings), now) {
        // Another environment in the same account was just read: the same figures, for free.
        recent.requests = 0;
        if let Err(e) = super::save(env, &recent) {
            recent.missing.push(format!("not kept on disk: {e}"));
        }
        return Ok(recent);
    }
    let reader_aws = billing(env_aws);
    let r = Reader { aws: &reader_aws, account: account_filter(&account), requests: Cell::new(0) };
    let w = Window::at(now);
    let mut s = Snapshot { account, read_at: now, currency: "USD".into(), reader: reader_label(&settings), ..Default::default() };
    let by_org = settings.reader().is_some();

    progress("costs by day");
    let pages = daily(&r, &w).map_err(|e| explain(e, true))?;
    if let Some(c) = each(&pages, "ResultsByTime").find_map(|t| t.get("Total").map(|m| unit(m, "AmortizedCost"))).filter(|c| !c.is_empty()) {
        s.currency = c;
    }
    s.services = services_from_daily(&pages, &w);
    s.month_to_date = s.services.iter().map(|x| x.month_to_date).sum();
    s.last_month = s.services.iter().map(|x| x.last_month).sum();
    s.last_month_to_date = s.services.iter().map(|x| x.last_month_to_date).sum();

    /// Notes a part that could not be read; an expired session stops the reading instead.
    fn note(s: &mut Snapshot, what: &str, e: AwsError) -> aws::Result<()> {
        if e.is_login_required() {
            return Err(e);
        }
        s.missing.push(format!("{what}: {}", explain(e, false).short()));
        Ok(())
    }

    progress("usage types");
    match usage(&r, &w) {
        Ok(u) => s.usage = u,
        Err(e) => note(&mut s, "usage types and rates", e)?,
    }
    progress("forecast");
    match forecast(&r, &w) {
        Ok(rest) => {
            let before_today: f64 = s.services.iter().flat_map(|x| &x.daily).filter(|(t, _)| *t >= w.this_month && *t < w.today).map(|(_, c)| c).sum();
            s.forecast = Some(before_today + rest);
        }
        Err(e) => note(&mut s, "forecast", e)?,
    }
    progress("Savings Plans coverage");
    match sp_coverage(&r, &w) {
        Ok(c) => s.coverage.extend(c),
        Err(e) => note(&mut s, "Savings Plans coverage", e)?,
    }
    progress("Savings Plans");
    let utilization = match sp_utilization(&r, &w) {
        Ok(u) => u,
        Err(e) => {
            note(&mut s, "Savings Plans utilization", e)?;
            BTreeMap::new()
        }
    };
    match savingsplans::active(&reader_aws) {
        Ok(plans) => s.commitments.extend(plans.iter().map(|p| plan_commitment(p, &utilization, by_org))),
        Err(e) => note(&mut s, "Savings Plans (DescribeSavingsPlans)", e)?,
    }
    progress("reservations");
    match reserved_share(&r, &w) {
        Ok(c) => s.coverage.extend(c),
        Err(e) => note(&mut s, "reservation coverage", e)?,
    }
    match reservations(&r, &w, by_org) {
        Ok(c) => s.commitments.extend(c),
        Err(e) => note(&mut s, "reservations", e)?,
    }
    progress("purchase recommendation");
    match recommendation(&r, by_org) {
        Ok(rec) => s.recommendation = rec,
        Err(e) => note(&mut s, "Savings Plans recommendation", e)?,
    }
    s.requests = r.requests.get();
    if let Err(e) = super::save(env, &s) {
        s.missing.push(format!("not kept on disk: {e}"));
    }
    Ok(s)
}

/// A reading of `account` by the same reader kept for another environment less than
/// [`REUSE_SECS`] ago (environments often share an account; Cost Explorer only updates a few
/// times a day), marked with where it came from.
fn recent_reading(env: &str, account: &str, reader: &str, now: f64) -> Option<Snapshot> {
    let own = super::cache_path(env)?;
    let dir = own.parent()?;
    let mut best: Option<(String, Snapshot)> = None;
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let path = entry.path();
        let name = path.file_name()?.to_string_lossy().into_owned();
        let Some(other) = name.strip_prefix("costs-").and_then(|n| n.strip_suffix(".json")) else { continue };
        if path == own {
            continue;
        }
        let Some(s) = std::fs::read_to_string(&path).ok().and_then(|t| serde_json::from_str::<Snapshot>(&t).ok()) else { continue };
        let fresh = now - s.read_at < REUSE_SECS && s.read_at <= now;
        if s.account == account && s.reader == reader && fresh && best.as_ref().is_none_or(|(_, b)| s.read_at > b.read_at) {
            best = Some((other.to_string(), s));
        }
    }
    best.map(|(from, mut s)| {
        s.copied_from = Some(from);
        s
    })
}

/// How long a reading serves the other environments of its account.
pub const REUSE_SECS: f64 = 3600.0;

/// Cost Explorer errors that say what to do about them.
fn explain(e: AwsError, essential: bool) -> AwsError {
    let AwsError::Api { code, message, status } = &e else { return e };
    let lower = message.to_lowercase();
    let text = if lower.contains("not enabled for cost explorer") || lower.contains("enable cost explorer") {
        "Cost Explorer is not enabled for this account: open it once in the AWS console (Billing and Cost Management → \
         Cost Explorer) with the management account; data shows within a day"
            .to_string()
    } else if code.contains("AccessDenied") || code.contains("UnauthorizedOperation") || code.contains("NotAuthorized") {
        if essential {
            "this profile cannot read Cost Explorer: it needs ce:GetCostAndUsage and the other ce:Get* actions (s chooses \
             another profile, the management account's)"
                .to_string()
        } else {
            "no permission".to_string()
        }
    } else if code.contains("DataUnavailable") {
        "Cost Explorer has no data for this period yet".to_string()
    } else {
        return e;
    };
    AwsError::Api { status: *status, code: code.clone(), message: text }
}

fn daily(r: &Reader, w: &Window) -> aws::Result<Vec<Value>> {
    let input = json!({
        "TimePeriod": period(w.last_month, w.today + DAY),
        "Granularity": "DAILY",
        "Metrics": ["AmortizedCost"],
        "GroupBy": [{ "Type": "DIMENSION", "Key": "SERVICE" }],
    });
    r.pages("GetCostAndUsage", r.filtered(input, Some(no_credits())), ("NextPageToken", "NextPageToken"))
}

/// Per service, from daily costs: this month so far, last month, last month to the same
/// day, and the series; the most expensive first.
pub fn services_from_daily(pages: &[Value], w: &Window) -> Vec<ServiceCost> {
    let mut by_service: BTreeMap<String, BTreeMap<i64, f64>> = BTreeMap::new();
    for result in each(pages, "ResultsByTime") {
        let Some(day) = result.get("TimePeriod").and_then(|p| p.get("Start")).and_then(Value::as_str).and_then(parse_date) else { continue };
        for group in result.get("Groups").and_then(Value::as_array).into_iter().flatten() {
            let cost = amount(group.get("Metrics").unwrap_or(&Value::Null), "AmortizedCost");
            *by_service.entry(key(group, 0)).or_default().entry(day as i64).or_default() += cost;
        }
    }
    let to_date_end = w.last_month_to_date_end();
    let mut out: Vec<ServiceCost> = by_service
        .into_iter()
        .filter(|(name, _)| !name.is_empty())
        .map(|(service, days)| {
            let sum = |from: f64, to: f64| days.iter().filter(|(t, _)| (**t as f64) >= from && (**t as f64) < to).map(|(_, c)| c).sum::<f64>();
            ServiceCost {
                month_to_date: sum(w.this_month, f64::INFINITY),
                last_month: sum(w.last_month, w.this_month),
                last_month_to_date: sum(w.last_month, to_date_end),
                daily: days.iter().map(|(t, c)| (*t as f64, *c)).collect(),
                service,
            }
        })
        .collect();
    out.sort_by(|a, b| b.month_to_date.total_cmp(&a.month_to_date).then(b.last_month.total_cmp(&a.last_month)));
    out
}

fn usage(r: &Reader, w: &Window) -> aws::Result<Vec<UsageCost>> {
    let input = json!({
        "TimePeriod": period(w.since, w.today),
        "Granularity": "MONTHLY",
        "Metrics": ["AmortizedCost", "UsageQuantity"],
        "GroupBy": [{ "Type": "DIMENSION", "Key": "SERVICE" }, { "Type": "DIMENSION", "Key": "USAGE_TYPE" }],
    });
    let pages = r.pages("GetCostAndUsage", r.filtered(input, Some(usage_lines())), ("NextPageToken", "NextPageToken"))?;
    Ok(usage_from(&pages))
}

/// Usage types summed over the calendar-month pieces of the period, the costliest first.
pub fn usage_from(pages: &[Value]) -> Vec<UsageCost> {
    let mut by: BTreeMap<(String, String), UsageCost> = BTreeMap::new();
    for result in each(pages, "ResultsByTime") {
        for group in result.get("Groups").and_then(Value::as_array).into_iter().flatten() {
            let metrics = group.get("Metrics").unwrap_or(&Value::Null);
            let (service, usage_type) = (key(group, 0), key(group, 1));
            let u = by.entry((service.clone(), usage_type.clone())).or_insert_with(|| UsageCost { service, usage_type, ..Default::default() });
            u.cost += amount(metrics, "AmortizedCost");
            u.quantity += amount(metrics, "UsageQuantity");
            if u.unit.is_empty() {
                u.unit = unit(metrics, "UsageQuantity");
            }
        }
    }
    let mut out: Vec<UsageCost> = by.into_values().filter(|u| !u.usage_type.is_empty() && (u.cost != 0.0 || u.quantity != 0.0)).collect();
    out.sort_by(|a, b| b.cost.total_cmp(&a.cost));
    out
}

/// What the rest of the month (today included) should cost.
fn forecast(r: &Reader, w: &Window) -> aws::Result<f64> {
    let input = json!({
        "TimePeriod": period(w.today, next_month(w.today)),
        "Metric": "AMORTIZED_COST",
        "Granularity": "MONTHLY",
    });
    let out = r.call("GetCostForecast", &r.filtered(input, Some(no_credits())))?;
    Ok(number(out.get("Total").and_then(|t| t.get("Amount"))))
}

/// A Cost Explorer attribute map's value for `name`, whatever its case.
fn attribute(attributes: Option<&Value>, name: &str) -> Option<String> {
    attributes?.as_object()?.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).and_then(|(_, v)| v.as_str()).map(sanitize)
}

fn sp_coverage(r: &Reader, w: &Window) -> aws::Result<Vec<Coverage>> {
    let input = json!({
        "TimePeriod": period(w.since, w.today),
        "Granularity": "MONTHLY",
        "GroupBy": [{ "Type": "DIMENSION", "Key": "SERVICE" }],
    });
    let pages = r.pages("GetSavingsPlansCoverage", r.filtered(input, None), ("NextToken", "NextToken"))?;
    Ok(coverage_from(&pages))
}

/// Savings Plans coverage per service, summed over the period's pieces.
pub fn coverage_from(pages: &[Value]) -> Vec<Coverage> {
    let mut by: BTreeMap<String, (f64, f64, f64)> = BTreeMap::new();
    for c in each(pages, "SavingsPlansCoverages") {
        let Some(service) = attribute(c.get("Attributes"), "SERVICE") else { continue };
        let figures = c.get("Coverage").unwrap_or(&Value::Null);
        let e = by.entry(service).or_default();
        e.0 += number(figures.get("SpendCoveredBySavingsPlans"));
        e.1 += number(figures.get("OnDemandCost"));
        e.2 += number(figures.get("TotalCost"));
    }
    by.into_iter()
        .filter(|(_, (_, _, total))| *total > 0.0)
        .map(|(service, (covered, on_demand, total))| Coverage {
            service,
            covered: (covered / total).clamp(0.0, 1.0),
            on_demand_cost: on_demand,
            by: "Savings Plans".into(),
        })
        .collect()
}

/// Each plan's (utilization 0 to 1, unused commitment) over the period, by ARN.
fn sp_utilization(r: &Reader, w: &Window) -> aws::Result<BTreeMap<String, (f64, f64)>> {
    let input = json!({ "TimePeriod": period(w.since, w.today) });
    // The plans are the payer's when it reads: no account filter on what they are.
    let pages = r.pages("GetSavingsPlansUtilizationDetails", input, ("NextToken", "NextToken"))?;
    Ok(utilization_from(&pages))
}

pub fn utilization_from(pages: &[Value]) -> BTreeMap<String, (f64, f64)> {
    each(pages, "SavingsPlansUtilizationDetails")
        .filter_map(|d| {
            let arn = d.get("SavingsPlanArn").and_then(Value::as_str)?;
            let u = d.get("Utilization")?;
            Some((sanitize(arn), (number(u.get("UtilizationPercentage")) / 100.0, number(u.get("UnusedCommitment").and_then(|c| c.get("Amount")).or(u.get("UnusedCommitment"))))))
        })
        .collect()
}

/// The services a Savings Plan's product types are, in Cost Explorer's names.
fn plan_services(products: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    for p in products {
        let names: &[&str] = match p.to_ascii_lowercase().as_str() {
            "ec2" => &["Amazon Elastic Compute Cloud - Compute"],
            "fargate" => &["Amazon Elastic Container Service", "Amazon Elastic Kubernetes Service"],
            "lambda" => &["AWS Lambda"],
            "sagemaker" => &["Amazon SageMaker"],
            "rds" => &["Amazon Relational Database Service"],
            "dynamodb" => &["Amazon DynamoDB"],
            "elasticache" => &["Amazon ElastiCache"],
            "dms" => &["AWS Database Migration Service"],
            "redshift" => &["Amazon Redshift"],
            _ => &[],
        };
        out.extend(names.iter().map(|s| s.to_string()));
    }
    out
}

fn plan_commitment(p: &savingsplans::SavingsPlan, utilization: &BTreeMap<String, (f64, f64)>, by_org: bool) -> Commitment {
    let kind = match p.savings_plan_type.as_str() {
        "Compute" => "Compute Savings Plan".to_string(),
        "EC2Instance" => "EC2 Instance Savings Plan".to_string(),
        other => format!("{} Savings Plan", sanitize(other)),
    };
    let used = utilization.get(&p.savings_plan_arn);
    Commitment {
        kind,
        id: sanitize(&p.savings_plan_id),
        services: plan_services(&p.product_types),
        hourly: p.commitment.trim().parse().unwrap_or(0.0),
        end: parse_rfc3339(&p.end),
        utilization: used.map(|u| u.0),
        unused_cost: used.map(|u| u.1),
        scope: if by_org { "organization".into() } else { "this account".into() },
    }
}

/// The share of each reservable service's cost that reservations cover, by purchase type.
fn reserved_share(r: &Reader, w: &Window) -> aws::Result<Vec<Coverage>> {
    let input = json!({
        "TimePeriod": period(w.since, w.today),
        "Granularity": "MONTHLY",
        "Metrics": ["AmortizedCost"],
        "GroupBy": [{ "Type": "DIMENSION", "Key": "SERVICE" }, { "Type": "DIMENSION", "Key": "PURCHASE_TYPE" }],
    });
    let pages = r.pages("GetCostAndUsage", r.filtered(input, Some(usage_lines())), ("NextPageToken", "NextPageToken"))?;
    Ok(reserved_from(&pages))
}

pub fn reserved_from(pages: &[Value]) -> Vec<Coverage> {
    let mut by: BTreeMap<String, (f64, f64)> = BTreeMap::new();
    for result in each(pages, "ResultsByTime") {
        for group in result.get("Groups").and_then(Value::as_array).into_iter().flatten() {
            let service = key(group, 0);
            if !RESERVED.contains(&service.as_str()) {
                continue;
            }
            let cost = amount(group.get("Metrics").unwrap_or(&Value::Null), "AmortizedCost");
            let e = by.entry(service).or_default();
            e.1 += cost;
            if key(group, 1).to_lowercase().contains("reserved") {
                e.0 += cost;
            }
        }
    }
    by.into_iter()
        .filter(|(_, (_, total))| *total > 0.0)
        .map(|(service, (reserved, total))| Coverage {
            service,
            covered: (reserved / total).clamp(0.0, 1.0),
            on_demand_cost: total - reserved,
            by: "reservations".into(),
        })
        .collect()
}

fn reservations(r: &Reader, w: &Window, by_org: bool) -> aws::Result<Vec<Commitment>> {
    let input = json!({
        "TimePeriod": period(w.since, w.today),
        "GroupBy": [{ "Type": "DIMENSION", "Key": "SUBSCRIPTION_ID" }],
        "Filter": { "Dimensions": { "Key": "SERVICE", "Values": RESERVED } },
    });
    let pages = r.pages("GetReservationUtilization", input, ("NextPageToken", "NextPageToken"))?;
    Ok(reservations_from(&pages, by_org))
}

/// The service a reservation is for, from its instance type (`db.r6g.large`, `ra3.4xlarge`).
fn reserved_service(instance_type: &str) -> &'static str {
    match instance_type {
        t if t.starts_with("db.") => "Amazon Relational Database Service",
        t if t.starts_with("cache.") => "Amazon ElastiCache",
        t if t.starts_with("ra3.") || t.starts_with("dc2.") || t.starts_with("ds2.") => "Amazon Redshift",
        t if t.ends_with(".search") || t.contains(".search") => "Amazon OpenSearch Service",
        _ => "",
    }
}

pub fn reservations_from(pages: &[Value], by_org: bool) -> Vec<Commitment> {
    let mut out = Vec::new();
    for period in each(pages, "UtilizationsByTime") {
        let hours = match (
            period.get("TimePeriod").and_then(|p| p.get("Start")).and_then(Value::as_str).and_then(parse_date),
            period.get("TimePeriod").and_then(|p| p.get("End")).and_then(Value::as_str).and_then(parse_date),
        ) {
            (Some(a), Some(b)) if b > a => (b - a) / 3600.0,
            _ => WINDOW_DAYS * 24.0,
        };
        for g in period.get("Groups").and_then(Value::as_array).into_iter().flatten() {
            let attributes = g.get("Attributes");
            let instance_type = attribute(attributes, "instanceType").unwrap_or_default();
            let count = attribute(attributes, "numberOfInstances").unwrap_or_else(|| "1".into());
            let u = g.get("Utilization").unwrap_or(&Value::Null);
            let fee = number(u.get("TotalAmortizedFee"));
            let (purchased, unused) = (number(u.get("PurchasedHours")), number(u.get("UnusedHours")));
            let service = reserved_service(&instance_type);
            out.push(Commitment {
                kind: match service {
                    "Amazon Relational Database Service" => "RDS reserved instances".into(),
                    "Amazon Redshift" => "Redshift reserved nodes".into(),
                    "Amazon ElastiCache" => "ElastiCache reserved nodes".into(),
                    _ => "Reservation".into(),
                },
                id: format!("{count} × {instance_type}"),
                services: if service.is_empty() { Vec::new() } else { vec![service.to_string()] },
                hourly: fee / hours,
                end: attribute(attributes, "endDateTime").and_then(|e| parse_rfc3339(&e)),
                utilization: Some(number(u.get("UtilizationPercentage")) / 100.0),
                unused_cost: (purchased > 0.0).then(|| fee * unused / purchased),
                scope: if by_org { "organization".into() } else { "this account".into() },
            });
        }
    }
    out
}

fn recommendation(r: &Reader, by_org: bool) -> aws::Result<Option<Recommendation>> {
    let input = json!({
        "SavingsPlansType": "COMPUTE_SP",
        "TermInYears": "ONE_YEAR",
        "PaymentOption": "NO_UPFRONT",
        "LookbackPeriodInDays": "THIRTY_DAYS",
        "AccountScope": if by_org { "PAYER" } else { "LINKED" },
    });
    let out = r.call("GetSavingsPlansPurchaseRecommendation", &input)?;
    Ok(recommendation_from(&out))
}

/// AWS's recommendation, or `None` when it has none (or has not generated it yet).
pub fn recommendation_from(out: &Value) -> Option<Recommendation> {
    let rec = out.get("SavingsPlansPurchaseRecommendation")?;
    let summary = rec.get("SavingsPlansPurchaseRecommendationSummary")?;
    let hourly = number(summary.get("HourlyCommitmentToPurchase"));
    (hourly > 0.0).then(|| Recommendation {
        kind: "Compute Savings Plan".into(),
        hourly_commitment: hourly,
        estimated_monthly_savings: number(summary.get("EstimatedMonthlySavingsAmount")),
        lookback_days: 30,
        term: rec.get("TermInYears").and_then(Value::as_str).map_or_else(|| "ONE_YEAR".into(), sanitize),
        payment: rec.get("PaymentOption").and_then(Value::as_str).map_or_else(|| "NO_UPFRONT".into(), sanitize),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(s: &str) -> f64 {
        parse_rfc3339(s).unwrap()
    }

    #[test]
    fn month_boundaries() {
        assert_eq!(date(month_start(at("2026-10-23T15:00:00Z"))), "2026-10-01");
        assert_eq!(date(next_month(at("2026-10-23T15:00:00Z"))), "2026-11-01");
        assert_eq!(date(next_month(at("2026-12-31T23:00:00Z"))), "2027-01-01");
        assert_eq!(date(next_month(at("2028-02-10T00:00:00Z"))), "2028-03-01");
        let w = Window::at(at("2026-03-31T10:00:00Z"));
        assert_eq!((date(w.today), date(w.this_month), date(w.last_month)), ("2026-03-31".into(), "2026-03-01".into(), "2026-02-01".into()));
        // The 31st of March compares with the whole of February, no more.
        assert_eq!(date(w.last_month_to_date_end()), "2026-03-01");
        let w = Window::at(at("2026-10-23T10:00:00Z"));
        assert_eq!(date(w.last_month_to_date_end()), "2026-09-24");
        assert_eq!(date(w.since), "2026-09-23");
        // On the last day of a month the forecast still covers one day.
        let last = Window::at(at("2026-10-31T22:00:00Z"));
        assert_eq!(period(last.today, next_month(last.today)), json!({ "Start": "2026-10-31", "End": "2026-11-01" }));
    }

    #[test]
    fn daily_costs_make_months_and_series() {
        let w = Window::at(at("2026-10-03T12:00:00Z"));
        let day = |d: &str, lambda: &str, s3: &str| {
            json!({ "TimePeriod": { "Start": d }, "Groups": [
                { "Keys": ["AWS Lambda"], "Metrics": { "AmortizedCost": { "Amount": lambda, "Unit": "USD" } } },
                { "Keys": ["Amazon Simple Storage Service"], "Metrics": { "AmortizedCost": { "Amount": s3, "Unit": "USD" } } },
            ]})
        };
        // Two pages, as when Cost Explorer splits a long answer.
        let pages = [
            json!({ "ResultsByTime": [day("2026-09-01", "1", "5"), day("2026-09-02", "2", "5"), day("2026-09-04", "4", "5")], "NextPageToken": "x" }),
            json!({ "ResultsByTime": [day("2026-10-01", "10", "1"), day("2026-10-02", "20", "1"), day("2026-10-03", "3", "1")] }),
        ];
        let services = services_from_daily(&pages, &w);
        assert_eq!(services[0].service, "AWS Lambda");
        let l = &services[0];
        assert_eq!((l.month_to_date, l.last_month, l.last_month_to_date), (33.0, 7.0, 3.0));
        assert_eq!(l.daily.len(), 6);
        assert_eq!(services[1].last_month, 15.0);
    }

    #[test]
    fn usage_types_sum_their_pieces() {
        let piece = |lambda_gbs: &str, cost: &str| {
            json!({ "Groups": [
                { "Keys": ["AWS Lambda", "Lambda-GB-Second"], "Metrics": {
                    "AmortizedCost": { "Amount": cost, "Unit": "USD" },
                    "UsageQuantity": { "Amount": lambda_gbs, "Unit": "Lambda-GB-Second" } } },
                { "Keys": ["AWS Lambda", "USW2-Request\u{1b}[31m"], "Metrics": {
                    "AmortizedCost": { "Amount": "0.2", "Unit": "USD" }, "UsageQuantity": { "Amount": "1000000", "Unit": "Requests" } } },
            ]})
        };
        let pages = [json!({ "ResultsByTime": [piece("600000", "10"), piece("400000", "6.6")] })];
        let usage = usage_from(&pages);
        assert_eq!(usage[0].usage_type, "Lambda-GB-Second");
        assert_eq!((usage[0].cost, usage[0].quantity, usage[0].unit.as_str()), (16.6, 1_000_000.0, "Lambda-GB-Second"));
        assert_eq!(usage[1].usage_type, "USW2-Request", "names are sanitized");
        let snapshot = Snapshot { usage, ..Default::default() };
        assert!((snapshot.rate("AWS Lambda", "Lambda-GB-Second").unwrap().0 - 0.0000166).abs() < 1e-12);
    }

    #[test]
    fn coverage_commitments_and_recommendation() {
        let pages = [json!({ "SavingsPlansCoverages": [
            { "Attributes": { "SERVICE": "AWS Lambda" }, "Coverage": { "SpendCoveredBySavingsPlans": "0", "OnDemandCost": "910", "TotalCost": "910" } },
            { "Attributes": { "service": "Amazon Elastic Compute Cloud - Compute" }, "Coverage": { "SpendCoveredBySavingsPlans": "1400", "OnDemandCost": "0", "TotalCost": "1400" } },
        ]})];
        let c = coverage_from(&pages);
        assert_eq!(c.len(), 2);
        assert_eq!((c[0].service.as_str(), c[0].covered), ("AWS Lambda", 0.0));
        assert_eq!(c[1].covered, 1.0);

        let used = utilization_from(&[json!({ "SavingsPlansUtilizationDetails": [
            { "SavingsPlanArn": "arn:sp/1", "Utilization": { "UtilizationPercentage": "97", "UnusedCommitment": { "Amount": "45.36" } } } ] })]);
        let plan = savingsplans::SavingsPlan {
            savings_plan_id: "sp-1".into(),
            savings_plan_arn: "arn:sp/1".into(),
            savings_plan_type: "Compute".into(),
            commitment: "2.1".into(),
            end: "2027-03-12T00:00:00Z".into(),
            product_types: vec!["EC2".into(), "Fargate".into(), "Lambda".into()],
            ..Default::default()
        };
        let c = plan_commitment(&plan, &used, true);
        assert_eq!((c.kind.as_str(), c.hourly, c.utilization, c.unused_cost), ("Compute Savings Plan", 2.1, Some(0.97), Some(45.36)));
        assert!(c.services.contains(&"AWS Lambda".to_string()) && c.scope == "organization");

        let ri = reservations_from(&[json!({ "UtilizationsByTime": [{
            "TimePeriod": { "Start": "2026-09-23", "End": "2026-10-23" },
            "Groups": [{ "Key": "SUBSCRIPTION_ID", "Value": "ri-1",
                "Attributes": { "instanceType": "db.r6g.xlarge", "numberOfInstances": "2", "endDateTime": "2027-01-10T00:00:00.000Z" },
                "Utilization": { "UtilizationPercentage": "100", "PurchasedHours": "1440", "UnusedHours": "0", "TotalAmortizedFee": "720" } }] }] })], false);
        assert_eq!(ri[0].services, ["Amazon Relational Database Service"]);
        assert_eq!((ri[0].id.as_str(), ri[0].hourly, ri[0].unused_cost), ("2 × db.r6g.xlarge", 1.0, Some(0.0)));

        let share = reserved_from(&[json!({ "ResultsByTime": [{ "Groups": [
            { "Keys": ["Amazon Relational Database Service", "Standard Reserved Instances"], "Metrics": { "AmortizedCost": { "Amount": "60" } } },
            { "Keys": ["Amazon Relational Database Service", "On Demand Instances"], "Metrics": { "AmortizedCost": { "Amount": "40" } } },
            { "Keys": ["AWS Lambda", "On Demand Instances"], "Metrics": { "AmortizedCost": { "Amount": "40" } } } ] }] })]);
        assert_eq!(share.len(), 1);
        assert_eq!((share[0].covered, share[0].by.as_str()), (0.6, "reservations"));

        assert!(recommendation_from(&json!({ "SavingsPlansPurchaseRecommendation": {} })).is_none());
        let rec = recommendation_from(&json!({ "SavingsPlansPurchaseRecommendation": { "TermInYears": "ONE_YEAR",
            "SavingsPlansPurchaseRecommendationSummary": { "HourlyCommitmentToPurchase": "0.80", "EstimatedMonthlySavingsAmount": "140.25" } } }))
        .unwrap();
        assert_eq!((rec.hourly_commitment, rec.estimated_monthly_savings), (0.8, 140.25));
    }

    #[test]
    fn errors_say_what_to_do() {
        let api = |code: &str, message: &str| AwsError::Api { status: 400, code: code.into(), message: message.into() };
        let e = explain(api("AccessDeniedException", "User not enabled for cost explorer access"), true);
        assert!(e.short().contains("not enabled"), "{}", e.short());
        assert!(explain(api("AccessDeniedException", "not authorized to perform ce:GetCostAndUsage"), true).short().contains("ce:GetCostAndUsage"));
        assert!(explain(api("AccessDeniedException", "x"), false).short().ends_with("no permission"));
        assert_eq!(explain(api("ValidationException", "bad"), true).short(), "ValidationException: bad");
    }
}
