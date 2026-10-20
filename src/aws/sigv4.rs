//! AWS Signature Version 4, header-based signing.
//! https://docs.aws.amazon.com/IAM/latest/UserGuide/create-signed-request.html

use hmac::{Hmac, KeyInit, Mac};
use sha2::{Digest, Sha256};

use super::credentials::Credentials;
use crate::util::{hex, uri_encode};

pub struct Signable<'a> {
    pub method: &'a str,
    pub host: &'a str,
    /// Path exactly as it must appear in the canonical request.
    pub canonical_uri: &'a str,
    /// Raw (unencoded) query parameters.
    pub query: &'a [(String, String)],
    /// Extra headers to sign, besides host / x-amz-date / x-amz-security-token.
    pub headers: &'a [(String, String)],
    pub payload: &'a [u8],
}

fn hmac(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

pub fn sha256_hex(data: &[u8]) -> String {
    hex(&Sha256::digest(data))
}

pub fn canonical_query(query: &[(String, String)]) -> String {
    let mut pairs: Vec<(String, String)> =
        query.iter().map(|(k, v)| (uri_encode(k), uri_encode(v))).collect();
    pairs.sort();
    pairs
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&")
}

/// Returns the headers to add to the request: `x-amz-date`, optionally
/// `x-amz-security-token`, and `authorization`.
pub fn sign(
    req: &Signable,
    creds: &Credentials,
    region: &str,
    service: &str,
    amz_date: &str,
) -> Vec<(String, String)> {
    let date = &amz_date[..8];

    let mut headers: Vec<(String, String)> = req
        .headers
        .iter()
        .map(|(k, v)| (k.to_ascii_lowercase(), v.split_whitespace().collect::<Vec<_>>().join(" ")))
        .collect();
    headers.push(("host".into(), req.host.into()));
    headers.push(("x-amz-date".into(), amz_date.into()));
    if let Some(token) = &creds.session_token {
        headers.push(("x-amz-security-token".into(), token.clone()));
    }
    headers.sort();

    let canonical_headers: String = headers.iter().map(|(k, v)| format!("{k}:{v}\n")).collect();
    let signed_headers = headers.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>().join(";");

    let canonical_request = format!(
        "{}\n{}\n{}\n{}\n{}\n{}",
        req.method,
        req.canonical_uri,
        canonical_query(req.query),
        canonical_headers,
        signed_headers,
        sha256_hex(req.payload),
    );

    let scope = format!("{date}/{region}/{service}/aws4_request");
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}",
        sha256_hex(canonical_request.as_bytes())
    );

    let signature = hex(&hmac(&signing_key(creds, date, region, service), string_to_sign.as_bytes()));

    let mut out = vec![("x-amz-date".to_string(), amz_date.to_string())];
    if let Some(token) = &creds.session_token {
        out.push(("x-amz-security-token".into(), token.clone()));
    }
    out.push((
        "authorization".into(),
        format!(
            "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed_headers}, Signature={signature}",
            creds.access_key_id
        ),
    ));
    out
}

fn signing_key(creds: &Credentials, date: &str, region: &str, service: &str) -> Vec<u8> {
    let k_date = hmac(format!("AWS4{}", creds.secret_access_key).as_bytes(), date.as_bytes());
    let k_region = hmac(&k_date, region.as_bytes());
    let k_service = hmac(&k_region, service.as_bytes());
    hmac(&k_service, b"aws4_request")
}

/// Query-string signing (a presigned URL): returns the query string, signature included,
/// for `method` on `host` + `canonical_uri`, valid `expires` seconds from `amz_date`. The
/// payload is not signed, so anyone with the URL can use it until it expires (or until
/// the credentials that signed it do).
#[allow(clippy::too_many_arguments)]
pub fn presign_query(
    method: &str,
    host: &str,
    canonical_uri: &str,
    creds: &Credentials,
    region: &str,
    service: &str,
    amz_date: &str,
    expires: u64,
) -> String {
    let date = &amz_date[..8];
    let scope = format!("{date}/{region}/{service}/aws4_request");
    let mut query: Vec<(String, String)> = vec![
        ("X-Amz-Algorithm".into(), "AWS4-HMAC-SHA256".into()),
        ("X-Amz-Credential".into(), format!("{}/{scope}", creds.access_key_id)),
        ("X-Amz-Date".into(), amz_date.into()),
        ("X-Amz-Expires".into(), expires.to_string()),
        ("X-Amz-SignedHeaders".into(), "host".into()),
    ];
    if let Some(token) = &creds.session_token {
        query.push(("X-Amz-Security-Token".into(), token.clone()));
    }
    let query_string = canonical_query(&query);
    let canonical_request = format!("{method}\n{canonical_uri}\n{query_string}\nhost:{host}\n\nhost\nUNSIGNED-PAYLOAD");
    let string_to_sign =
        format!("AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}", sha256_hex(canonical_request.as_bytes()));
    let signature = hex(&hmac(&signing_key(creds, date, region, service), string_to_sign.as_bytes()));
    format!("{query_string}&X-Amz-Signature={signature}")
}

