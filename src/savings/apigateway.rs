//! API Gateway: REST APIs that use nothing only REST has (HTTP API candidates), caches that rarely hit.

use super::{Finding, Scan};
use crate::aws;

pub fn check(_scan: &Scan) -> aws::Result<Vec<Finding>> {
    Ok(Vec::new())
}
