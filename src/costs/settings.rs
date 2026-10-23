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
    /// The AWS account each profile signs in to, learnt once (STS, or `sso_account_id`).
    pub accounts: BTreeMap<String, String>,
}

impl Settings {
    /// The profile that reads Cost Explorer for everyone, when one is set.
    pub fn reader(&self) -> Option<&str> {
        self.profile.as_deref().filter(|p| !p.is_empty())
    }
}

pub fn path() -> Option<PathBuf> {
    Some(crate::util::home_dir()?.join(".deployment-cli").join("costs.json"))
}

pub fn load() -> Settings {
    path().and_then(|p| std::fs::read_to_string(p).ok()).and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
}

/// Whether the costs were ever set up (the settings file exists).
pub fn exists() -> bool {
    path().is_some_and(|p| p.exists())
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

/// Remembers that `profile` signs in to `account`.
pub fn remember_account(profile: &str, account: &str) {
    let mut s = load();
    if s.accounts.get(profile).map(String::as_str) != Some(account) {
        s.accounts.insert(profile.to_string(), account.to_string());
        let _ = save(&s);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn months_and_readers() {
        // 2026-10-31T23:59:59Z and 2026-11-01T00:00:00Z
        assert_eq!(month(1_793_491_199.0), "2026-10");
        assert_eq!(month(1_793_491_200.0), "2026-11");
        let mut s = Settings::default();
        assert_eq!(s.reader(), None);
        s.profile = Some(String::new());
        assert_eq!(s.reader(), None);
        s.profile = Some("billing".into());
        assert_eq!(s.reader(), Some("billing"));
        let text = serde_json::to_string(&s).unwrap();
        assert!(text.contains("\"refresh\":\"manual\""), "{text}");
        assert_eq!(serde_json::from_str::<Settings>("{}").unwrap(), Settings::default());
    }
}