#[cfg(test)]
mod tests {
    //! Vectors from the official AWS SigV4 test suite (aws-signing-test-suite/v4).
    use super::*;

    const DATE: &str = "20150830T123600Z";

    fn creds(token: Option<&str>) -> Credentials {
        Credentials {
            access_key_id: "AKIDEXAMPLE".into(),
            secret_access_key: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".into(),
            session_token: token.map(Into::into),
            expires_at: None,
        }
    }

    fn signature(req: &Signable, token: Option<&str>) -> String {
        let auth = sign(req, &creds(token), "us-east-1", "service", DATE)
            .into_iter()
            .find(|(k, _)| k == "authorization")
            .unwrap()
            .1;
        auth.rsplit("Signature=").next().unwrap().to_string()
    }

    fn simple(method: &'static str, query: &'static [(String, String)]) -> Signable<'static> {
        Signable {
            method,
            host: "example.amazonaws.com",
            canonical_uri: "/",
            query,
            headers: &[],
            payload: b"",
        }
    }

    #[test]
    fn get_vanilla() {
        assert_eq!(
            signature(&simple("GET", &[]), None),
            "5fa00fa31553b73ebf1942676e86291e8372ff2a2260956d9b8aae1d763fbf31"
        );
    }

    #[test]
    fn post_vanilla() {
        assert_eq!(
            signature(&simple("POST", &[]), None),
            "5da7c1a2acd57cee7505fc6676e4e544621c30862966e37dddb68e92efbe5d6b"
        );
    }

    #[test]
    fn get_vanilla_query_order_key_case() {
        let query = vec![
            ("Param2".to_string(), "value2".to_string()),
            ("Param1".to_string(), "value1".to_string()),
        ];
        let req = Signable { query: &query, ..simple("GET", &[]) };
        assert_eq!(
            signature(&req, None),
            "b97d918cfa904a5beff61c982a1b6f458b799221646efd99d3219ec94cdf2500"
        );
    }

    #[test]
    fn get_vanilla_with_session_token() {
        let token = "6e86291e8372ff2a2260956d9b8aae1d763fbf315fa00fa31553b73ebf194267";
        assert_eq!(
            signature(&simple("GET", &[]), Some(token)),
            "07ec1639c89043aa0e3e2de82b96708f198cceab042d4a97044c66dd9f74e7f8"
        );
    }

    #[test]
    fn post_x_www_form_urlencoded() {
        let body = b"Param1=value1";
        let headers = vec![
            ("Content-Type".to_string(), "application/x-www-form-urlencoded".to_string()),
            ("Content-Length".to_string(), "13".to_string()),
            ("X-Amz-Content-Sha256".to_string(), sha256_hex(body)),
        ];
        let req = Signable { headers: &headers, payload: body, ..simple("POST", &[]) };
        assert_eq!(
            signature(&req, None),
            "d3875051da38690788ef43de4db0d8f280229d82040bfac253562e56c3f20e0b"
        );
    }

    #[test]
    fn get_space() {
        let req = Signable { canonical_uri: "/example%20space/", ..simple("GET", &[]) };
        assert_eq!(
            signature(&req, None),
            "652487583200325589f1fba4c7e578f72c47cb61beeca81406b39ddec1366741"
        );
    }

    /// The presigned GET example of the S3 documentation ("Authenticating Requests: Using
    /// Query Parameters").
    #[test]
    fn presigned_url_matches_the_s3_documentation() {
        let creds = Credentials {
            access_key_id: "AKIAIOSFODNN7EXAMPLE".into(),
            secret_access_key: "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY".into(),
            session_token: None,
            expires_at: None,
        };
        let query =
            presign_query("GET", "examplebucket.s3.amazonaws.com", "/test.txt", &creds, "us-east-1", "s3", "20130524T000000Z", 86400);
        assert!(query.ends_with("X-Amz-Signature=aeeed9bbccd4d02ee5c0109b86d86835f995330da4c265957d157751f604d404"), "{query}");
    }
}
