//! `dcli dns`: Route 53. `dcli dns [ENV]` lists hosted zones; `dcli dns [ENV] ZONE` prints a
//! zone's records. `--create NAME` adds a record (values or an alias to an AWS resource) and
//! `--edit NAME` changes what one answers, keeping its routing policy and health check; both
//! ask first.

use std::process::ExitCode;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, bail};
use serde_json::{Value, json};

use crate::aws::route53::{self as r53, Alias, Answer, Candidate, Change, Record, Zone};
use crate::aws::{Aws, AwsError};
use crate::cli::{aws_for, confirm_write, current_env, paint, print_json};
use crate::config::{Config, is_production};

/// Records read at most from one zone.
const MAX_RECORDS: usize = 100_000;
/// How long `--wait` waits for a change to reach every Route 53 name server.
const WAIT: Duration = Duration::from_secs(300);

#[derive(clap::Args)]
pub struct Args {
    /// Environment (default: the current one); then the hosted zone
    #[arg(value_name = "ENV")]
    first: Option<String>,
    /// Hosted zone (name or id; the id when a public and a private zone share the name)
    #[arg(value_name = "ZONE")]
    second: Option<String>,
    /// Record type. Listing: only records of this type. --create: the new record's (A by
    /// default with --alias). --edit: the record's, when its name has several
    #[arg(long = "type", value_name = "TYPE")]
    kind: Option<String>,
    /// Listing: only records whose name contains this text
    #[arg(long, value_name = "TEXT", conflicts_with_all = ["create", "edit"])]
    name: Option<String>,
    #[arg(long)]
    json: bool,
    /// Create the record NAME: relative to the zone (api, @ for the apex) or a full name.
    /// Creating never overwrites: Route 53 refuses a record that exists
    #[arg(long, value_name = "NAME", conflicts_with = "edit")]
    create: Option<String>,
    /// Change what record NAME answers (values, TTL or alias target), in one atomic change
    /// that fails if the record changed meanwhile; its routing policy and health check stay
    #[arg(long, value_name = "NAME")]
    edit: Option<String>,
    /// With --edit: the set identifier of a record under a routing policy (weighted, failover…)
    #[arg(long, value_name = "ID", requires = "edit")]
    set_id: Option<String>,
    /// Repeatable, one value each: 192.0.2.10, '10 mail.example.com', a TXT text (quoted for
    /// you). With --edit it replaces every value
    #[arg(long = "value", value_name = "VALUE", conflicts_with = "alias")]
    values: Vec<String>,
    /// Seconds resolvers keep the answer (--create: 300 by default)
    #[arg(long, value_name = "SECONDS", conflicts_with = "alias")]
    ttl: Option<String>,
    /// Alias to an AWS resource: a CloudFront distribution, a load balancer or an API Gateway
    /// custom domain (by its name or DNS name), or a record of the zone
    #[arg(long, value_name = "TARGET")]
    alias: Option<String>,
    /// With --alias: the target's hosted zone id; TARGET is then its DNS name, taken as is
    #[arg(long, value_name = "ZONE_ID", requires = "alias")]
    alias_zone: Option<String>,
    /// Route 53 answers with the alias target only while it is healthy (true when given alone;
    /// load balancers default to true)
    #[arg(long, value_name = "BOOL", num_args = 0..=1, default_missing_value = "true")]
    evaluate_health: Option<bool>,
    /// After writing, wait until every Route 53 name server answers with the change
    #[arg(long)]
    wait: bool,
    /// With --create or --edit: do not ask for confirmation
    #[arg(short, long)]
    yes: bool,
}

