//! What the AWS bill says, from Cost Explorer, to put real money on what dcli finds.
//!
//! Cost Explorer bills every API request, so nothing reads it on its own: the Costs module
//! and `dcli costs` read it when asked (or once a day, when the user turned that on), count
//! the calls, and keep what they read on disk ([`save`], [`cached`]) for the rest of dcli.
//! The Savings module prices its findings with [`Snapshot::rate`], the account's own
//! effective rate for a usage type (discounts, Savings Plans and region included), so dcli
//! still stores no prices.
//!
//! Costs are amortized: Savings Plans and reservations are spread over the usage they cover,
//! so each service shows what it really costs. With AWS Organizations, a profile in the
//! management account reads every account at once; an environment's costs are its account's.

#![allow(dead_code)] // used by the costs and savings work

pub mod analysis;
pub mod attribution;
pub mod explorer;
pub mod ledger;
pub mod services;
pub mod settings;

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// The costs of one environment's AWS account, as last read.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Snapshot {
    /// The environment's AWS account id.
    pub account: String,
    /// Unix seconds when it was read.
    pub read_at: f64,
    /// `USD`.
    pub currency: String,
    /// This month so far, all of last month, and the forecast for the whole of this month.
    pub month_to_date: f64,
    pub last_month: f64,
    pub forecast: Option<f64>,
    /// Per service (Cost Explorer's names: `AWS Lambda`, `AmazonCloudWatch`…).
    pub services: Vec<ServiceCost>,
    /// The usage types of the last 30 days with how much was used: effective rates.
    pub usage: Vec<UsageCost>,
    /// How much of each service's usage Savings Plans and reservations cover.
    pub coverage: Vec<Coverage>,
    pub commitments: Vec<Commitment>,
    pub recommendation: Option<Recommendation>,
    /// Last month up to the same day: what this month so far compares with.
    pub last_month_to_date: f64,
    /// Who read it: `the management account (profile billing)`, `the environment's profile`.
    pub reader: String,
    /// Parts that could not be read, and why (no permission for coverage, say).
    pub missing: Vec<String>,
    /// Cost Explorer requests the reading took.
    pub requests: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ServiceCost {
    pub service: String,
    pub month_to_date: f64,
    pub last_month: f64,
    /// Last month up to the same day.
    pub last_month_to_date: f64,
    /// (day start, Unix seconds; cost), oldest first: the last 30 days or more.
    pub daily: Vec<(f64, f64)>,
}

/// A usage type over the last 30 days: what it cost and how much of it was used, in the
/// unit Cost Explorer reports (`Lambda-GB-Second`, `GB-Mo`, `Hrs`, `Requests`…).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct UsageCost {
    pub service: String,
    /// With its region prefix where Cost Explorer gives one (`USW2-Lambda-GB-Second`).
    pub usage_type: String,
    pub cost: f64,
    pub quantity: f64,
    pub unit: String,
}

impl UsageCost {
    /// What one unit cost, on average.
    pub fn rate(&self) -> Option<f64> {
        (self.quantity > 0.0 && self.cost.is_finite()).then(|| self.cost / self.quantity)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Coverage {
    pub service: String,
    /// The share of its usage commitments cover, 0 to 1.
    pub covered: f64,
    /// What its usage outside any commitment cost over the last 30 days.
    pub on_demand_cost: f64,
    /// `Savings Plans` or `reservations`.
    pub by: String,
}

/// A Savings Plan or a reservation.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Commitment {
    /// `Compute Savings Plan`, `RDS reserved instances`…
    pub kind: String,
    pub id: String,
    /// The services whose usage it can cover (Cost Explorer's names).
    pub services: Vec<String>,
    /// Committed spend per hour.
    pub hourly: f64,
    /// When it ends (Unix seconds).
    pub end: Option<f64>,
    /// The share of it used over the last 30 days, 0 to 1.
    pub utilization: Option<f64>,
    /// What went unused over the last 30 days.
    pub unused_cost: Option<f64>,
    /// `organization` when it was bought in the management account and is shared.
    pub scope: String,
}

/// What AWS recommends buying (Savings Plans purchase recommendation), with AWS's estimate.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Recommendation {
    pub kind: String,
    pub hourly_commitment: f64,
    pub estimated_monthly_savings: f64,
    pub lookback_days: u32,
    /// `ONE_YEAR`, `THREE_YEARS`.
    pub term: String,
    /// `NO_UPFRONT`…
    pub payment: String,
}

