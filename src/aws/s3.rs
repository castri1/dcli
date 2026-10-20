//! S3 (restXml, path-style): buckets, objects, previews, presigned URLs, downloads.
//! Path-style URLs (`https://s3.{region}.amazonaws.com/{bucket}/{key}`) work for every
//! bucket name, dots included, and for endpoints such as LocalStack.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use super::{Aws, AwsError, Result, Service};
use crate::util::{parse_rfc3339, xml_blocks, xml_text, xml_text_raw};

pub const SERVICE: Service = Service { endpoint: "s3", signing_name: "s3", target_prefix: "" };

/// Bytes read for a preview.
pub const PREVIEW_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, Default)]
pub struct Bucket {
    pub name: String,
    /// Unix seconds.
    pub created: Option<f64>,
    /// Known from ListBuckets on recent APIs; otherwise found with [`bucket_region`].
    pub region: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct Object {
    pub key: String,
    pub size: i64,
    /// Unix seconds.
    pub modified: Option<f64>,
    pub storage_class: Option<String>,
}

/// One page of a folder: sub-folders (full prefixes, ending in `/`) and objects.
#[derive(Debug, Clone, Default)]
pub struct Page {
    pub folders: Vec<String>,
    pub objects: Vec<Object>,
    pub next: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct ObjectInfo {
    pub size: i64,
    pub content_type: Option<String>,
    /// Unix seconds.
    pub modified: Option<f64>,
    pub etag: Option<String>,
    pub storage_class: Option<String>,
    pub encryption: Option<String>,
    pub kms_key: Option<String>,
    pub version_id: Option<String>,
    /// `x-amz-meta-*` headers, without the prefix.
    pub metadata: Vec<(String, String)>,
}

/// The start of an object, for a preview.
#[derive(Debug, Clone)]
pub struct Preview {
    pub bytes: Vec<u8>,
    /// The object is longer than what was read.
    pub truncated: bool,
}

/// Request path segments for an object: the bucket, then the key split on `/`.
pub fn segments<'a>(bucket: &'a str, key: &'a str) -> Vec<&'a str> {
    std::iter::once(bucket).chain(key.split('/')).collect()
}

/// `s3://bucket/some/key` (or `bucket/some/key`) → `(bucket, key)`; the key may be empty.
pub fn parse_uri(uri: &str) -> Option<(String, String)> {
    let rest = uri.trim().strip_prefix("s3://").unwrap_or(uri.trim());
    let (bucket, key) = rest.split_once('/').unwrap_or((rest, ""));
    (!bucket.is_empty()).then(|| (bucket.to_string(), key.to_string()))
}

pub fn uri(bucket: &str, key: &str) -> String {
    format!("s3://{bucket}/{key}")
}

/// Every bucket of the account (ListBuckets). Recent APIs also report each bucket's region
/// when the request has a parameter, hence `max-buckets`.
pub fn list_buckets(aws: &Aws) -> Result<Vec<Bucket>> {
    let mut buckets = Vec::new();
    let mut token: Option<String> = None;
    loop {
        let mut query = vec![("max-buckets".to_string(), "1000".to_string())];
        if let Some(t) = &token {
            query.push(("continuation-token".to_string(), t.clone()));
        }
        let xml = text(aws.raw(&SERVICE, "GET", &[], &query, Vec::new(), Vec::new())?.body);
        buckets.extend(parse_buckets(&xml));
        token = xml_text_raw(&xml, "ContinuationToken").filter(|t| !t.is_empty());
        if token.is_none() {
            break;
        }
    }
    for b in &buckets {
        if let Some(r) = &b.region {
            region_cache().lock().unwrap().insert(b.name.clone(), r.clone());
        }
    }
    Ok(buckets)
}

fn text(bytes: Vec<u8>) -> String {
    String::from_utf8_lossy(&bytes).into_owned()
}

fn parse_buckets(xml: &str) -> Vec<Bucket> {
    let list = xml_blocks(xml, "Buckets").into_iter().next().unwrap_or("");
    xml_blocks(list, "Bucket")
        .into_iter()
        .map(|b| Bucket {
            name: xml_text(b, "Name").unwrap_or_default(),
            created: xml_text(b, "CreationDate").and_then(|d| parse_rfc3339(&d)),
            region: xml_text(b, "BucketRegion").filter(|r| !r.is_empty()),
        })
        .filter(|b| !b.name.is_empty())
        .collect()
}

fn region_cache() -> &'static Mutex<HashMap<String, String>> {
    static CACHE: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
    CACHE.get_or_init(Default::default)
}

/// The region a bucket lives in (cached: bucket names are global). GetBucketLocation on
/// us-east-1 answers for any bucket; HeadBucket's `x-amz-bucket-region` is the fallback.
pub fn bucket_region(aws: &Aws, bucket: &str) -> Result<String> {
    if let Some(r) = region_cache().lock().unwrap().get(bucket) {
        return Ok(r.clone());
    }
    let global = aws.in_region("us-east-1");
    let query = [("location".to_string(), String::new())];
    let region = match global.raw(&SERVICE, "GET", &[bucket], &query, Vec::new(), Vec::new()) {
        Ok(resp) => location_region(&text(resp.body)),
        Err(e) if e.is_login_required() => return Err(e),
        Err(_) => {
            let resp = global.raw(&SERVICE, "HEAD", &[bucket], &[], Vec::new(), Vec::new())?;
            resp.header("x-amz-bucket-region").unwrap_or("us-east-1").to_string()
        }
    };
    region_cache().lock().unwrap().insert(bucket.to_string(), region.clone());
    Ok(region)
}

/// GetBucketLocation's answer: empty means us-east-1 and `EU` the old name of eu-west-1.
/// The element carries an `xmlns` attribute, so it is read by hand.
fn location_region(xml: &str) -> String {
    let value = xml
        .find("<LocationConstraint")
        .map(|i| &xml[i..])
        .and_then(|rest| {
            let open = rest.find('>')?;
            if rest[..open].ends_with('/') {
                return Some(String::new());
            }
            let body = &rest[open + 1..];
            Some(crate::util::sanitize(&body[..body.find("</LocationConstraint>")?]))
        })
        .unwrap_or_default();
    match value.trim() {
        "" => "us-east-1".into(),
        "EU" => "eu-west-1".into(),
        r => r.to_string(),
    }
}

/// The same credentials, talking to the bucket's region.
pub fn regional(aws: &Aws, bucket: &str) -> Result<Arc<Aws>> {
    Ok(aws.in_region(&bucket_region(aws, bucket)?))
}

/// One page (up to 1,000 entries) of a folder: objects and sub-folders under `prefix`.
pub fn list_objects(aws: &Aws, bucket: &str, prefix: &str, token: Option<&str>) -> Result<Page> {
    let mut query = vec![
        ("list-type".to_string(), "2".to_string()),
        ("delimiter".to_string(), "/".to_string()),
        ("max-keys".to_string(), "1000".to_string()),
        ("prefix".to_string(), prefix.to_string()),
    ];
    if let Some(t) = token {
        query.push(("continuation-token".to_string(), t.to_string()));
    }
    let xml = text(aws.raw(&SERVICE, "GET", &[bucket], &query, Vec::new(), Vec::new())?.body);
    Ok(parse_page(&xml, prefix))
}

fn parse_page(xml: &str, prefix: &str) -> Page {
    let objects = xml_blocks(xml, "Contents")
        .into_iter()
        .map(|c| Object {
            key: xml_text_raw(c, "Key").unwrap_or_default(),
            size: xml_text(c, "Size").and_then(|s| s.parse().ok()).unwrap_or(0),
            modified: xml_text(c, "LastModified").and_then(|d| parse_rfc3339(&d)),
            storage_class: xml_text(c, "StorageClass"),
        })
        // The folder's own marker object ("photos/" inside "photos/").
        .filter(|o| !o.key.is_empty() && o.key != prefix)
        .collect();
    let folders = xml_blocks(xml, "CommonPrefixes").into_iter().filter_map(|p| xml_text_raw(p, "Prefix")).collect();
    let truncated = xml_text(xml, "IsTruncated").is_some_and(|t| t == "true");
    let next = xml_text_raw(xml, "NextContinuationToken").filter(|t| truncated && !t.is_empty());
    Page { folders, objects, next }
}

/// HeadObject: size, type, dates, storage class, encryption and user metadata.
pub fn head_object(aws: &Aws, bucket: &str, key: &str) -> Result<ObjectInfo> {
    let resp = aws.raw(&SERVICE, "HEAD", &segments(bucket, key), &[], Vec::new(), Vec::new())?;
    let h = |name: &str| resp.header(name).map(crate::util::sanitize);
    let mut metadata: Vec<(String, String)> = resp
        .headers
        .iter()
        .filter_map(|(k, v)| Some((k.strip_prefix("x-amz-meta-")?.to_string(), crate::util::sanitize(v))))
        .collect();
    metadata.sort();
    Ok(ObjectInfo {
        size: h("content-length").and_then(|v| v.parse().ok()).unwrap_or(0),
        content_type: h("content-type"),
        modified: h("last-modified").and_then(|v| parse_http_date(&v)),
        etag: h("etag").map(|e| e.trim_matches('"').to_string()),
        storage_class: h("x-amz-storage-class").or_else(|| Some("STANDARD".into())),
        encryption: h("x-amz-server-side-encryption"),
        kms_key: h("x-amz-server-side-encryption-aws-kms-key-id"),
        version_id: h("x-amz-version-id").filter(|v| v != "null"),
        metadata,
    })
}

/// `Sun, 06 Nov 1994 08:49:37 GMT` → Unix seconds.
fn parse_http_date(s: &str) -> Option<f64> {
    let parts: Vec<&str> = s.split_whitespace().collect();
    let [_, day, month, year, time, ..] = parts.as_slice() else { return None };
    const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    let month = MONTHS.iter().position(|m| m == month)? + 1;
    let day: u32 = day.parse().ok()?;
    parse_rfc3339(&format!("{year}-{month:02}-{day:02}T{time}Z"))
}

/// The first [`PREVIEW_BYTES`] of an object.
pub fn preview(aws: &Aws, bucket: &str, key: &str) -> Result<Preview> {
    let headers = vec![("range".to_string(), format!("bytes=0-{}", PREVIEW_BYTES - 1))];
    let resp = aws.raw(&SERVICE, "GET", &segments(bucket, key), &[], headers, Vec::new())?;
    // `content-range: bytes 0-65535/1234567` tells the full size.
    let total = resp.header("content-range").and_then(|r| r.rsplit('/').next()).and_then(|t| t.parse::<usize>().ok());
    let truncated = total.is_some_and(|t| t > resp.body.len()) || resp.body.len() > PREVIEW_BYTES;
    let mut bytes = resp.body;
    bytes.truncate(PREVIEW_BYTES);
    Ok(Preview { bytes, truncated })
}

/// The text of a preview, or `None` for binary content (NUL bytes or invalid UTF-8; a
/// character cut at the end of a truncated preview does not count).
pub fn preview_text(p: &Preview) -> Option<String> {
    if p.bytes.contains(&0) {
        return None;
    }
    match std::str::from_utf8(&p.bytes) {
        Ok(s) => Some(s.to_string()),
        Err(e) if e.error_len().is_none() => Some(String::from_utf8_lossy(&p.bytes[..e.valid_up_to()]).into_owned()),
        Err(_) => None,
    }
}

/// A link anyone can download the object with, for `expires` seconds (at most 7 days),
/// or until the credentials that signed it expire.
pub fn presigned_url(aws: &Aws, bucket: &str, key: &str, expires: u64) -> Result<String> {
    aws.presign(&SERVICE, &segments(bucket, key), expires.clamp(1, 7 * 86_400))
}

/// Where downloads go: `~/Downloads` when it exists, else the home directory.
pub fn downloads_dir() -> PathBuf {
    let home = crate::util::home_dir().unwrap_or_else(|| PathBuf::from("."));
    let downloads = home.join("Downloads");
    if downloads.is_dir() { downloads } else { home }
}

/// The key's file name, made safe for a path (no separators or control characters).
pub fn file_name(key: &str) -> String {
    let name = key.trim_end_matches('/').rsplit('/').next().unwrap_or("");
    let clean: String =
        name.chars().map(|c| if crate::util::is_unsafe(c) || matches!(c, '/' | '\\' | ':') { '_' } else { c }).collect();
    if clean.is_empty() || clean == "." || clean == ".." { "download".into() } else { clean }
}

/// `dir/name`, or `dir/name (1).ext`, `dir/name (2).ext`… so nothing is overwritten.
pub fn free_path(dir: &Path, name: &str) -> PathBuf {
    let path = dir.join(name);
    if !path.exists() {
        return path;
    }
    let (stem, ext) = match name.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() => (s, format!(".{e}")),
        _ => (name, String::new()),
    };
    (1..)
        .map(|n| dir.join(format!("{stem} ({n}){ext}")))
        .find(|p| !p.exists())
        .unwrap_or(path)
}