pub fn run(config: &Config, args: Args) -> Result<ExitCode> {
    let writing = args.create.is_some() || args.edit.is_some();
    let write_only = !args.values.is_empty()
        || args.ttl.is_some()
        || args.alias.is_some()
        || args.evaluate_health.is_some()
        || args.wait
        || args.yes;
    if !writing && write_only {
        bail!("--value, --ttl, --alias, --evaluate-health, --wait and --yes go with --create or --edit");
    }
    let kind = args.kind.as_deref().map(|k| k.trim().to_uppercase());
    // Checked before anything is asked of AWS.
    if args.create.is_some() {
        match (&kind, args.values.is_empty(), args.alias.is_some()) {
            (_, true, false) => bail!("--create needs --value (repeatable) or --alias TARGET"),
            (None, false, _) => bail!("--create with --value needs --type ({})", type_names()),
            (Some(k), false, _) if !r53::TYPES.iter().any(|(t, _)| t == k) => {
                bail!("dcli creates records of type {}; not {k}", type_names())
            }
            _ => {}
        }
    }
    let ttl = args.ttl.as_deref().map(r53::check_ttl).transpose().map_err(|e| anyhow!(e))?;

    let mut parts: Vec<String> = [args.first.clone(), args.second.clone()].into_iter().flatten().collect();
    let env = if parts.first().is_some_and(|p| config.environments.contains_key(p)) {
        parts.remove(0)
    } else {
        current_env(config)?
    };
    if parts.len() > 1 {
        bail!("unexpected {}: dcli dns [ENV] [ZONE]", parts[1]);
    }
    if writing && parts.is_empty() {
        bail!("name the hosted zone: dcli dns [ENV] ZONE --{} NAME …", if args.create.is_some() { "create" } else { "edit" });
    }
    let aws = aws_for(config, &env)?;
    let zones = r53::list_zones(&aws)?;
    let Some(query) = parts.first() else { return list_zones(&zones, args.json) };
    let zone = r53::find_zone(&zones, query).map_err(|e| {
        let mut known: Vec<&str> = zones.iter().map(Zone::domain).collect();
        known.dedup();
        let known = known.join(", ");
        if e.starts_with("no hosted zone") { anyhow!("{e} in {env}. Zones: {known}") } else { anyhow!(e) }
    })?;
    let records = r53::all_records(&aws, &zone.id, MAX_RECORDS)?;
    let target = Target { env: &env, aws: &aws, zone, records: &records };
    if let Some(name) = &args.create {
        let name = r53::full_name(name, &zone.name).map_err(|e| anyhow!(e))?;
        let kind = kind.unwrap_or_else(|| "A".into());
        return create(&target, &name, &kind, &args, ttl);
    }
    if let Some(name) = &args.edit {
        let name = r53::full_name(name, &zone.name).map_err(|e| anyhow!(e))?;
        return edit(&target, &name, kind.as_deref(), &args, ttl);
    }
    show(zone, &records, kind.as_deref(), args.name.as_deref(), args.json)
}

fn type_names() -> String {
    r53::TYPES.iter().map(|(t, _)| *t).collect::<Vec<_>>().join(", ")
}

// ---- reading ------------------------------------------------------------------------

fn list_zones(zones: &[Zone], json: bool) -> Result<ExitCode> {
    if json {
        print_json(&Value::Array(
            zones
                .iter()
                .map(|z| {
                    json!({
                        "name": z.name, "id": z.id, "private": z.private, "records": z.records,
                        "comment": z.comment, "managedBy": z.managed_by,
                    })
                })
                .collect(),
        ));
        return Ok(ExitCode::SUCCESS);
    }
    if zones.is_empty() {
        println!("No hosted zones.");
        return Ok(ExitCode::SUCCESS);
    }
    let name_w = zones.iter().map(|z| z.name.chars().count()).max().unwrap_or(4).clamp(4, 50) + 2;
    for z in zones {
        let kind = format!("{:<8}", z.kind());
        let count = format!("{:>6} records", z.records);
        let mut extra = String::new();
        if let Some(c) = &z.comment {
            extra.push_str(&format!("  {c}"));
        }
        if let Some(m) = &z.managed_by {
            extra.push_str(&format!("  managed by {m}"));
        }
        println!(
            "{}{}{}  {}{}",
            paint(&format!("{:<name_w$}", z.name), "1"),
            paint(&kind, if z.private { "35" } else { "36" }),
            count,
            paint(&z.id, "90"),
            paint(&extra, "90")
        );
    }
    Ok(ExitCode::SUCCESS)
}

