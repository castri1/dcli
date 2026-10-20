//! IAM Identity Center (SSO): reads the token cache written by `aws sso login`,
//! refreshes it when possible, and exchanges it for role credentials.

use std::path::PathBuf;

use serde::Deserialize;
use serde_json::{Value, json};
use sha1::{Digest, Sha1};

use super::credentials::Credentials;
use super::error::{AwsError, Result};
use super::http::{api_error, send};
use super::profile::{AwsFiles, Section};
use crate::util::{hex, home_dir, now_secs, parse_rfc3339, rfc3339, uri_encode};

#[derive(Debug, Clone)]
pub struct SsoConfig {
    pub profile: String,
    pub start_url: String,
    pub region: String,
    pub account_id: String,
    pub role_name: String,
    /// Set for `sso_session` profiles; legacy profiles key the cache by start URL.
    pub session_name: Option<String>,
}

impl SsoConfig {
    pub fn from_profile(name: &str, p: &Section, files: &AwsFiles) -> Result<SsoConfig> {
        let missing = |key: &str| AwsError::Config(format!("SSO profile '{name}' is missing '{key}'"));
        let (start_url, region, session_name) = match p.get("sso_session") {
            Some(session) => {
                let s = files.sso_session(session).ok_or_else(|| {
                    AwsError::Config(format!("sso-session '{session}' (profile '{name}') not found in ~/.aws/config"))
                })?;
                (
                    s.get("sso_start_url").ok_or_else(|| missing("sso_start_url"))?.clone(),
                    s.get("sso_region").ok_or_else(|| missing("sso_region"))?.clone(),
                    Some(session.clone()),
                )
            }
            None => (
                p.get("sso_start_url").ok_or_else(|| missing("sso_start_url"))?.clone(),
                p.get("sso_region").ok_or_else(|| missing("sso_region"))?.clone(),
                None,
            ),
        };
        Ok(SsoConfig {
            profile: name.to_string(),
            start_url,
            region,
            account_id: p.get("sso_account_id").ok_or_else(|| missing("sso_account_id"))?.clone(),
            role_name: p.get("sso_role_name").ok_or_else(|| missing("sso_role_name"))?.clone(),
            session_name,
        })
    }

    fn cache_path(&self) -> Option<PathBuf> {
        let key = self.session_name.as_deref().unwrap_or(&self.start_url);
        let file = format!("{}.json", hex(&Sha1::digest(key.as_bytes())));
        Some(home_dir()?.join(".aws").join("sso").join("cache").join(file))
    }

    fn login_required(&self, detail: impl Into<String>) -> AwsError {
        AwsError::LoginRequired { profile: self.profile.clone(), detail: detail.into() }
    }
}

/// A valid access token from the cache, refreshed through SSO-OIDC when it has expired
/// but the cached refresh token is still usable.
fn access_token(config: &SsoConfig) -> Result<String> {
    let path = config.cache_path().ok_or_else(|| config.login_required("no home directory"))?;
    let text = std::fs::read_to_string(&path).map_err(|_| config.login_required("no cached SSO session"))?;
    let mut cache: Value = serde_json::from_str(&text).map_err(|_| config.login_required("unreadable SSO cache"))?;

    let str_field = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).map(str::to_string);
    let expires_at = str_field(&cache, "expiresAt").as_deref().and_then(parse_rfc3339).unwrap_or(0.0);
    let token = str_field(&cache, "accessToken").unwrap_or_default();
    if !token.is_empty() && expires_at - now_secs() > 60.0 {
        return Ok(token);
    }

    // Expired: try the refresh token (only sso-session profiles get one).
    let refresh = (
        str_field(&cache, "refreshToken"),
        str_field(&cache, "clientId"),
        str_field(&cache, "clientSecret"),
    );
    let registration_valid = str_field(&cache, "registrationExpiresAt")
        .as_deref()
        .and_then(parse_rfc3339)
        .is_some_and(|t| t > now_secs());
    let (Some(refresh_token), Some(client_id), Some(client_secret)) = refresh else {
        return Err(config.login_required("SSO session expired"));
    };
    if !registration_valid {
        return Err(config.login_required("SSO client registration expired"));
    }

    let body = json!({
        "clientId": client_id,
        "clientSecret": client_secret,
        "grantType": "refresh_token",
        "refreshToken": refresh_token,
    });
    let url = format!("https://oidc.{}.amazonaws.com/token", config.region);
    let headers = [("content-type".to_string(), "application/json".to_string())];
    let resp = send("POST", &url, &headers, body.to_string().into_bytes())?;
    if !resp.ok() {
        return Err(config.login_required(format!("SSO refresh failed: {}", api_error(&resp).short())));
    }

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct TokenResponse {
        access_token: String,
        expires_in: i64,
        refresh_token: Option<String>,
    }
    let fresh: TokenResponse =
        serde_json::from_slice(&resp.body).map_err(|e| AwsError::Network(format!("bad SSO token response: {e}")))?;

    cache["accessToken"] = json!(fresh.access_token);
    cache["expiresAt"] = json!(rfc3339(now_secs() as i64 + fresh.expires_in));
    if let Some(rt) = fresh.refresh_token {
        cache["refreshToken"] = json!(rt);
    }
    // Write back like the AWS CLI does, atomically, so other tools see the new token.
    let tmp = path.with_extension("json.tmp");
    if std::fs::write(&tmp, serde_json::to_vec(&cache).unwrap_or_default()).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
    Ok(fresh.access_token)
}

pub fn role_credentials(config: &SsoConfig) -> Result<Credentials> {
    let token = access_token(config)?;
    let url = format!(
        "https://portal.sso.{}.amazonaws.com/federation/credentials?account_id={}&role_name={}",
        config.region,
        uri_encode(&config.account_id),
        uri_encode(&config.role_name),
    );
    let headers = [("x-amz-sso_bearer_token".to_string(), token)];
    let resp = send("GET", &url, &headers, Vec::new())?;
    if resp.status == 401 || resp.status == 403 {
        return Err(config.login_required(api_error(&resp).short()));
    }
    if !resp.ok() {
        return Err(api_error(&resp));
    }

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct RoleCredentials {
        access_key_id: String,
        secret_access_key: String,
        session_token: String,
        /// Milliseconds since the epoch.
        expiration: i64,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Envelope {
        role_credentials: RoleCredentials,
    }
    let env: Envelope =
        serde_json::from_slice(&resp.body).map_err(|e| AwsError::Network(format!("bad SSO credentials response: {e}")))?;
    let c = env.role_credentials;
    Ok(Credentials {
        access_key_id: c.access_key_id,
        secret_access_key: c.secret_access_key,
        session_token: Some(c.session_token),
        expires_at: Some(c.expiration as f64 / 1000.0),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_key_matches_aws_cli() {
        // The AWS CLI names the cache file sha1(session name).
        let files = AwsFiles::parse(
            "[profile p]\nsso_session = my-sso\nsso_account_id = 1\nsso_role_name = r\n\
             [sso-session my-sso]\nsso_start_url = https://x.awsapps.com/start\nsso_region = us-east-1\n",
            "",
        );
        let c = SsoConfig::from_profile("p", &files.profile("p").unwrap(), &files).unwrap();
        let name = c.cache_path().unwrap();
        let name = name.file_name().unwrap().to_str().unwrap();
        assert_eq!(name, format!("{}.json", hex(&Sha1::digest(b"my-sso"))));
        assert_eq!(c.region, "us-east-1");
    }
}
