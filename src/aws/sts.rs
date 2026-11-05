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

/// The IAM principal `aws` signs in as, as an EKS access entry names it: an assumed role's
/// role ARN (`arn:aws:iam::…:role/NAME`, without the path an SSO role has in IAM), else the
/// ARN STS gives (a user).
pub fn caller_principal(aws: &Aws) -> Result<String> {
    let xml = aws.query(&SERVICE, "GetCallerIdentity", VERSION, &[])?;
    let arn = xml_text(&xml, "Arn").ok_or_else(|| AwsError::Network("STS did not say who this profile signs in as".into()))?;
    Ok(principal(&arn))
}

/// `arn:aws:sts::111122223333:assumed-role/Admin/me` → `arn:aws:iam::111122223333:role/Admin`.
pub fn principal(arn: &str) -> String {
    let parts: Vec<&str> = arn.splitn(6, ':').collect();
    if let [_, partition, "sts", _, account, rest] = parts.as_slice()
        && let Some(role) = rest.strip_prefix("assumed-role/").and_then(|r| r.split('/').next())
    {
        return format!("arn:{partition}:iam::{account}:role/{role}");
    }
    arn.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assumed_roles_become_their_role() {
        assert_eq!(
            principal("arn:aws:sts::111122223333:assumed-role/AWSReservedSSO_Dev_0abc/me@example.com"),
            "arn:aws:iam::111122223333:role/AWSReservedSSO_Dev_0abc"
        );
        assert_eq!(principal("arn:aws:iam::111122223333:user/ci"), "arn:aws:iam::111122223333:user/ci");
    }
}
