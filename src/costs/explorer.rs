//! Reading Cost Explorer. Every request is counted ([`super::settings::count_requests`]) and
//! sent at most once more, only when AWS throttled it: AWS bills each one.

use std::sync::Arc;

use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use super::settings;
use crate::aws::{self, Aws, ce};

/// Cost Explorer answers from us-east-1.
pub const REGION: &str = "us-east-1";

/// One Cost Explorer request, counted.
pub fn call<T: DeserializeOwned>(aws: &Aws, operation: &str, input: &Value) -> aws::Result<T> {
    settings::count_requests(1);
    aws.call_once(&ce::SERVICE, operation, input)
}

/// Who reads the costs of an environment whose AWS client is `env_aws`: the profile set in
/// the costs settings (the management account's), else the environment's own; in us-east-1.
pub fn billing(env_aws: &Aws) -> Arc<Aws> {
    match settings::load().profile.filter(|p| !p.is_empty()) {
        Some(profile) => Aws::new(&profile, REGION),
        None => env_aws.in_region(REGION),
    }
}

/// The filter that keeps `account`'s costs, for a reader that sees several (the management
/// account); `None` when the environment reads its own.
pub fn account_filter(account: &str) -> Option<Value> {
    settings::load()
        .profile
        .filter(|p| !p.is_empty())
        .map(|_| json!({ "Dimensions": { "Key": "LINKED_ACCOUNT", "Values": [account] } }))
}
