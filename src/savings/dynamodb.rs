//! DynamoDB: provisioned capacity against consumed, tables without use, table class.

use super::{Finding, Scan};
use crate::aws;

pub fn check(_scan: &Scan) -> aws::Result<Vec<Finding>> {
    Ok(Vec::new())
}