fn record_json(r: &Record, zone: &Zone) -> Value {
    json!({
        "name": r.name,
        "relative": r.relative(&zone.name),
        "type": r.kind,
        "ttl": r.ttl,
        "values": r.values,
        "alias": r.alias.as_ref().map(|a| json!({
            "zoneId": a.zone_id, "dnsName": a.dns_name, "evaluateHealth": a.evaluate_health,
            "target": r53::alias_kind(a, &zone.id),
        })),
        "setId": r.set_id,
        "routing": r.routing,
        "healthCheck": r.health_check,
        "trafficPolicy": r.traffic_policy,
    })
}

/// The zone's records, filtered by type and by a part of the name.
fn show(zone: &Zone, records: &[Record], kind: Option<&str>, name: Option<&str>, json: bool) -> Result<ExitCode> {
    let needle = name.map(|n| n.trim().trim_end_matches('.').to_lowercase());
    let shown: Vec<&Record> = records
        .iter()
        .filter(|r| kind.is_none_or(|k| r.kind == k))
        .filter(|r| needle.as_ref().is_none_or(|n| r.name.to_lowercase().contains(n.as_str())))
        .collect();
    if json {
        print_json(&json!({
            "zone": zone.name, "id": zone.id, "private": zone.private,
            "records": shown.iter().map(|r| record_json(r, zone)).collect::<Vec<_>>(),
        }));
        return Ok(ExitCode::SUCCESS);
    }
    let filtered = if shown.len() == records.len() { String::new() } else { format!(" · {} shown", shown.len()) };
    println!(
        "{}  {}",
        paint(&zone.name, "1"),
        paint(&format!("{} · {} · {} records{filtered}", zone.kind(), zone.id, records.len()), "90")
    );
    if shown.is_empty() {
        println!("No records match.");
        return Ok(ExitCode::SUCCESS);
    }
    let rel: Vec<String> = shown.iter().map(|r| r.relative(&zone.name)).collect();
    let name_w = rel.iter().map(|n| n.chars().count()).max().unwrap_or(4).clamp(4, 48) + 2;
    let routing: Vec<String> = shown
        .iter()
        .map(|r| match (&r.routing, &r.set_id) {
            // Latency records are often identified by their region.
            (Some(p), Some(id)) if !p.ends_with(id.as_str()) => format!("{p} · {id}"),
            (Some(p), _) => p.clone(),
            (None, Some(id)) => id.clone(),
            (None, None) => String::new(),
        })
        .collect();
    let routing_w = routing.iter().map(|s| s.chars().count()).max().unwrap_or(0);
    let routing_w = if routing_w == 0 { 0 } else { routing_w.min(36) + 2 };
    let mut header = format!("{:<name_w$}{:<7}{:<8}", "NAME", "TYPE", "TTL");
    if routing_w > 0 {
        header.push_str(&format!("{:<routing_w$}", "ROUTING"));
    }
    header.push_str("ANSWER");
    println!("{}", paint(&header, "90"));
    for ((r, name), routing) in shown.iter().zip(&rel).zip(&routing) {
        let ttl = r.ttl.map_or_else(|| "alias".to_string(), |t| t.to_string());
        let mut line = format!("{:<name_w$}{}{:<8}", name, paint(&format!("{:<7}", r.kind), "36"), ttl);
        if routing_w > 0 {
            line.push_str(&paint(&format!("{routing:<routing_w$}"), "35"));
        }
        let answer = match &r.alias {
            Some(a) => {
                let health = if a.evaluate_health { " · evaluates health" } else { "" };
                format!("→ {} {}", a.dns_name, paint(&format!("({}{health})", r53::alias_kind(a, &zone.id)), "90"))
            }
            None => r.values.join(", "),
        };
        line.push_str(&answer);
        if r.health_check.is_some() {
            line.push_str(&paint("  health check", "33"));
        }
        if r.traffic_policy.is_some() {
            line.push_str(&paint("  traffic policy", "33"));
        }
        println!("{line}");
    }
    Ok(ExitCode::SUCCESS)
}

// ---- writing ------------------------------------------------------------------------

