//! Savings Plans (restJson1, `POST /DescribeSavingsPlans`): the plans an account holds, their
//! commitment and end. A global service, signed for us-east-1.

use super::Service;

#[allow(dead_code)] // used by the costs work
pub const SERVICE: Service = Service { endpoint: "savingsplans", signing_name: "savingsplans", target_prefix: "" };
