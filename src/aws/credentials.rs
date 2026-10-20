//! Credential resolution for a named profile: static keys, IAM Identity Center (SSO)
//! or `credential_process`. Resolved credentials are cached until shortly before expiry.

use std::collections::HashMap;
use std::process::Command;
use std::sync::{Arc, Mutex, OnceLock};

use serde::Deserialize;

use super::error::{AwsError, Result};
use super::profile::{AwsFiles, Section};
use super::sso;
use crate::util::{now_secs, parse_rfc3339};

#[derive(Debug, Clone)]
pub struct Credentials {
    pub access_key_id: String,
    pub secret_access_key: String,
    pub session_token: Option<String>,
    /// Unix seconds.
    pub expires_at: Option<f64>,
}

impl Credentials {
    fn fresh(&self) -> bool {
        self.expires_at.is_none_or(|t| t - now_secs() > 300.0)
    }
}

pub struct CredentialProvider {
    profile: String,
    cached: Mutex<Option<Credentials>>,
}

impl CredentialProvider {
    /// Providers are shared per profile so switching environments keeps their cache.
    pub fn for_profile(profile: &str) -> Arc<CredentialProvider> {
        static REGISTRY: OnceLock<Mutex<HashMap<String, Arc<CredentialProvider>>>> = OnceLock::new();
        let mut registry = REGISTRY.get_or_init(Default::default).lock().unwrap();
        registry
            .entry(profile.to_string())
            .or_insert_with(|| {
                Arc::new(CredentialProvider { profile: profile.to_string(), cached: Mutex::new(None) })
            })
            .clone()
    }

    pub fn get(&self) -> Result<Credentials> {
        let mut cached = self.cached.lock().unwrap();
        if let Some(c) = cached.as_ref().filter(|c| c.fresh()) {
            return Ok(c.clone());
        }
        let creds = resolve(&self.profile)?;
        *cached = Some(creds.clone());
        Ok(creds)
    }

    pub fn invalidate(&self) {
        *self.cached.lock().unwrap() = None;
    }
}

/// Profile name used when an environment leaves `profile` empty.
pub fn effective_profile(profile: &str) -> String {
    if !profile.is_empty() {
        return profile.to_string();
    }
    std::env::var("AWS_PROFILE").ok().filter(|p| !p.is_empty()).unwrap_or_else(|| "default".into())
}

fn from_env() -> Option<Credentials> {
    let key = std::env::var("AWS_ACCESS_KEY_ID").ok().filter(|v| !v.is_empty())?;
    let secret = std::env::var("AWS_SECRET_ACCESS_KEY").ok().filter(|v| !v.is_empty())?;
    Some(Credentials {
        access_key_id: key,
        secret_access_key: secret,
        session_token: std::env::var("AWS_SESSION_TOKEN").ok().filter(|v| !v.is_empty()),
        expires_at: None,
    })
}

fn resolve(profile: &str) -> Result<Credentials> {
    if profile.is_empty()
        && let Some(c) = from_env()
    {
        return Ok(c);
    }
    let name = effective_profile(profile);
    let files = AwsFiles::load();
    let section = files.profile(&name).ok_or_else(|| {
        AwsError::Config(format!("AWS profile '{name}' not found in ~/.aws/config or ~/.aws/credentials"))
    })?;
    resolve_section(&name, &section, &files)
}

fn resolve_section(name: &str, p: &Section, files: &AwsFiles) -> Result<Credentials> {
    if let (Some(key), Some(secret)) = (p.get("aws_access_key_id"), p.get("aws_secret_access_key")) {
        return Ok(Credentials {
            access_key_id: key.clone(),
            secret_access_key: secret.clone(),
            session_token: p.get("aws_session_token").cloned(),
            expires_at: None,
        });
    }
    if p.contains_key("sso_session") || p.contains_key("sso_start_url") {
        let config = sso::SsoConfig::from_profile(name, p, files)?;
        return sso::role_credentials(&config);
    }
    if let Some(command) = p.get("credential_process") {
        return credential_process(command);
    }
    if p.contains_key("role_arn") {
        return Err(AwsError::Config(format!(
            "profile '{name}' assumes a role (role_arn), which dcli does not support yet"
        )));
    }
    Err(AwsError::Config(format!(
        "profile '{name}' has no supported credentials (static keys, SSO or credential_process)"
    )))
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ProcessOutput {
    access_key_id: String,
    secret_access_key: String,
    session_token: Option<String>,
    expiration: Option<String>,
}

fn credential_process(command: &str) -> Result<Credentials> {
    let output = crate::platform::shell(command)
        .output()
        .map_err(|e| AwsError::Config(format!("credential_process failed to start: {e}")))?;
    if !output.status.success() {
        return Err(AwsError::Config(format!(
            "credential_process exited with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    let parsed: ProcessOutput = serde_json::from_slice(&output.stdout)
        .map_err(|e| AwsError::Config(format!("credential_process returned invalid JSON: {e}")))?;
    Ok(Credentials {
        access_key_id: parsed.access_key_id,
        secret_access_key: parsed.secret_access_key,
        session_token: parsed.session_token,
        expires_at: parsed.expiration.as_deref().and_then(parse_rfc3339),
    })
}

/// Runs `aws sso login` for the profile. Returns the command so callers decide how to
/// wire its stdio (inherited in the CLI, captured in the UI).
pub fn sso_login_command(profile: &str) -> Command {
    let mut cmd = Command::new("aws");
    cmd.args(["sso", "login", "--profile", &effective_profile(profile)]);
    cmd
}