struct Target<'a> {
    /// The dcli environment.
    env: &'a str,
    aws: &'a Aws,
    zone: &'a Zone,
    records: &'a [Record],
}

impl Target<'_> {
    fn place(&self) -> String {
        format!("in {} ({}, {}) · {}", self.zone.name, self.zone.kind(), self.zone.id, self.env)
    }
}

/// What a record answers now.
fn answer_of(r: &Record) -> Answer {
    match &r.alias {
        Some(a) => Answer::Alias(a.clone()),
        None => Answer::Values { ttl: r.ttl.unwrap_or(r53::DEFAULT_TTL), values: r.values.clone() },
    }
}

fn norm(s: &str) -> String {
    s.trim().trim_end_matches('.').to_lowercase()
}

/// The alias target `target` names among `cands`: by DNS name first (unique), then by label
/// (a distribution's or load balancer's DNS name, a custom domain), then a record of the zone
/// written relative to it. `own` is the record being written, which cannot point at itself.
fn pick<'a>(cands: &'a [Candidate], target: &str, zone: &Zone, kind: &str, own: &str) -> Result<&'a Candidate> {
    let wanted = norm(target);
    let usable: Vec<&Candidate> = cands.iter().filter(|c| !(c.source == "record" && norm(&c.alias.dns_name) == norm(own))).collect();
    let in_zone = r53::full_name(target, &zone.name).ok().map(|n| norm(&n));
    let steps: [&dyn Fn(&Candidate) -> bool; 3] = [
        &|c| norm(&c.alias.dns_name) == wanted,
        &|c| norm(&c.label) == wanted,
        &|c| c.source == "record" && in_zone.as_ref().is_some_and(|n| norm(&c.alias.dns_name) == *n),
    ];
    for matches in steps {
        let found: Vec<&Candidate> = usable.iter().copied().filter(|c| matches(c)).collect();
        let fitting: Vec<&Candidate> = found.iter().copied().filter(|c| c.kinds.iter().any(|k| k == kind)).collect();
        match (found.as_slice(), fitting.as_slice()) {
            ([], _) => continue,
            (_, [one]) => return Ok(one),
            ([one], []) => bail!("{} ({}) answers {} records, not {kind}", one.label, one.source, one.kinds.join(" and ")),
            (_, []) => bail!("{target} matches {} targets, none answering {kind}", found.len()),
            (_, many) => bail!(
                "{target} matches several targets; name one by its DNS name: {}",
                many.iter().map(|c| format!("{} ({})", c.alias.dns_name.trim_end_matches('.'), c.source)).collect::<Vec<_>>().join(", ")
            ),
        }
    }
    let first = wanted.split('.').next().unwrap_or_default();
    let close: Vec<String> = usable
        .iter()
        .filter(|c| !first.is_empty() && (c.label.contains(first) || c.alias.dns_name.contains(first)))
        .chain(usable.iter().filter(|c| c.source != "record"))
        .map(|c| format!("{} ({})", c.label, c.source))
        .fold(Vec::new(), |mut acc, s| {
            if !acc.contains(&s) && acc.len() < 8 {
                acc.push(s);
            }
            acc
        });
    let hint = if close.is_empty() { String::new() } else { format!(". Targets: {}", close.join(", ")) };
    bail!("no alias target {target}{hint}. For another AWS resource pass its DNS name with --alias-zone ZONE_ID")
}

/// The alias `--alias` (and `--alias-zone`, `--evaluate-health`) describes, and the candidate it
/// matched, if any.
fn resolve_alias(t: &Target, args: &Args, kind: &str, own: &str, evaluate_now: Option<bool>) -> Result<(Alias, Option<Candidate>)> {
    let target = args.alias.as_deref().unwrap_or_default();
    if let Some(zone_id) = &args.alias_zone {
        let dns = norm(target);
        if dns.is_empty() {
            bail!("--alias takes the target's DNS name");
        }
        let evaluate = args.evaluate_health.or(evaluate_now).unwrap_or(false);
        let alias = Alias { zone_id: zone_id.trim().to_string(), dns_name: format!("{dns}."), evaluate_health: evaluate };
        return Ok((alias, None));
    }
    let (cands, failed) = r53::candidates(t.aws, t.zone, t.records);
    for f in &failed {
        eprintln!("{}", paint(&format!("Could not list {f}"), "90"));
    }
    let found = pick(&cands, target, t.zone, kind, own)?.clone();
    let evaluate = args.evaluate_health.or(evaluate_now).unwrap_or(found.alias.evaluate_health);
    Ok((Alias { evaluate_health: evaluate, ..found.alias.clone() }, Some(found)))
}

