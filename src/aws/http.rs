//! One shared HTTP agent (keep-alive connection pool, proxy from env) for every AWS call.

use std::sync::OnceLock;
use std::time::Duration;

use ureq::Agent;

use super::error::{AwsError, Result};

pub struct RawResponse {
    pub status: u16,
    pub error_type: Option<String>,
    /// Response headers, names lower-cased.
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl RawResponse {
    pub fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }
}

pub fn agent() -> &'static Agent {
    static AGENT: OnceLock<Agent> = OnceLock::new();
    AGENT.get_or_init(|| {
        let config = Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(Some(Duration::from_secs(30)))
            .timeout_connect(Some(Duration::from_secs(10)))
            .user_agent(concat!("dcli/", env!("CARGO_PKG_VERSION")))
            .build();
        Agent::new_with_config(config)
    })
}

pub fn send(method: &str, url: &str, headers: &[(String, String)], body: Vec<u8>) -> Result<RawResponse> {
    let mut builder = ureq::http::Request::builder().method(method).uri(url);
    for (k, v) in headers {
        builder = builder.header(k.as_str(), v.as_str());
    }
    let request = builder.body(body).map_err(|e| AwsError::Network(e.to_string()))?;
    let mut response = agent().run(request).map_err(|e| AwsError::Network(e.to_string()))?;
    let status = response.status().as_u16();
    let headers = header_list(response.headers());
    let error_type = headers.iter().find(|(k, _)| k == "x-amzn-errortype").map(|(_, v)| v.clone());
    let body = if method == "HEAD" {
        Vec::new()
    } else {
        response
            .body_mut()
            .with_config()
            .limit(64 * 1024 * 1024)
            .read_to_vec()
            .map_err(|e| AwsError::Network(e.to_string()))?
    };
    Ok(RawResponse { status, error_type, headers, body })
}

fn header_list(headers: &ureq::http::HeaderMap) -> Vec<(String, String)> {
    headers
        .iter()
        .filter_map(|(k, v)| Some((k.as_str().to_ascii_lowercase(), v.to_str().ok()?.to_string())))
        .collect()
}

/// GETs `url` into the file at `path`, streaming (no size limit). Returns the bytes
/// written; on an error status the body is read as an AWS error.
pub fn download(url: &str, headers: &[(String, String)], path: &std::path::Path) -> Result<u64> {
    let mut builder = ureq::http::Request::builder().method("GET").uri(url);
    for (k, v) in headers {
        builder = builder.header(k.as_str(), v.as_str());
    }
    let request = builder.body(Vec::new()).map_err(|e| AwsError::Network(e.to_string()))?;
    // Large files take longer than the agent's global timeout: a dedicated agent without one.
    let config = Agent::config_builder()
        .http_status_as_error(false)
        .timeout_connect(Some(Duration::from_secs(10)))
        .user_agent(concat!("dcli/", env!("CARGO_PKG_VERSION")))
        .build();
    let mut response = Agent::new_with_config(config).run(request).map_err(|e| AwsError::Network(e.to_string()))?;
    let status = response.status().as_u16();
    if !(200..300).contains(&status) {
        let headers = header_list(response.headers());
        let body = response.body_mut().read_to_vec().unwrap_or_default();
        let error_type = headers.iter().find(|(k, _)| k == "x-amzn-errortype").map(|(_, v)| v.clone());
        return Err(api_error(&RawResponse { status, error_type, headers, body }));
    }
    let mut file = std::fs::File::create(path).map_err(|e| AwsError::Config(format!("cannot write {}: {e}", path.display())))?;
    let mut reader = response.body_mut().with_config().limit(u64::MAX).reader();
    std::io::copy(&mut reader, &mut file).map_err(|e| AwsError::Network(e.to_string()))
}

/// Turns a non-2xx response into an `AwsError::Api`, reading the error code from the
/// `x-amzn-ErrorType` header or the JSON body (`__type`, `code`, `Code`).
pub fn api_error(resp: &RawResponse) -> AwsError {
    // Query APIs (CloudFormation…) answer with XML: <Error><Code>…</Code><Message>…</Message>.
    if resp.body.trim_ascii_start().starts_with(b"<") {
        let text = String::from_utf8_lossy(&resp.body);
        if let Some(code) = crate::util::xml_text(&text, "Code") {
            let message = crate::util::xml_text(&text, "Message").unwrap_or_default();
            return AwsError::Api { status: resp.status, code, message };
        }
    }
    let json: serde_json::Value = serde_json::from_slice(&resp.body).unwrap_or_default();
    let field = |names: &[&str]| {
        names
            .iter()
            .find_map(|n| json.get(*n).and_then(|v| v.as_str()))
            .map(str::to_string)
    };
    let raw_code = resp
        .error_type
        .clone()
        .or_else(|| field(&["__type", "code", "Code"]))
        .unwrap_or_else(|| format!("HTTP{}", resp.status));
    // "com.amazon.coral#ThrottlingException" / "ResourceNotFoundException:http://…"
    let code = raw_code
        .rsplit('#')
        .next()
        .unwrap_or(&raw_code)
        .split(':')
        .next()
        .unwrap_or(&raw_code)
        .to_string();
    let message = field(&["message", "Message", "errorMessage"]).unwrap_or_else(|| {
        let text = String::from_utf8_lossy(&resp.body);
        crate::util::ellipsize(text.trim(), 200)
    });
    AwsError::Api { status: resp.status, code, message }
}
