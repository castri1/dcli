//! Minimal AWS client: SigV4-signed JSON calls over a shared HTTP agent.
//!
//! Adding a service = one `Service` constant plus typed request/response structs in
//! its own file. Most AWS APIs speak either `awsJson1_1` (`Aws::call`) or `restJson1`
//! (`Aws::rest`), so no per-service SDK crate is needed.

pub mod alarms;
pub mod apigateway;
pub mod appconfig;
pub mod bedrock;
pub mod ce;
pub mod cloudformation;
pub mod cloudwatch;
pub mod codebuild;
pub mod codepipeline;
pub mod credentials;
pub mod dms;
pub mod dynamodb;
pub mod ec2;
pub mod error;
pub mod events;
pub mod http;
pub mod lambda;
pub mod logs;
pub mod metrics;
pub mod pi;
pub mod profile;
pub mod rds;
pub mod redshift;
pub mod redshift_data;
pub mod s3;
pub mod savingsplans;
pub mod secrets;
pub mod sfn;
pub mod sigv4;
pub mod sqs;
pub mod ssm;
pub mod sso;
pub mod sts;
pub mod xml;
pub mod xray;

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

/// Services that answer on one host whatever the region (`savingsplans.amazonaws.com`),
/// signed for the client's region (us-east-1).
const GLOBAL_ENDPOINTS: &[&str] = &["savingsplans"];

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
        let host = if GLOBAL_ENDPOINTS.contains(&svc.endpoint) {
            format!("{}.{suffix}", svc.endpoint)
        } else {
            format!("{}.{}.{suffix}", svc.endpoint, self.region)
        };
        (format!("https://{host}"), host)
    }

    /// `awsJson1_1` call: `POST /` with `X-Amz-Target: {prefix}.{operation}`.
    pub fn call<T: DeserializeOwned>(&self, svc: &Service, operation: &str, input: &Value) -> Result<T> {
        self.json_call(svc, operation, input, "application/x-amz-json-1.1")
    }

    /// Like [`call`](Self::call) for an operation that must not run twice (starting or
    /// reloading a DMS task): sent again only when AWS throttled it.
    pub fn call_once<T: DeserializeOwned>(&self, svc: &Service, operation: &str, input: &Value) -> Result<T> {
        let headers = vec![
            ("content-type".to_string(), "application/x-amz-json-1.1".to_string()),
            ("x-amz-target".to_string(), format!("{}.{operation}", svc.target_prefix)),
        ];
        let body = serde_json::to_vec(input).unwrap_or_default();
        let resp = self.send_with(svc, "POST", &[], &[], headers, body, false)?;
        parse(&resp.body)
    }

    /// `awsJson1_0` call (DynamoDB, Step Functions): same as [`call`](Self::call), another
    /// content type.
    pub fn call_json10<T: DeserializeOwned>(&self, svc: &Service, operation: &str, input: &Value) -> Result<T> {
        self.json_call(svc, operation, input, "application/x-amz-json-1.0")
    }

    fn json_call<T: DeserializeOwned>(&self, svc: &Service, operation: &str, input: &Value, content_type: &str) -> Result<T> {
        let headers = vec![
            ("content-type".to_string(), content_type.to_string()),
            ("x-amz-target".to_string(), format!("{}.{operation}", svc.target_prefix)),
        ];
        let body = serde_json::to_vec(input).unwrap_or_default();
        let resp = self.send(svc, "POST", &[], &[], headers, body)?;
        parse(&resp.body)
    }

    /// `awsQuery` call (CloudFormation and other older APIs): a form-encoded `POST /`;
    /// returns the XML response body.
    pub fn query(&self, svc: &Service, action: &str, version: &str, params: &[(&str, &str)]) -> Result<String> {
        self.query_with(svc, action, version, params, true)
    }

    /// Like [`Aws::query`] for a call that must not run twice (failing over a cluster,
    /// rebooting an instance): sent again only when AWS throttled it, not after a server
    /// error or a lost connection, when it may already have run.
    pub fn query_once(&self, svc: &Service, action: &str, version: &str, params: &[(&str, &str)]) -> Result<String> {
        self.query_with(svc, action, version, params, false)
    }

    fn query_with(&self, svc: &Service, action: &str, version: &str, params: &[(&str, &str)], idempotent: bool) -> Result<String> {
        let mut body = format!("Action={}&Version={}", uri_encode(action), uri_encode(version));
        for (k, v) in params {
            body.push_str(&format!("&{}={}", uri_encode(k), uri_encode(v)));
        }
        let headers = vec![("content-type".to_string(), "application/x-www-form-urlencoded; charset=utf-8".to_string())];
        let resp = self.send_with(svc, "POST", &[], &[], headers, body.into_bytes(), idempotent)?;
        Ok(crate::util::sanitize(&String::from_utf8_lossy(&resp.body)))
    }

    /// The same profile in another region (an action that deploys elsewhere, say).
    pub fn in_region(&self, region: &str) -> Arc<Aws> {
        Aws::new(&self.profile, region)
    }

    /// When the current credentials expire (Unix seconds); `None` for long-lived keys.
    /// Presigned URLs stop working then, whatever expiry they were given.
    pub fn credentials_expiry(&self) -> Option<f64> {
        self.creds.get().ok()?.expires_at
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
        self.rest_with(svc, method, segments, query, body, true)
    }

    /// Like [`Aws::rest`] for a request that must not run twice (testing an API Gateway
    /// method runs its integration): sent again only when AWS throttled it.
    pub fn rest_once<T: DeserializeOwned>(
        &self,
        svc: &Service,
        method: &str,
        segments: &[&str],
        query: &[(String, String)],
        body: Option<&Value>,
    ) -> Result<T> {
        self.rest_with(svc, method, segments, query, body, false)
    }

    fn rest_with<T: DeserializeOwned>(
        &self,
        svc: &Service,
        method: &str,
        segments: &[&str],
        query: &[(String, String)],
        body: Option<&Value>,
        idempotent: bool,
    ) -> Result<T> {
        let mut headers = Vec::new();
        // API Gateway answers in HAL (`_embedded`, `_links`) unless asked for plain JSON, as the SDKs do.
        if svc.signing_name == "apigateway" {
            headers.push(("accept".to_string(), "application/json".to_string()));
        }
        let payload = match body {
            Some(b) => {
                headers.push(("content-type".to_string(), "application/json".to_string()));
                serde_json::to_vec(b).unwrap_or_default()
            }
            None => Vec::new(),
        };
        let resp = self.send_with(svc, method, segments, query, headers, payload, idempotent)?;
        parse(&resp.body)
    }

    /// Any request, returning status, headers and the raw body of a successful response:
    /// S3 (XML, object bytes), AppConfig hosted configurations (the document as stored).
    pub fn raw(
        &self,
        svc: &Service,
        method: &str,
        segments: &[&str],
        query: &[(String, String)],
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    ) -> Result<http::RawResponse> {
        self.send(svc, method, segments, query, headers, body)
    }

    /// Like [`Aws::raw`] for a request that must not run twice (invoking a function): sent
    /// again only when AWS throttled it, not after a server error or a lost connection,
    /// when it may already have run.
    pub fn raw_once(
        &self,
        svc: &Service,
        method: &str,
        segments: &[&str],
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    ) -> Result<http::RawResponse> {
        self.send_with(svc, method, segments, &[], headers, body, false)
    }

    /// A URL anyone can GET until `expires` seconds from now, or until the credentials
    /// that signed it expire, whichever comes first (SSO sessions last hours).
    pub fn presign(&self, svc: &Service, segments: &[&str], expires: u64) -> Result<String> {
        let (base, host) = self.endpoint(svc);
        let (path, canonical_uri) = paths(svc, segments);
        let creds = self.creds.get()?;
        let (_, amz_date) = amz_dates(now_secs() as i64);
        let query = sigv4::presign_query("GET", &host, &canonical_uri, &creds, &self.region, svc.signing_name, &amz_date, expires);
        Ok(format!("{base}{path}?{query}"))
    }

    /// Streams a GET to the file at `path`; returns the bytes written.
    pub fn download(&self, svc: &Service, segments: &[&str], path: &std::path::Path) -> Result<u64> {
        let (url, headers) = self.signed(svc, "GET", segments, &[], Vec::new(), &[])?;
        http::download(&url, &headers, path)
    }

    /// URL and signed headers for a request.
    fn signed(
        &self,
        svc: &Service,
        method: &str,
        segments: &[&str],
        query: &[(String, String)],
        mut headers: Vec<(String, String)>,
        body: &[u8],
    ) -> Result<(String, Vec<(String, String)>)> {
        let (base, host) = self.endpoint(svc);
        let (path, canonical_uri) = paths(svc, segments);
        let query_string = sigv4::canonical_query(query);
        let url = if query_string.is_empty() { format!("{base}{path}") } else { format!("{base}{path}?{query_string}") };
        if svc.signing_name == "s3" {
            headers.push(("x-amz-content-sha256".to_string(), sigv4::sha256_hex(body)));
        }
        let creds = self.creds.get()?;
        let (_, amz_date) = amz_dates(now_secs() as i64);
        let signable = sigv4::Signable { method, host: &host, canonical_uri: &canonical_uri, query, headers: &headers, payload: body };
        let mut all = headers.clone();
        all.extend(sigv4::sign(&signable, &creds, &self.region, svc.signing_name, &amz_date));
        Ok((url, all))
    }

    fn send(
        &self,
        svc: &Service,
        method: &str,
        segments: &[&str],
        query: &[(String, String)],
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    ) -> Result<http::RawResponse> {
        self.send_with(svc, method, segments, query, headers, body, true)
    }

    /// `idempotent`: may be sent again after a server error or a network failure.
    #[allow(clippy::too_many_arguments)]
    fn send_with(
        &self,
        svc: &Service,
        method: &str,
        segments: &[&str],
        query: &[(String, String)],
        headers: Vec<(String, String)>,
        body: Vec<u8>,
        idempotent: bool,
    ) -> Result<http::RawResponse> {
        let mut refreshed = false;
        let mut attempt = 0;
        loop {
            attempt += 1;
            let (url, all_headers) = self.signed(svc, method, segments, query, headers.clone(), &body)?;
            let result = http::send(method, &url, &all_headers, body.clone())
                .and_then(|resp| if resp.ok() { Ok(resp) } else { Err(http::api_error(&resp)) });
            match result {
                Ok(resp) => return Ok(resp),
                Err(e) if e.is_expired_credentials() && !refreshed => {
                    // Cached role credentials were revoked or expired early: resolve again once.
                    refreshed = true;
                    self.creds.invalidate();
                }
                Err(e) if e.is_expired_credentials() => {
                    return Err(AwsError::LoginRequired { profile: self.profile.clone(), detail: e.short() });
                }
                Err(e) if (if idempotent { e.is_retryable() } else { e.is_throttling() }) && attempt < MAX_ATTEMPTS => {
                    std::thread::sleep(Duration::from_millis(250 * 2u64.pow(attempt)));
                }
                Err(e) => return Err(e),
            }
        }
    }
}