/// What `--edit` makes `current` answer: what was given, the rest as it is.
fn edited_answer(current: &Record, zone: &Zone, values: &[String], ttl: Option<i64>, evaluate: Option<bool>) -> Result<Answer> {
    if !values.is_empty() {
        let values = r53::check_values(&current.kind, &current.name, &zone.name, values).map_err(|e| anyhow!(e))?;
        return Ok(Answer::Values { ttl: ttl.or(current.ttl).unwrap_or(r53::DEFAULT_TTL), values });
    }
    match (&current.alias, ttl, evaluate) {
        (Some(_), Some(_), _) => bail!("{} is an alias: aliases have no TTL (to answer with values, pass --value)", current.name),
        (None, Some(ttl), None) => Ok(Answer::Values { ttl, values: current.values.clone() }),
        (None, _, Some(_)) => bail!("--evaluate-health applies to aliases; {} answers with values", current.name),
        (Some(a), None, Some(evaluate)) => Ok(Answer::Alias(Alias { evaluate_health: evaluate, ..a.clone() })),
        _ => bail!("nothing to change: pass --value, --ttl, --alias or --evaluate-health"),
    }
}

/// Route 53's refusal, said for this command.
fn refused(e: AwsError, name: &str, kind: &str, editing: bool) -> anyhow::Error {
    let AwsError::Api { code, message, .. } = &e else { return e.into() };
    if code != "InvalidChangeBatch" {
        return e.into();
    }
    // An edit's delete that no longer matches also makes its create collide: the delete says why.
    if editing && (message.contains("do not match the current values") || message.contains("was not found")) {
        return anyhow!("{name} {kind} changed since it was read (someone edited it meanwhile); nothing changed: run it again");
    }
    if message.contains("already exists") {
        return anyhow!("{name} {kind} already exists: change it with --edit");
    }
    anyhow!("Route 53 refused it: {message}")
}

/// What `answer` makes a record answer, naming what an alias points at.
fn describe(answer: &Answer, zone: &Zone) -> String {
    match answer {
        Answer::Alias(a) => format!("{} ({})", answer.describe(), r53::alias_kind(a, &zone.id)),
        Answer::Values { .. } => answer.describe(),
    }
}

fn say(json: bool, line: &str) {
    // With --json, stdout carries only the JSON.
    if json {
        eprintln!("{line}");
    } else {
        println!("{line}");
    }
}

fn create(t: &Target, name: &str, kind: &str, args: &Args, ttl: Option<i64>) -> Result<ExitCode> {
    if let Some(r) = r53::find_record(t.records, name, kind, None) {
        bail!("{name} {kind} already exists ({}): change it with --edit", r.summary());
    }
    if let Some(r) = t.records.iter().find(|r| r.name.eq_ignore_ascii_case(name) && r.kind == kind) {
        bail!(
            "{name} has {kind} records under a routing policy ({}); dcli creates simple records only",
            r.routing.as_deref().unwrap_or("set identifiers")
        );
    }
    let (answer, warning) = if args.alias.is_some() {
        let (alias, found) = resolve_alias(t, args, kind, name, None)?;
        (Answer::Alias(alias), found.and_then(|c| c.warning(name)))
    } else {
        let values = r53::check_values(kind, name, &t.zone.name, &args.values).map_err(|e| anyhow!(e))?;
        (Answer::Values { ttl: ttl.unwrap_or(r53::DEFAULT_TTL), values }, None)
    };
    say(args.json, &format!("{} {} {}", paint("Create", "1"), paint(&format!("{name} {kind}"), "1"), describe(&answer, t.zone)));
    say(args.json, &paint(&t.place(), if is_production(t.env) { "31" } else { "90" }));
    if let Some(w) = &warning {
        eprintln!("{}", paint(&format!("⚠ {w}"), "33"));
    }
    if !confirm_write(t.env, &format!("Create {name} {kind}?"), args.yes)? {
        println!("Cancelled.");
        return Ok(ExitCode::FAILURE);
    }
    let change = r53::create_record(t.aws, &t.zone.id, name, kind, &answer).map_err(|e| refused(e, name, kind, false))?;
    finish(t, "create", name, kind, &answer, change, args)
}

