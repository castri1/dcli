//! Cost savings: checks that look at an environment's resources and their usage and say
//! what could cost less. There are no prices here (they change, and dcli would have to be
//! released again): a finding says how much usage it removes, in the unit AWS bills for it
//! (GB-seconds, GB stored, requests, capacity units, instance hours), and how large that is.
//!
//! Each service has a file with a [`Check`]; [`CHECKS`] runs them. Findings are built with
//! [`Finding::new`] and its builder methods, never with a struct literal, so fields can be
//! added without touching every check.

pub mod alarms;
pub mod apigateway;
pub mod aurora;
pub mod dynamodb;
pub mod lambda;
pub mod logs;
pub mod s3;
pub mod secrets;
pub mod sfn;
pub mod sqs;

use crate::aws::{self, Aws};

/// How much a finding matters, from the volume of usage it removes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[allow(dead_code)] // used by the checks
pub enum Impact {
    Low,
    Medium,
    High,
}

#[derive(Debug, Clone)]
#[allow(dead_code)] // used by the Savings module and `dcli optimize`
pub struct Finding {
    /// The title of the module that opens the resource, and what its `open` takes (as in
    /// [`crate::catalog`]).
    pub module: &'static str,
    pub target: String,
    /// The resource as shown: a function, log group, table, cluster…
    pub resource: String,
    /// What was found, in one line: "uses 410 of 3008 MB (p99)".
    pub summary: String,
    /// What to change: "set the memory to 512 MB".
    pub recommendation: String,
    /// The usage it removes, in the unit AWS bills: "−83 % GB-s (4.1 M GB-s a month)".
    pub reduction: String,
    pub impact: Impact,
    /// The measurements behind it.
    pub evidence: Vec<String>,
    /// What the estimate assumes or could get wrong.
    pub caveats: Vec<String>,
}

#[allow(dead_code)] // used by the checks
impl Finding {
    pub fn new(
        module: &'static str,
        target: impl Into<String>,
        resource: impl Into<String>,
        summary: impl Into<String>,
        recommendation: impl Into<String>,
        reduction: impl Into<String>,
        impact: Impact,
    ) -> Finding {
        Finding {
            module,
            target: target.into(),
            resource: resource.into(),
            summary: summary.into(),
            recommendation: recommendation.into(),
            reduction: reduction.into(),
            impact,
            evidence: Vec::new(),
            caveats: Vec::new(),
        }
    }

    pub fn evidence(mut self, line: impl Into<String>) -> Finding {
        self.evidence.push(line.into());
        self
    }

    pub fn caveat(mut self, line: impl Into<String>) -> Finding {
        self.caveats.push(line.into());
        self
    }
}

/// What every check is given.
#[allow(dead_code)] // used by the checks
pub struct Scan<'a> {
    pub aws: &'a Aws,
    /// Days of usage looked at.
    pub days: u32,
}

/// Looks at one service. An error means the service could not be read (no permission,
/// expired session); the other checks still run.
pub type Check = fn(&Scan) -> aws::Result<Vec<Finding>>;

/// Every check, with the service it looks at.
#[allow(dead_code)] // used by the Savings module and `dcli optimize`
pub const CHECKS: &[(&str, Check)] = &[
    ("Lambda", lambda::check),
    ("CloudWatch Logs", logs::check),
    ("SQS", sqs::check),
    ("DynamoDB", dynamodb::check),
    ("S3", s3::check),
    ("Step Functions", sfn::check),
    ("Secrets Manager", secrets::check),
    ("CloudWatch alarms", alarms::check),
    ("API Gateway", apigateway::check),
    ("Aurora", aurora::check),
];
