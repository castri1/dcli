//! Which modules an environment uses. A module whose service was listed and has nothing there
//! (no ECS clusters, no Redshift) leaves the header, Tab and the digits; `:` still opens it.
//! The counts come from the `:` catalog and are kept per environment in
//! `~/.deployment-cli/cache/modules-<env>.json`, so the next start shows the right modules at
//! once; they are listed again after a day. `defaults.modules` in the config, when set, names
//! the modules to show instead (in its order).

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::module::Env;

/// Counts older than this are listed again at start.
const REFRESH_SECS: f64 = 24.0 * 3600.0;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Usage {
    pub profile: String,
    pub region: String,
    /// Unix seconds.
    pub at: f64,
    /// Resources per module title, for the services that could be listed.
    pub counts: BTreeMap<String, usize>,
}

impl Usage {
    pub fn new(env: &Env, counts: &[(&str, usize)], now: f64) -> Usage {
        Usage {
            profile: env.profile.clone(),
            region: env.region.clone(),
            at: now,
            counts: counts.iter().map(|(m, n)| (m.to_string(), *n)).collect(),
        }
    }

    /// The counts saved for `env`, unless its profile or region changed since.
    pub fn load(env: &Env) -> Option<Usage> {
        let text = std::fs::read_to_string(path(&env.name)?).ok()?;
        let usage: Usage = serde_json::from_str(&text).ok()?;
        (usage.profile == env.profile && usage.region == env.region).then_some(usage)
    }

    pub fn save(&self, env: &str) {
        if let (Some(path), Ok(json)) = (path(env), serde_json::to_vec_pretty(self)) {
            let _ = crate::platform::write_private(&path, &json);
        }
    }

    pub fn stale(&self, now: f64) -> bool {
        now - self.at > REFRESH_SECS || self.at > now
    }

    /// True when the module's service was listed and has nothing.
    pub fn empty(&self, title: &str) -> bool {
        self.counts.get(title) == Some(&0)
    }

    /// The modules (titles, in `order`) that are empty here.
    pub fn hidden<'a>(&self, order: &[&'a str]) -> Vec<&'a str> {
        order.iter().copied().filter(|t| self.empty(t)).collect()
    }
}

fn path(env: &str) -> Option<PathBuf> {
    let name = format!("modules-{}.json", crate::costs::file_name(env));
    Some(crate::util::home_dir()?.join(".deployment-cli").join("cache").join(name))
}

/// The modules to show (indexes into `titles`, in display order): those named in `pinned`
/// when it names any, else every module except the empty ones. Never empty.
pub fn shown(titles: &[&str], pinned: &[String], usage: Option<&Usage>) -> Vec<usize> {
    let mut list: Vec<usize> = Vec::new();
    if pinned.is_empty() {
        list = (0..titles.len()).filter(|&i| usage.is_none_or(|u| !u.empty(titles[i]))).collect();
    } else {
        for name in pinned {
            let found = titles.iter().position(|t| t.eq_ignore_ascii_case(name.trim()));
            if let Some(i) = found.filter(|i| !list.contains(i)) {
                list.push(i);
            }
        }
    }
    if list.is_empty() { (0..titles.len()).collect() } else { list }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TITLES: [&str; 4] = ["Pipelines", "Lambda", "ECS", "EKS"];

    fn usage(counts: &[(&str, usize)]) -> Usage {
        Usage { counts: counts.iter().map(|(m, n)| (m.to_string(), *n)).collect(), ..Usage::default() }
    }

    #[test]
    fn hides_only_modules_listed_empty() {
        // EKS could not be listed (no permission): it stays.
        let u = usage(&[("Lambda", 3), ("ECS", 0)]);
        assert_eq!(shown(&TITLES, &[], Some(&u)), vec![0, 1, 3]);
        assert_eq!(u.hidden(&TITLES), vec!["ECS"]);
        assert_eq!(shown(&TITLES, &[], None), vec![0, 1, 2, 3]);
    }

    #[test]
    fn pinned_modules_win_in_their_order() {
        let u = usage(&[("EKS", 0)]);
        let pinned = vec!["eks".to_string(), "Pipelines".into(), "Nope".into(), "EKS".into()];
        assert_eq!(shown(&TITLES, &pinned, Some(&u)), vec![3, 0]);
        // Nothing valid pinned: everything.
        assert_eq!(shown(&TITLES, &["Nope".to_string()], None), vec![0, 1, 2, 3]);
    }

    #[test]
    fn stale_after_a_day_or_from_the_future() {
        let u = Usage { at: 1000.0, ..Usage::default() };
        assert!(!u.stale(1000.0 + 3600.0));
        assert!(u.stale(1000.0 + REFRESH_SECS + 1.0));
        assert!(u.stale(10.0));
    }
}