fn edit(t: &Target, name: &str, kind: Option<&str>, args: &Args, ttl: Option<i64>) -> Result<ExitCode> {
    let at_name: Vec<&Record> = t.records.iter().filter(|r| r.name.eq_ignore_ascii_case(name)).collect();
    if at_name.is_empty() {
        bail!("{name} has no records in {}: create one with --create", t.zone.name);
    }
    let kind = match kind {
        Some(k) => k.to_string(),
        None => {
            let mut kinds: Vec<&str> = at_name.iter().map(|r| r.kind.as_str()).filter(|k| !["SOA", "NS"].contains(k)).collect();
            kinds.sort_unstable();
            kinds.dedup();
            match kinds.as_slice() {
                [one] => one.to_string(),
                [] => at_name[0].kind.clone(),
                many => bail!("{name} has records of types {}: pass --type", many.join(", ")),
            }
        }
    };
    let same: Vec<&Record> = at_name.iter().copied().filter(|r| r.kind == kind).collect();
    let current = match (same.as_slice(), args.set_id.as_deref()) {
        ([], _) => bail!("{name} has no {kind} record: create it with --create"),
        (_, Some(id)) => same
            .iter()
            .find(|r| r.set_id.as_deref() == Some(id))
            .copied()
            .ok_or_else(|| anyhow!("{name} {kind} has no set identifier {id}. Set identifiers: {}", set_ids(&same)))?,
        ([one], None) if one.set_id.is_none() => *one,
        (_, None) => bail!("{name} {kind} is under a routing policy: pass --set-id ({})", set_ids(&same)),
    };
    if let Some(why) = current.locked(&t.zone.name) {
        bail!("{why}; dcli does not edit {name} {kind}");
    }
    let (answer, warning) = if args.alias.is_some() {
        let evaluate_now = current.alias.as_ref().map(|a| a.evaluate_health);
        let (alias, found) = resolve_alias(t, args, &kind, name, evaluate_now)?;
        (Answer::Alias(alias), found.and_then(|c| c.warning(name)))
    } else {
        (edited_answer(current, t.zone, &args.values, ttl, args.evaluate_health)?, None)
    };
    let now = answer_of(current);
    let policy = match (&current.routing, &current.set_id) {
        (Some(p), Some(id)) => format!(" ({p} · {id})"),
        _ => String::new(),
    };
    if answer == now {
        say(args.json, &format!("{name} {kind}{policy} already answers {}; nothing to change.", describe(&now, t.zone)));
        return Ok(ExitCode::SUCCESS);
    }
    say(args.json, &format!("{} {}{policy}", paint("Edit", "1"), paint(&format!("{name} {kind}"), "1")));
    say(args.json, &format!("  {} {}", paint("now", "90"), describe(&now, t.zone)));
    say(args.json, &format!("  {} {}", paint("new", "90"), paint(&describe(&answer, t.zone), "33")));
    say(args.json, &paint(&t.place(), if is_production(t.env) { "31" } else { "90" }));
    if let Some(w) = &warning {
        eprintln!("{}", paint(&format!("⚠ {w}"), "33"));
    }
    if !confirm_write(t.env, &format!("Change {name} {kind}?"), args.yes)? {
        println!("Cancelled.");
        return Ok(ExitCode::FAILURE);
    }
    let change = r53::edit_record(t.aws, &t.zone.id, current, &answer).map_err(|e| refused(e, name, &kind, true))?;
    finish(t, "edit", name, &kind, &answer, change, args)
}

