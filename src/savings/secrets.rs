//! Secrets Manager: secrets nobody has read in a long time (each is billed every month).

use super::{Finding, Impact, Scan, billing};
use crate::aws::secrets::{self, Secret};
use crate::aws::{self, Aws};
use crate::util::{fmt_utc, now_secs};

const MODULE: &str = "Secrets";
/// A secret unread for this many days (or the scan window, if longer) is listed.
const IDLE_DAYS: f64 = 90.0;

pub fn check(scan: &Scan) -> aws::Result<Vec<Finding>> {
    let idle = IDLE_DAYS.max(f64::from(scan.days));
    Ok(all_secrets(scan.aws)?.iter().filter_map(|s| unused(s, idle, now_secs())).collect())
}

fn all_secrets(aws: &Aws) -> aws::Result<Vec<Secret>> {
    let mut all = Vec::new();
    let mut token: Option<String> = None;
    for _ in 0..100 {
        let (page, next) = secrets::list_secrets(aws, token.as_deref())?;
        all.extend(page);
        match next {
            Some(t) => token = Some(t),
            None => break,
        }
    }
    Ok(all)
}

fn unused(s: &Secret, idle_days: f64, now: f64) -> Option<Finding> {
    // Secrets another service manages (an RDS master password…) are its business.
    if s.owning_service.is_some() {
        return None;
    }
    let since = s.last_accessed_date.or(s.created_date)?;
    let days = ((now - since) / 86_400.0).floor();
    if days < idle_days {
        return None;
    }
    let summary = match s.last_accessed_date {
        Some(_) => format!("not read in {days:.0} days"),
        None => format!("never read (created {days:.0} days ago)"),
    };
    let mut finding = Finding::new(
        MODULE,
        &s.name,
        &s.name,
        summary,
        "Delete it if nothing needs it (it can be restored during the recovery window)",
        "1 secret a month",
        Impact::Low,
    )
    .kind("secrets-unused")
    .score(days / 3650.0)
    .billed(billing::SECRETS, 1.0)
    .caveat("Secrets Manager keeps the last read to the day: a secret read once a year looks unused.");
    if let Some(at) = s.last_accessed_date {
        finding = finding.evidence(format!("last read {}", &fmt_utc(at)[..10]));
    }
    finding = finding.evidence(format!("rotation {}", if s.rotation_enabled == Some(true) { "on" } else { "off" }));
    Some(finding)
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: f64 = 1_790_000_000.0;

    fn secret(accessed_days_ago: Option<f64>, created_days_ago: f64) -> Secret {
        Secret {
            name: "legacy/partner-api".into(),
            last_accessed_date: accessed_days_ago.map(|d| NOW - d * 86_400.0),
            created_date: Some(NOW - created_days_ago * 86_400.0),
            ..Default::default()
        }
    }

    #[test]
    fn secrets_unread_for_months() {
        let f = unused(&secret(Some(140.0), 400.0), 90.0, NOW).unwrap();
        assert_eq!(f.summary, "not read in 140 days");
        assert_eq!(f.target, "legacy/partner-api");
        assert_eq!(unused(&secret(None, 120.0), 90.0, NOW).unwrap().summary, "never read (created 120 days ago)");
        assert!(unused(&secret(Some(1.0), 400.0), 90.0, NOW).is_none());
        assert!(unused(&secret(None, 40.0), 90.0, NOW).is_none());
        let mut managed = secret(Some(300.0), 400.0);
        managed.owning_service = Some("rds".into());
        assert!(unused(&managed, 90.0, NOW).is_none());
    }
}
