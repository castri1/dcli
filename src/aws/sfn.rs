//! Step Functions (awsJson1_0): state machines, executions, history, redrive/start/stop.

use super::Service;

pub const SERVICE: Service = Service { endpoint: "states", signing_name: "states", target_prefix: "AWSStepFunctions" };