fn set_ids(records: &[&Record]) -> String {
    records.iter().filter_map(|r| r.set_id.as_deref()).collect::<Vec<_>>().join(", ")
}

/// Waits (with --wait) and reports a change.
fn finish(t: &Target, action: &str, name: &str, kind: &str, answer: &Answer, mut change: Change, args: &Args) -> Result<ExitCode> {
    if args.wait {
        change = wait_in_sync(t.aws, change)?;
    }
    if args.json {
        let answer = match answer {
            Answer::Values { ttl, values } => json!({ "ttl": ttl, "values": values }),
            Answer::Alias(a) => json!({ "alias": { "zoneId": a.zone_id, "dnsName": a.dns_name, "evaluateHealth": a.evaluate_health } }),
        };
        print_json(&json!({
            "action": action, "zone": t.zone.name, "zoneId": t.zone.id, "name": name, "type": kind,
            "answer": answer, "change": { "id": change.id, "status": change.status },
        }));
        return Ok(ExitCode::SUCCESS);
    }
    let done = if action == "create" { "Created" } else { "Changed" };
    println!("{} {done} {name} {kind} · change {} {}", paint("✓", "32"), change.id, change.status);
    if !change.in_sync() {
        println!("{}", paint("Route 53's name servers have it within about a minute (--wait waits for them).", "90"));
    }
    Ok(ExitCode::SUCCESS)
}

