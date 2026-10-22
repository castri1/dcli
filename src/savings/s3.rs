//! S3: buckets without lifecycle rules, incomplete multipart uploads, old versions piling up.

use super::{Finding, Scan};
use crate::aws;

pub fn check(_scan: &Scan) -> aws::Result<Vec<Finding>> {
    Ok(Vec::new())
}
