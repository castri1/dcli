//! Minimal AWS client: SigV4-signed JSON calls over a shared HTTP agent.
//!
//! Adding a service = one `Service` constant plus typed request/response structs in
//! its own file. Most AWS APIs speak either `awsJson1_1` (`Aws::call`) or `restJson1`
//! (`Aws::rest`), so no per-service SDK crate is needed.

pub mod cloudformation;
pub mod codebuild;
pub mod codepipeline;
pub mod credentials;
pub mod error;
pub mod http;
pub mod lambda;
pub mod logs;
pub mod profile;
pub mod secrets;
pub mod sigv4;
pub mod ssm;
pub mod sso;

use std::sync::Arc;
use std::time::Duration;

use serde::de::DeserializeOwned;
use serde_json::Value;

pub use credentials::CredentialProvider;
pub use error::{AwsError, Result};

use crate::util::{amz_dates, now_secs, uri_encode};

/// How to reach and sign for one AWS service.
pub struct Service {
    /// Host prefix: `{endpoint}.{region}.amazonaws.com`.
    pub endpoint: &'static str,
    /// SigV4 signing name.
    pub signing_name: &'static str,
    /// `X-Amz-Target` prefix for `awsJson1_1` services; empty for REST services.
    pub target_prefix: &'static str,
}

pub struct Aws {
    pub profile: String,
    pub region: String,
    creds: Arc<CredentialProvider>,
}

const MAX_ATTEMPTS: u32 = 4;

impl Aws {
    pub fn new(profile: &str, region: &str) -> Arc<Aws> {
        Arc::new(Aws {
            profile: profile.to_string(),
            region: region.to_string(),
            creds: CredentialProvider::for_profile(profile),
        })
    }

    /// Forgets cached credentials (after an SSO login, for example).
    pub fn reset_credentials(&self) {
        self.creds.invalidate();
    }

    /// Fails fast with `LoginRequired` when the profile has no usable session.
    pub fn check_credentials(&self) -> Result<()> {
        self.creds.get().map(|_| ())
    }

    /// `(base URL, host)` for a service. `AWS_ENDPOINT_URL`, as in the AWS SDKs, sends
    /// every service to one endpoint (LocalStack, a proxy, a test server).
    fn endpoint(&self, svc: &Service) -> (String, String) {
        if let Some(url) = std::env::var("AWS_ENDPOINT_URL").ok().filter(|u| !u.is_empty()) {
            let base = url.trim_end_matches('/').to_string();
            let host = base.split_once("://").map_or(base.as_str(), |(_, h)| h).to_string();
            return (base, host);
        }
        let suffix = if self.region.starts_with("cn-") { "amazonaws.com.cn" } else { "amazonaws.com" };
        let host = format!("{}.{}.{suffix}", svc.endpoint, self.region);
        (format!("https://{host}"), host)
    }

    /// `awsJson1_1` call: `POST /` with `X-Amz-Target: {prefix}.{operation}`.
    pub fn call<T: DeserializeOwned>(&self, svc: &Service, operation: &str, input: &Value) -> Result<T> {
        let headers = vec![
            ("content-type".to_string(), "application/x-amz-json-1.1".to_string()),
            ("x-amz-target".to_string(), format!("{}.{operation}", svc.target_prefix)),
        ];
        let body = serde_json::to_vec(input).unwrap_or_default();
        let bytes = self.send(svc, "POST", &[], &[], headers, body)?;
        parse(&bytes)
    }

    /// `awsQuery` call (CloudFormation and other older APIs): a form-encoded `POST /`;
    /// returns the XML response body.
    pub fn query(&self, svc: &Service, action: &str, version: &str, params: &[(&str, &str)]) -> Result<String> {
        let mut body = format!("Action={}&Version={}", uri_encode(action), uri_encode(version));
        for (k, v) in params {
            body.push_str(&format!("&{}={}", uri_encode(k), uri_encode(v)));
        }
        let headers = vec![("content-type".to_string(), "application/x-www-form-urlencoded; charset=utf-8".to_string())];
        let bytes = self.send(svc, "POST", &[], &[], headers, body.into_bytes())?;
        Ok(crate::util::sanitize(&String::from_utf8_lossy(&bytes)))
    }

    /// The same profile in another region (an action that deploys elsewhere, say).
    pub fn in_region(&self, region: &str) -> Arc<Aws> {
        Aws::new(&self.profile, region)
    }

    /// `restJson1` call. `segments` are raw path segments, encoded here.
    pub fn rest<T: DeserializeOwned>(
        &self,
        svc: &Service,
        method: &str,
        segments: &[&str],
        query: &[(String, String)],
        body: Option<&Value>,
    ) -> Result<T> {
        let mut headers = Vec::new();
        let payload = match body {
            Some(b) => {
                headers.push(("content-type".to_string(), "application/json".to_string()));
                serde_json::to_vec(b).unwrap_or_default()
            }
            None => Vec::new(),
        };
        let bytes = self.send(svc, method, segments, query, headers, payload)?;
        parse(&bytes)
    }