fn wait_in_sync(aws: &Aws, mut change: Change) -> Result<Change> {
    let start = Instant::now();
    while !change.in_sync() {
        if start.elapsed() > WAIT {
            eprintln!("{}", paint(&format!("Still {} after {} minutes; stopped waiting.", change.status, WAIT.as_secs() / 60), "33"));
            break;
        }
        std::thread::sleep(Duration::from_secs(2));
        change = r53::change_status(aws, &change.id)?;
    }
    Ok(change)
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser)]
    struct Cli {
        #[command(flatten)]
        args: Args,
    }

    fn parse(line: &str) -> std::result::Result<Args, clap::Error> {
        Cli::try_parse_from(std::iter::once("dns").chain(line.split_whitespace())).map(|c| c.args)
    }

    fn zone() -> Zone {
        Zone { id: "Z1".into(), name: "acme.dev.".into(), ..Zone::default() }
    }

    fn cand(source: &'static str, label: &str, dns: &str, kinds: &[&str]) -> Candidate {
        Candidate {
            source,
            label: label.into(),
            detail: String::new(),
            alias: Alias { zone_id: "Z".into(), dns_name: format!("{dns}."), evaluate_health: source == "load balancer" },
            kinds: kinds.iter().map(|k| k.to_string()).collect(),
            serves: Vec::new(),
        }
    }

    #[test]
    fn options_parse() {
        let a = parse("prod acme.dev --create api --type a --value 192.0.2.1 --value 192.0.2.2 --ttl 60").unwrap();
        assert_eq!((a.create.as_deref(), a.values.len(), a.ttl.as_deref()), (Some("api"), 2, Some("60")));
        let a = parse("acme.dev --edit app --set-id blue --alias web --evaluate-health").unwrap();
        assert_eq!(a.evaluate_health, Some(true));
        let a = parse("acme.dev --edit app --alias web --evaluate-health false").unwrap();
        assert_eq!(a.evaluate_health, Some(false));
        assert!(parse("acme.dev --create api --value 1.2.3.4 --alias x").is_err(), "values or an alias");
        assert!(parse("acme.dev --create api --edit api").is_err());
        assert!(parse("acme.dev --set-id blue").is_err(), "--set-id goes with --edit");
        assert!(parse("acme.dev --create api --name x").is_err(), "--name filters the listing");
    }

    #[test]
    fn alias_targets_are_found_by_dns_name_label_or_record() {
        let cands = vec![
            cand("CloudFront", "d1.cloudfront.net", "d1.cloudfront.net", &["A", "AAAA"]),
            cand("load balancer", "web-1.us-east-1.elb.amazonaws.com", "web-1.us-east-1.elb.amazonaws.com", &["A"]),
            cand("API Gateway", "api.acme.dev", "d-abc.execute-api.us-east-1.amazonaws.com", &["A"]),
            cand("record", "api.acme.dev", "api.acme.dev", &["A"]),
            cand("record", "www.acme.dev", "www.acme.dev", &["CNAME"]),
        ];
        let z = zone();
        assert_eq!(pick(&cands, "D1.cloudfront.net.", &z, "AAAA", "x.acme.dev.").unwrap().source, "CloudFront");
        assert!(pick(&cands, "web-1.us-east-1.elb.amazonaws.com", &z, "AAAA", "x.acme.dev.").unwrap_err().to_string().contains("answers A"));
        // The record api.acme.dev exists: its DNS name wins over the custom domain's label.
        assert_eq!(pick(&cands, "api.acme.dev", &z, "A", "x.acme.dev.").unwrap().source, "record");
        // Writing api.acme.dev itself, it cannot point at itself: the custom domain it is.
        assert_eq!(pick(&cands, "api.acme.dev", &z, "A", "api.acme.dev.").unwrap().source, "API Gateway");
        assert_eq!(pick(&cands, "www", &z, "CNAME", "x.acme.dev.").unwrap().label, "www.acme.dev", "records of the zone by relative name");
        let e = pick(&cands, "nothing", &z, "A", "x.acme.dev.").unwrap_err().to_string();
        assert!(e.contains("no alias target nothing") && e.contains("d1.cloudfront.net (CloudFront)") && e.contains("--alias-zone"), "{e}");
    }

    #[test]
    fn edits_keep_what_is_not_given() {
        let z = zone();
        let r = Record::sample("legacy.acme.dev.", "A", 3600, &["192.0.2.10", "192.0.2.11"]);
        assert_eq!(
            edited_answer(&r, &z, &[], Some(60), None).unwrap(),
            Answer::Values { ttl: 60, values: vec!["192.0.2.10".into(), "192.0.2.11".into()] }
        );
        assert_eq!(edited_answer(&r, &z, &["192.0.2.12".into()], None, None).unwrap(), Answer::Values { ttl: 3600, values: vec!["192.0.2.12".into()] });
        assert!(edited_answer(&r, &z, &["nope".into()], None, None).unwrap_err().to_string().contains("not an IPv4"));
        assert!(edited_answer(&r, &z, &[], None, Some(true)).unwrap_err().to_string().contains("aliases"));
        assert!(edited_answer(&r, &z, &[], None, None).unwrap_err().to_string().contains("nothing to change"));
        let mut alias = r.clone();
        alias.ttl = None;
        alias.values.clear();
        alias.alias = Some(Alias { zone_id: "Z".into(), dns_name: "d1.cloudfront.net.".into(), evaluate_health: false });
        assert!(edited_answer(&alias, &z, &[], Some(60), None).unwrap_err().to_string().contains("no TTL"));
        assert!(matches!(edited_answer(&alias, &z, &[], None, Some(true)).unwrap(), Answer::Alias(a) if a.evaluate_health));
        assert_eq!(answer_of(&r), Answer::Values { ttl: 3600, values: vec!["192.0.2.10".into(), "192.0.2.11".into()] });
    }

    #[test]
    fn route53_refusals_are_said_plainly() {
        let api = |m: &str| AwsError::Api { status: 400, code: "InvalidChangeBatch".into(), message: m.into() };
        let e = refused(api("Tried to create resource record set [name='a.acme.dev.', type='A'] but it already exists"), "a.acme.dev.", "A", false);
        assert!(e.to_string().contains("--edit"));
        let e = refused(
            api("Tried to delete resource record set [name='a.acme.dev.', type='A'] but the values provided do not match the current values · Tried to create resource record set [name='a.acme.dev.', type='A'] but it already exists"),
            "a.acme.dev.",
            "A",
            true,
        );
        assert!(e.to_string().contains("changed since it was read"));
        let e = refused(api("RRSet of type CNAME with DNS name a.acme.dev. is not permitted"), "a.acme.dev.", "CNAME", false);
        assert!(e.to_string().starts_with("Route 53 refused it: RRSet of type CNAME"));
    }
}
