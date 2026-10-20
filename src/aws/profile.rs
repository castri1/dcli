//! Reader for the shared AWS files: `~/.aws/config` and `~/.aws/credentials`.

use std::collections::HashMap;
use std::path::PathBuf;

use indexmap::IndexMap;

use crate::util::home_dir;

pub type Section = HashMap<String, String>;

#[derive(Debug, Default)]
pub struct AwsFiles {
    /// `[default]` and `[profile x]` sections of the config file, in file order.
    profiles: IndexMap<String, Section>,
    /// `[sso-session x]` sections.
    sso_sessions: HashMap<String, Section>,
    /// Sections of the credentials file.
    credentials: HashMap<String, Section>,
}

fn aws_path(env_var: &str, file: &str) -> Option<PathBuf> {
    if let Some(p) = std::env::var_os(env_var).filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(p));
    }
    home_dir().map(|h| h.join(".aws").join(file))
}

/// Parses an INI file into `(header, section)` pairs, lower-casing keys.
/// Indented lines (nested sub-properties such as `s3 =` blocks) are skipped.
fn parse_ini(text: &str) -> Vec<(String, Section)> {
    let mut out: Vec<(String, Section)> = Vec::new();
    for raw in text.lines() {
        if raw.starts_with(char::is_whitespace) && !raw.trim().is_empty() {
            continue;
        }
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if let Some(header) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            out.push((header.trim().to_string(), Section::new()));
            continue;
        }
        if let (Some((k, v)), Some((_, section))) = (line.split_once('='), out.last_mut()) {
            section.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
        }
    }
    out
}

impl AwsFiles {
    pub fn load() -> Self {
        let read = |p: Option<PathBuf>| p.and_then(|p| std::fs::read_to_string(p).ok()).unwrap_or_default();
        Self::parse(
            &read(aws_path("AWS_CONFIG_FILE", "config")),
            &read(aws_path("AWS_SHARED_CREDENTIALS_FILE", "credentials")),
        )
    }

    pub fn parse(config: &str, credentials: &str) -> Self {
        let mut files = AwsFiles::default();
        for (header, section) in parse_ini(config) {
            let mut words = header.split_whitespace();
            match (words.next(), words.next()) {
                (Some("default"), None) => {
                    files.profiles.entry("default".into()).or_default().extend(section)
                }
                (Some("profile"), Some(name)) => {
                    files.profiles.entry(name.into()).or_default().extend(section)
                }
                (Some("sso-session"), Some(name)) => {
                    files.sso_sessions.entry(name.into()).or_default().extend(section)
                }
                _ => {}
            }
        }
        for (header, section) in parse_ini(credentials) {
            files.credentials.entry(header).or_default().extend(section);
        }
        files
    }

    /// The merged profile; keys from the credentials file win.
    pub fn profile(&self, name: &str) -> Option<Section> {
        let config = self.profiles.get(name);
        let creds = self.credentials.get(name);
        if config.is_none() && creds.is_none() {
            return None;
        }
        let mut merged = config.cloned().unwrap_or_default();
        if let Some(c) = creds {
            merged.extend(c.clone());
        }
        Some(merged)
    }

    pub fn sso_session(&self, name: &str) -> Option<&Section> {
        self.sso_sessions.get(name)
    }

    /// Profile names in config-file order, then any only present in the credentials file.
    pub fn profile_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.profiles.keys().cloned().collect();
        let mut extra: Vec<&String> =
            self.credentials.keys().filter(|k| !self.profiles.contains_key(*k)).collect();
        extra.sort();
        names.extend(extra.into_iter().cloned());
        names
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_profiles_sessions_and_credentials() {
        let config = "\
[default]
region = us-east-1

[profile dev]
sso_session = corp
sso_account_id = 111122223333
sso_role_name = Admin
region = us-west-2
s3 =
  max_concurrent_requests = 10

[sso-session corp]
sso_start_url = https://corp.awsapps.com/start
sso_region = us-east-1
";
        let creds = "[ci]\naws_access_key_id = AKID\naws_secret_access_key = SECRET\n";
        let files = AwsFiles::parse(config, creds);
        let dev = files.profile("dev").unwrap();
        assert_eq!(dev["sso_session"], "corp");
        assert_eq!(dev["region"], "us-west-2");
        assert!(!dev.contains_key("max_concurrent_requests"));
        assert_eq!(files.sso_session("corp").unwrap()["sso_region"], "us-east-1");
        assert_eq!(files.profile("ci").unwrap()["aws_access_key_id"], "AKID");
        assert_eq!(files.profile_names(), vec!["default", "dev", "ci"]);
    }
}
