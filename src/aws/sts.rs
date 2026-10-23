//! STS (awsQuery protocol, API version 2011-06-15): the account a profile signs in to
//! (GetCallerIdentity, which needs no permission and is not billed), which says whose costs
//! are an environment's.

use super::{Aws, AwsError, Result, Service};
use crate::util::xml_text;

pub const SERVICE: Service = Service { endpoint: "sts", signing_name: "sts", target_prefix: "" };
pub const VERSION: &str = "2011-06-15";

/// The 12-digit id of the account `aws` signs in to.
pub fn caller_account(aws: &Aws) -> Result<String> {
    let xml = aws.query(&SERVICE, "GetCallerIdentity", VERSION, &[])?;
    xml_text(&xml, "Account")
        .filter(|a| is_account_id(a))
        .ok_or_else(|| AwsError::Network("STS did not say which account this profile is in".into()))
}

pub fn is_account_id(s: &str) -> bool {
    s.len() == 12 && s.chars().all(|c| c.is_ascii_digit())
}
