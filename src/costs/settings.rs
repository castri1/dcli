//! `~/.deployment-cli/costs.json`: which profile reads Cost Explorer (with AWS Organizations,
//! one in the management account reads every account), whether to read it once a day, and
//! how many requests were made each month (AWS bills each one).

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Refresh {
    /// Only when asked (`r`, `dcli costs --refresh`).
    #[default]
    Manual,
    /// When the costs read are older than a day and the Costs module or `dcli costs` is used.
    Daily,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Settings {
    /// The profile that reads Cost Explorer for every environment; `None`: each environment's own.
    pub profile: Option<String>,
    pub refresh: Refresh,
    /// Cost Explorer requests per month (`2026-10`).
    pub requests: BTreeMap<String, u64>,
}

pub fn path() -> Option<PathBuf> {
    Some(crate::util::home_dir()?.join(".deployment-cli").join("costs.json"))
}

pub fn load() -> Settings {
    path().and_then(|p| std::fs::read_to_string(p).ok()).and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
}

pub fn save(settings: &Settings) -> std::io::Result<()> {
    let path = path().ok_or_else(|| std::io::Error::other("cannot find the home directory"))?;
    crate::platform::write_private(&path, (serde_json::to_string_pretty(settings).unwrap_or_default() + "\n").as_bytes())
}

/// `2026-10`, the month a request counts in.
pub fn month(now: f64) -> String {
    let (y, m, ..) = crate::util::utc_parts(now as i64);
    format!("{y:04}-{m:02}")
}

/// Counts `n` more Cost Explorer requests this month.
pub fn count_requests(n: u64) {
    let mut s = load();
    *s.requests.entry(month(crate::util::now_secs())).or_default() += n;
    let _ = save(&s);
}

/// The Cost Explorer requests made this month.
pub fn requests_this_month() -> u64 {
    load().requests.get(&month(crate::util::now_secs())).copied().unwrap_or(0)
}