/// `(request path, canonical path)`. Every service but S3 signs the path encoded twice.
fn paths(svc: &Service, segments: &[&str]) -> (String, String) {
    let path = format!("/{}", segments.iter().map(|s| uri_encode(s)).collect::<Vec<_>>().join("/"));
    let canonical = if svc.signing_name == "s3" {
        path.clone()
    } else {
        format!("/{}", segments.iter().map(|s| uri_encode(&uri_encode(s))).collect::<Vec<_>>().join("/"))
    };
    (path, canonical)
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
        Value::String(s) if s.chars().any(crate::util::is_unsafe) => *s = crate::util::sanitize(s),
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
    fn global_services_answer_on_one_host() {
        if std::env::var_os("AWS_ENDPOINT_URL").is_some() {
            return;
        }
        let aws = Aws::new("default", "us-east-1");
        assert_eq!(aws.endpoint(&savingsplans::SERVICE).1, "savingsplans.amazonaws.com");
        assert_eq!(aws.endpoint(&ce::SERVICE).1, "ce.us-east-1.amazonaws.com");
        assert_eq!(aws.in_region("eu-west-1").endpoint(&sts::SERVICE).1, "sts.eu-west-1.amazonaws.com");
        assert!(sts::is_account_id("123456789012") && !sts::is_account_id("12345678901") && !sts::is_account_id("12345678901x"));
    }

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