impl Snapshot {
    /// The effective rate of `service`'s usage types named `usage` (in any region: Cost
    /// Explorer prefixes it, `USE1-Lambda-GB-Second`), as (cost per unit, unit), weighted by
    /// how much each region used.
    pub fn rate(&self, service: &str, usage: &str) -> Option<(f64, String)> {
        let matching: Vec<&UsageCost> = self.usage.iter().filter(|u| u.service == service && usage_is(&u.usage_type, usage)).collect();
        let (cost, quantity) = matching.iter().fold((0.0, 0.0), |(c, q), u| (c + u.cost, q + u.quantity));
        let unit = matching.first()?.unit.clone();
        (quantity > 0.0).then(|| (cost / quantity, unit))
    }

    /// The share of `service`'s usage commitments cover (0 when unknown).
    pub fn covered(&self, service: &str) -> f64 {
        self.coverage.iter().find(|c| c.service == service).map_or(0.0, |c| c.covered)
    }

    /// When the first commitment that can cover `service` ends.
    pub fn renewal(&self, service: &str) -> Option<f64> {
        self.commitments.iter().filter(|c| c.services.iter().any(|s| s == service)).filter_map(|c| c.end).reduce(f64::min)
    }
}

/// Whether `usage_type` is `name` in some region: `name` itself or `<REGION>-name`.
pub fn usage_is(usage_type: &str, name: &str) -> bool {
    usage_type == name || usage_type.strip_suffix(name).is_some_and(|prefix| prefix.ends_with('-') && !prefix[..prefix.len() - 1].contains('-'))
}

/// `~/.deployment-cli/cache/costs-<env>.json`.
pub fn cache_path(env: &str) -> Option<PathBuf> {
    let name: String = env.chars().map(|c| if c.is_ascii_alphanumeric() || matches!(c, '-' | '_') { c } else { '_' }).collect();
    Some(crate::util::home_dir()?.join(".deployment-cli").join("cache").join(format!("costs-{name}.json")))
}

/// What was last read for `env`, if anything.
pub fn cached(env: &str) -> Option<Snapshot> {
    let text = std::fs::read_to_string(cache_path(env)?).ok()?;
    serde_json::from_str(&text).ok()
}

/// Keeps `snapshot` as `env`'s costs (a private file: costs are not for every local user).
pub fn save(env: &str, snapshot: &Snapshot) -> std::io::Result<()> {
    let path = cache_path(env).ok_or_else(|| std::io::Error::other("cannot find the home directory"))?;
    crate::platform::write_private(&path, serde_json::to_string(snapshot).unwrap_or_default().as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rates_come_from_every_region_of_a_usage_type() {
        let usage = |usage_type: &str, cost: f64, quantity: f64| UsageCost {
            service: "AWS Lambda".into(),
            usage_type: usage_type.into(),
            cost,
            quantity,
            unit: "Lambda-GB-Second".into(),
        };
        let s = Snapshot {
            usage: vec![
                usage("Lambda-GB-Second", 10.0, 600_000.0),
                usage("USW2-Lambda-GB-Second", 5.0, 400_000.0),
                usage("USW2-Lambda-GB-Second-ARM", 8.0, 600_000.0),
            ],
            ..Default::default()
        };
        let (rate, unit) = s.rate("AWS Lambda", "Lambda-GB-Second").unwrap();
        assert!((rate - 15.0 / 1_000_000.0).abs() < 1e-12);
        assert_eq!(unit, "Lambda-GB-Second");
        assert!(s.rate("AWS Lambda", "Request").is_none());
        assert!(s.rate("Amazon DynamoDB", "Lambda-GB-Second").is_none());
        assert!(usage_is("APN1-TimedStorage-ByteHrs", "TimedStorage-ByteHrs"));
        assert!(!usage_is("USE1-Other-TimedStorage-ByteHrs", "TimedStorage-ByteHrs"));
    }
}
