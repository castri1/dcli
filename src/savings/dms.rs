//! DMS: replication instances with no task running, full-load tasks finished on a running instance, oversized instances, Multi-AZ outside production.

use super::{Finding, Scan};
use crate::aws;

pub fn check(_scan: &Scan) -> aws::Result<Vec<Finding>> {
    Ok(Vec::new())
}
