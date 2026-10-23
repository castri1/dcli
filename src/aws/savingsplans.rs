//! Savings Plans (restJson1, `POST /DescribeSavingsPlans`): the plans an account holds, their
//! commitment and end. A global service, signed for us-east-1; not billed per request.

use serde::Deserialize;
use serde_json::json;

use super::{Aws, Result, Service};

pub const SERVICE: Service = Service { endpoint: "savingsplans", signing_name: "savingsplans", target_prefix: "" };

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SavingsPlan {
    pub savings_plan_id: String,
    pub savings_plan_arn: String,
    /// `Compute`, `EC2Instance`, `SageMaker`, `Database`.
    pub savings_plan_type: String,
    /// Committed spend per hour, as text (`2.1`).
    pub commitment: String,
    pub currency: String,
    pub end: String,
    pub state: String,
    pub payment_option: String,
    /// `EC2`, `Fargate`, `Lambda`, `SageMaker`, `RDS`…
    pub product_types: Vec<String>,
}

/// The account's active Savings Plans.
pub fn active(aws: &Aws) -> Result<Vec<SavingsPlan>> {
    #[derive(Deserialize, Default)]
    #[serde(rename_all = "camelCase", default)]
    struct Out {
        savings_plans: Vec<SavingsPlan>,
        next_token: Option<String>,
    }
    let mut all = Vec::new();
    let mut token: Option<String> = None;
    for _ in 0..10 {
        let mut input = json!({ "states": ["active"], "maxResults": 100 });
        if let Some(t) = &token {
            input["nextToken"] = json!(t);
        }
        let out: Out = aws.rest(&SERVICE, "POST", &["DescribeSavingsPlans"], &[], Some(&input))?;
        all.extend(out.savings_plans);
        token = out.next_token.filter(|t| !t.is_empty());
        if token.is_none() {
            break;
        }
    }
    Ok(all)
}
