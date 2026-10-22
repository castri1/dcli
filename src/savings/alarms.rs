//! CloudWatch alarms: alarms without data for weeks (watching something that is gone).

use super::{Finding, Scan};
use crate::aws;

pub fn check(_scan: &Scan) -> aws::Result<Vec<Finding>> {
    Ok(Vec::new())
}