/// Downloads an object to `path`; a partial file is removed on failure.
pub fn download(aws: &Aws, bucket: &str, key: &str, path: &Path) -> Result<u64> {
    let result = aws.download(&SERVICE, &segments(bucket, key), path);
    if result.is_err() {
        let _ = std::fs::remove_file(path);
    }
    result
}

/// Console page of a bucket folder, or of an object.
pub fn console_url(region: &str, bucket: &str, key: &str, is_object: bool) -> String {
    let what = if is_object { "object" } else { "buckets" };
    format!(
        "https://s3.console.aws.amazon.com/s3/{what}/{bucket}?region={region}&prefix={}",
        crate::util::uri_encode(key)
    )
}

/// A missing object answers HEAD with a bare 404.
pub fn is_not_found(e: &AwsError) -> bool {
    matches!(e, AwsError::Api { status: 404, .. })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_uris_and_names() {
        assert_eq!(parse_uri("s3://bucket/a/b.txt"), Some(("bucket".into(), "a/b.txt".into())));
        assert_eq!(parse_uri("s3://bucket"), Some(("bucket".into(), String::new())));
        assert_eq!(parse_uri("bucket/dir/"), Some(("bucket".into(), "dir/".into())));
        assert_eq!(parse_uri("s3://"), None);
        assert_eq!(segments("b", "a/b c.txt"), ["b", "a", "b c.txt"]);
        assert_eq!(file_name("reports/2026/q3.csv"), "q3.csv");
        assert_eq!(file_name("weird/..\u{1b}"), ".._");
        assert_eq!(file_name("folder/"), "folder");
    }

    #[test]
    fn parses_bucket_and_object_listings() {
        let xml = r#"<ListAllMyBucketsResult><Owner><ID>x</ID></Owner><Buckets>
            <Bucket><Name>a.b-c</Name><CreationDate>2024-01-02T03:04:05.000Z</CreationDate><BucketRegion>eu-west-1</BucketRegion></Bucket>
            <Bucket><Name>plain</Name><CreationDate>2024-01-02T03:04:05.000Z</CreationDate></Bucket>
            </Buckets><ContinuationToken>tok</ContinuationToken></ListAllMyBucketsResult>"#;
        let buckets = parse_buckets(xml);
        assert_eq!(buckets.len(), 2);
        assert_eq!(buckets[0].region.as_deref(), Some("eu-west-1"));
        assert_eq!(buckets[1].region, None);
        assert_eq!(xml_text(xml, "ContinuationToken").as_deref(), Some("tok"));

        let xml = r#"<ListBucketResult><Name>b</Name><Prefix>docs/</Prefix><KeyCount>3</KeyCount>
            <IsTruncated>true</IsTruncated><NextContinuationToken>next1</NextContinuationToken>
            <Contents><Key>docs/</Key><Size>0</Size></Contents>
            <Contents><Key>docs/a &amp; b.txt</Key><LastModified>2026-10-19T12:00:00.000Z</LastModified><Size>1234</Size><StorageClass>STANDARD</StorageClass></Contents>
            <CommonPrefixes><Prefix>docs/old/</Prefix></CommonPrefixes></ListBucketResult>"#;
        let page = parse_page(xml, "docs/");
        assert_eq!(page.objects.len(), 1);
        assert_eq!(page.objects[0].key, "docs/a & b.txt");
        assert_eq!(page.objects[0].size, 1234);
        assert!(page.objects[0].modified.is_some());
        assert_eq!(page.folders, ["docs/old/"]);
        assert_eq!(page.next.as_deref(), Some("next1"));
        assert_eq!(parse_page("<ListBucketResult><IsTruncated>false</IsTruncated></ListBucketResult>", "").next, None);
    }

    #[test]
    fn regions_dates_and_previews() {
        assert_eq!(location_region("<LocationConstraint/>"), "us-east-1");
        assert_eq!(location_region("<LocationConstraint>EU</LocationConstraint>"), "eu-west-1");
        assert_eq!(location_region("<LocationConstraint>sa-east-1</LocationConstraint>"), "sa-east-1");
        let ns = r#"<?xml version="1.0"?><LocationConstraint xmlns="http://s3.amazonaws.com/doc/2006-03-01/">eu-central-1</LocationConstraint>"#;
        assert_eq!(location_region(ns), "eu-central-1");
        assert_eq!(location_region(r#"<LocationConstraint xmlns="x"/>"#), "us-east-1");
        assert_eq!(parse_http_date("Sun, 30 Aug 2015 12:36:00 GMT"), Some(1_440_938_160.0));
        let text = |b: &[u8], truncated| preview_text(&Preview { bytes: b.to_vec(), truncated });
        assert_eq!(text(b"hello", false).as_deref(), Some("hello"));
        assert_eq!(text(b"caf\xc3", true).as_deref(), Some("caf"));
        assert_eq!(text(b"\x89PNG\r\n\x1a\n\0\0", false), None);
        assert_eq!(text(b"a\xffb", false), None);
    }

    #[test]
    fn free_paths_do_not_overwrite() {
        let dir = std::env::temp_dir().join(format!("dcli-s3-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(free_path(&dir, "r.csv"), dir.join("r.csv"));
        std::fs::write(dir.join("r.csv"), "x").unwrap();
        std::fs::write(dir.join("r (1).csv"), "x").unwrap();
        assert_eq!(free_path(&dir, "r.csv"), dir.join("r (2).csv"));
        std::fs::write(dir.join("noext"), "x").unwrap();
        assert_eq!(free_path(&dir, "noext"), dir.join("noext (1)"));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
