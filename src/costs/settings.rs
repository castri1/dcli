//! `~/.deployment-cli/costs.json`: which profile reads Cost Explorer (with AWS Organizations,
//! one in the management account reads every account), whether to read it once a day, and
//! how many requests were made each month (AWS bills each one).
//!
//! Every change goes through [`update`]: one at a time in the process (readings count their
//! requests from background threads), and never over a file that cannot be read back, which
//! would silently drop the reader profile and the month's count.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Mutex;

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
    /// The AWS account each profile signed in to when last asked (STS, or `sso_account_id`):
    /// which environments share an account.
    pub accounts: BTreeMap<String, String>,
    /// When each environment's costs were last read once a day (Unix seconds), whether it
    /// worked or not: a failing reading is not tried again at every visit.
    pub daily_attempts: BTreeMap<String, f64>,
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

/// The settings, or the defaults when there are none or they cannot be read (see [`check`]).
pub fn load() -> Settings {
    check().unwrap_or_default()
}

/// The settings; an error when the file exists but cannot be read or is not valid: readings
/// stop there rather than fall back to another reader without saying so.
pub fn check() -> std::io::Result<Settings> {
    let path = path().ok_or_else(|| std::io::Error::other("cannot find the home directory"))?;
    match std::fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str(&text)
            .map_err(|e| std::io::Error::other(format!("{} is not valid ({e}): fix or delete it", path.display()))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Settings::default()),
        Err(e) => Err(std::io::Error::other(format!("cannot read {}: {e}", path.display()))),
    }
}

/// Whether the costs were ever set up (the settings file exists).
pub fn exists() -> bool {
    path().is_some_and(|p| p.exists())
}

/// One change at a time: read, changed, written back.
static LOCK: Mutex<()> = Mutex::new(());

/// Changes the settings with `change` and keeps them; refused when the file on disk cannot
/// be read back (it is left for the user to fix or delete).
pub fn update(change: impl FnOnce(&mut Settings)) -> std::io::Result<Settings> {
    let _one = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut s = check()?;
    change(&mut s);
    let path = path().ok_or_else(|| std::io::Error::other("cannot find the home directory"))?;
    crate::platform::write_private(&path, (serde_json::to_string_pretty(&s).unwrap_or_default() + "\n").as_bytes())?;
    Ok(s)
}

/// `2026-10`, the month a request counts in.
pub fn month(now: f64) -> String {
    let (y, m, ..) = crate::util::utc_parts(now as i64);
    format!("{y:04}-{m:02}")
}

/// Counts `n` more Cost Explorer requests this month.
pub fn count_requests(n: u64) {
    let _ = update(|s| *s.requests.entry(month(crate::util::now_secs())).or_default() += n);
}

/// The Cost Explorer requests made this month.
pub fn requests_this_month() -> u64 {
    load().requests.get(&month(crate::util::now_secs())).copied().unwrap_or(0)
}

/// Remembers that `profile` signs in to `account`.
pub fn remember_account(profile: &str, account: &str) {
    if load().accounts.get(profile).map(String::as_str) != Some(account) {
        let _ = update(|s| {
            s.accounts.insert(profile.to_string(), account.to_string());
        });
    }
}

/// How long a failed once-a-day reading waits before the next one.
pub const DAILY_RETRY_SECS: f64 = 6.0 * 3600.0;

/// Whether `env`'s once-a-day reading is due at `now`, its costs having been read at
/// `read_at`: in daily mode, more than a day after the last reading, and not attempted in
/// the last hours (a reading that fails is not retried at every visit).
pub fn daily_due(env: &str, read_at: Option<f64>, now: f64) -> bool {
    due(&load(), env, read_at, now)
}

fn due(s: &Settings, env: &str, read_at: Option<f64>, now: f64) -> bool {
    let stale = read_at.is_none_or(|t| now - t >= 86_400.0);
    let tried = s.daily_attempts.get(env).is_some_and(|t| now - t < DAILY_RETRY_SECS && *t <= now);
    s.refresh == Refresh::Daily && stale && !tried
}

/// Notes that `env`'s once-a-day reading starts now.
pub fn daily_attempted(env: &str, now: f64) {
    let _ = update(|s| {
        s.daily_attempts.insert(env.to_string(), now);
    });
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

    #[test]
    fn daily_readings_wait_after_an_attempt() {
        let now = 1_790_000_000.0;
        let mut s = Settings { refresh: Refresh::Daily, ..Default::default() };
        assert!(due(&s, "dev", None, now), "never read");
        assert!(due(&s, "dev", Some(now - 90_000.0), now), "older than a day");
        assert!(!due(&s, "dev", Some(now - 3600.0), now), "read an hour ago");
        s.daily_attempts.insert("dev".into(), now - 600.0);
        assert!(!due(&s, "dev", None, now), "tried ten minutes ago (and failed)");
        assert!(due(&s, "dev", None, now + DAILY_RETRY_SECS), "tried long enough ago");
        assert!(due(&s, "qa", None, now), "another environment");
        s.refresh = Refresh::Manual;
        assert!(!due(&s, "qa", None, now));
    }
}