    fn send(
        &self,
        svc: &Service,
        method: &str,
        segments: &[&str],
        query: &[(String, String)],
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    ) -> Result<Vec<u8>> {
        let (base, host) = self.endpoint(svc);
        // Non-S3 services sign the path encoded twice.
        let path = format!("/{}", segments.iter().map(|s| uri_encode(s)).collect::<Vec<_>>().join("/"));
        let canonical_uri =
            format!("/{}", segments.iter().map(|s| uri_encode(&uri_encode(s))).collect::<Vec<_>>().join("/"));
        let query_string = sigv4::canonical_query(query);
        let url = if query_string.is_empty() {
            format!("{base}{path}")
        } else {
            format!("{base}{path}?{query_string}")
        };

        let mut refreshed = false;
        let mut attempt = 0;
        loop {
            attempt += 1;
            let creds = self.creds.get()?;
            let (_, amz_date) = amz_dates(now_secs() as i64);
            let signable = sigv4::Signable {
                method,
                host: &host,
                canonical_uri: &canonical_uri,
                query,
                headers: &headers,
                payload: &body,
            };
            let mut all_headers = headers.clone();
            all_headers.extend(sigv4::sign(&signable, &creds, &self.region, svc.signing_name, &amz_date));

            let result = http::send(method, &url, &all_headers, body.clone()).and_then(|resp| {
                if resp.ok() { Ok(resp.body) } else { Err(http::api_error(&resp)) }
            });
            match result {
                Ok(bytes) => return Ok(bytes),
                Err(e) if e.is_expired_credentials() && !refreshed => {
                    // Cached role credentials were revoked or expired early: resolve again once.
                    refreshed = true;
                    self.creds.invalidate();
                }
                Err(e) if e.is_expired_credentials() => {
                    return Err(AwsError::LoginRequired { profile: self.profile.clone(), detail: e.short() });
                }
                Err(e) if e.is_retryable() && attempt < MAX_ATTEMPTS => {
                    std::thread::sleep(Duration::from_millis(250 * 2u64.pow(attempt)));
                }
                Err(e) => return Err(e),
            }
        }
    }
}

/// Deserializes a response body. Some services (Lambda, for one) send absent fields as
/// explicit `null`; those are dropped first so every field falls back to its default.
/// Strings lose terminal control sequences: commit messages, logs and errors are shown
/// in a terminal and must not be able to drive it.
fn parse<T: DeserializeOwned>(bytes: &[u8]) -> Result<T> {
    let bytes = if bytes.is_empty() { b"{}".as_slice() } else { bytes };
    let unexpected = |e: serde_json::Error| AwsError::Network(format!("unexpected response from AWS: {e}"));
    let mut value: Value = serde_json::from_slice(bytes).map_err(unexpected)?;
    clean(&mut value);
    T::deserialize(value).map_err(unexpected)
}

fn clean(value: &mut Value) {
    match value {
        Value::Object(map) => {
            map.retain(|_, v| !v.is_null());
            map.values_mut().for_each(clean);
        }
        Value::Array(items) => items.iter_mut().for_each(clean),
        Value::String(s) if s.chars().any(char::is_control) => *s = crate::util::sanitize(s),
        _ => {}
    }
}

/// `us-east-1` → AWS console base URL for that region.
pub fn console_url(region: &str, path: &str) -> String {
    format!("https://{region}.console.aws.amazon.com/{path}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nulls_fall_back_to_defaults() {
        let body = br#"{"Functions":[{"FunctionName":"f","Layers":null,"Environment":null,
            "VpcConfig":{"SubnetIds":null,"VpcId":null},"Architectures":["arm64"]}],"NextMarker":null}"#;
        #[derive(serde::Deserialize, Default)]
        #[serde(rename_all = "PascalCase", default)]
        struct Out {
            functions: Vec<lambda::FunctionConfiguration>,
            next_marker: Option<String>,
        }
        let out: Out = parse(body).unwrap();
        let f = &out.functions[0];
        assert!(f.layers.is_empty() && f.environment.is_none() && out.next_marker.is_none());
        assert!(f.vpc_config.as_ref().unwrap().subnet_ids.is_empty());
        assert_eq!(f.architectures, ["arm64"]);

        let body = "{\"Functions\":[{\"FunctionName\":\"f\",\"Description\":\"x\\u001b]52;c;aGk=\\u0007y\"}]}";
        let out: Out = parse(body.as_bytes()).unwrap();
        assert_eq!(out.functions[0].description.as_deref(), Some("xy"));
    }
}
