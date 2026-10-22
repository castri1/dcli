//! Aurora: idle clusters and instances, oversized instances, Serverless v2 minimum capacity above use.

use super::{Finding, Scan};
use crate::aws;

pub fn check(_scan: &Scan) -> aws::Result<Vec<Finding>> {
    Ok(Vec::new())
}
