//! What the MCP server's Logs Insights analyses scanned this month, in
//! `~/.deployment-cli/mcp-usage.json`: `--insights-gb-month` caps it. An analysis reserves its
//! estimate before it runs ([`reserve`]) and settles on what it scanned ([`settle`]), so two at
//! once, even from two servers, cannot pass the cap together.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct Usage {
    /// `2026-10`: the month counted.
    month: String,
    insights_bytes: f64,
}

/// `~/.deployment-cli/mcp-usage.json`
pub fn path() -> Option<PathBuf> {
    Some(crate::util::home_dir()?.join(".deployment-cli").join("mcp-usage.json"))
}

/// The file's figures, or an error when it exists but cannot be read (it is then never written
/// over: the user fixes or deletes it).
fn load(path: &Path) -> std::io::Result<Usage> {
    match std::fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text).map_err(|e| std::io::Error::other(format!("{} is not valid ({e}): fix or delete it", path.display()))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Usage::default()),
        Err(e) => Err(e),
    }
}

/// Bytes scanned in the month of `now` (0 for another month).
pub fn scanned(path: &Path, now: f64) -> std::io::Result<f64> {
    let u = load(path)?;
    let bytes = if u.insights_bytes.is_finite() { u.insights_bytes.max(0.0) } else { 0.0 };
    Ok(if u.month == crate::costs::settings::month(now) { bytes } else { 0.0 })
}

/// A reservation: the bytes counted, and the month they were counted in.
#[derive(Debug, Clone, PartialEq)]
pub struct Reserved {
    pub month: String,
    pub bytes: f64,
}

/// Counts `bytes` against the month of `now` when they fit in `cap`: `Ok(Ok(reservation))`,
/// or `Ok(Err(left))` with what is left when they do not. A cap of 0 holds nothing.
pub fn reserve(path: &Path, now: f64, bytes: f64, cap: f64) -> std::io::Result<Result<Reserved, f64>> {
    let _one = crate::platform::lock(path)?;
    let used = scanned(path, now)?;
    if cap <= 0.0 || used + bytes > cap {
        return Ok(Err((cap - used).max(0.0)));
    }
    let month = crate::costs::settings::month(now);
    write(path, &month, used + bytes)?;
    Ok(Ok(Reserved { month, bytes }))
}

/// Replaces a reservation with what the analysis scanned, in the month it was counted in. When
/// the file has moved on to a newer month meanwhile, that month's count is left as it is but for
/// what the analysis scanned beyond its reservation (it was paid then too).
pub fn settle(path: &Path, reserved: &Reserved, scanned_bytes: f64) -> std::io::Result<()> {
    let _one = crate::platform::lock(path)?;
    let u = load(path)?;
    let used = if u.insights_bytes.is_finite() { u.insights_bytes.max(0.0) } else { 0.0 };
    let scanned_bytes = if scanned_bytes.is_finite() { scanned_bytes.max(0.0) } else { reserved.bytes };
    if u.month == reserved.month {
        write(path, &reserved.month, (used - reserved.bytes + scanned_bytes).max(0.0))
    } else if u.month > reserved.month {
        write(path, &u.month, used + (scanned_bytes - reserved.bytes).max(0.0))
    } else {
        // The count was lost (the file deleted or older): it holds what this analysis scanned.
        write(path, &reserved.month, scanned_bytes)
    }
}

fn write(path: &Path, month: &str, bytes: f64) -> std::io::Result<()> {
    let u = Usage { month: month.to_string(), insights_bytes: bytes };
    crate::platform::write_private(path, serde_json::to_string(&u).unwrap_or_default().as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reservations_stay_within_the_month_and_settle() {
        let dir = std::env::temp_dir().join(format!("dcli-mcp-usage-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("mcp-usage.json");
        let oct = 1_793_000_000.0; // 2026-10
        assert_eq!(crate::costs::settings::month(oct), "2026-10");
        assert_eq!(scanned(&path, oct).unwrap(), 0.0);
        let first = reserve(&path, oct, 600.0, 1000.0).unwrap().unwrap();
        assert_eq!(first, Reserved { month: "2026-10".into(), bytes: 600.0 });
        assert_eq!(reserve(&path, oct, 500.0, 1000.0).unwrap(), Err(400.0), "what is left");
        settle(&path, &first, 450.0).unwrap();
        assert_eq!(scanned(&path, oct).unwrap(), 450.0);
        // A cap of 0 holds nothing, not even 0 bytes.
        assert_eq!(reserve(&path, oct, 0.0, 0.0).unwrap(), Err(0.0));
        // A new month starts from nothing.
        let nov = oct + 31.0 * 86_400.0;
        assert_eq!(scanned(&path, nov).unwrap(), 0.0);
        assert_eq!(reserve(&path, nov, 900.0, 1000.0).unwrap().unwrap().month, "2026-11");
        // A damaged file stops the count and is left as it is.
        std::fs::write(&path, "not json").unwrap();
        assert!(reserve(&path, nov, 1.0, 1000.0).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "not json");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn settling_across_the_month_leaves_the_new_month_alone() {
        let dir = std::env::temp_dir().join(format!("dcli-mcp-usage-month-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("mcp-usage.json");
        let oct = 1_793_000_000.0; // 2026-10
        let nov = oct + 31.0 * 86_400.0;
        // An October analysis is still running when a November one reserves.
        let a = reserve(&path, oct, 500.0, 1000.0).unwrap().unwrap();
        let b = reserve(&path, nov, 700.0, 1000.0).unwrap().unwrap();
        assert_eq!(scanned(&path, nov).unwrap(), 700.0);
        // A scanned less than it reserved: November's count is untouched.
        settle(&path, &a, 300.0).unwrap();
        assert_eq!(scanned(&path, nov).unwrap(), 700.0);
        // Had it scanned more, only what went beyond its reservation is added.
        let c = Reserved { month: "2026-10".into(), bytes: 100.0 };
        settle(&path, &c, 150.0).unwrap();
        assert_eq!(scanned(&path, nov).unwrap(), 750.0);
        settle(&path, &b, 680.0).unwrap();
        assert_eq!(scanned(&path, nov).unwrap(), 730.0);
        // A lost count holds what was scanned.
        std::fs::remove_file(&path).unwrap();
        settle(&path, &b, 680.0).unwrap();
        assert_eq!(scanned(&path, nov).unwrap(), 680.0);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
