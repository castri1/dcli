//! Lambda: memory assigned against memory used, x86 functions that could run on arm64, idle provisioned concurrency.

use super::{Finding, Scan};
use crate::aws;

pub fn check(_scan: &Scan) -> aws::Result<Vec<Finding>> {
    Ok(Vec::new())
}
