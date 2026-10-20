//! S3 (restXml, path-style): buckets, objects, previews, presigned URLs, downloads.

use super::Service;

pub const SERVICE: Service = Service { endpoint: "s3", signing_name: "s3", target_prefix: "" };
