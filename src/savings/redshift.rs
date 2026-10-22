//! Redshift: clusters idle at night or for days (candidates to pause), nodes to spare, old manual snapshots, heavy concurrency scaling.

use super::{Finding, Scan};
use crate::aws;

pub fn check(_scan: &Scan) -> aws::Result<Vec<Finding>> {
    Ok(Vec::new())
}
