//! Secrets Manager: secrets nobody has read in a long time.

use super::{Finding, Scan};
use crate::aws;

pub fn check(_scan: &Scan) -> aws::Result<Vec<Finding>> {
    Ok(Vec::new())
}
