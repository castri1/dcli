//! Feature modules, one per screen. To add one (CloudWatch, ECS, Secrets Manager…):
//!   1. add the AWS calls in `src/aws/<service>.rs` (a `Service` constant + typed structs),
//!   2. implement `tui::module::Module` in `src/modules/<name>.rs`,
//!   3. register it below. The header and the `:` palette list modules in this order.

mod alarms;
mod apigateway;
mod appconfig;
mod approvals;
mod aurora;
mod cfn;
mod common;
mod costs;
mod cwlogs;
mod dms;
mod dynamodb;
mod events;
mod lambda;
mod matrix;
mod parameters;
mod pipelines;
mod redshift;
mod s3;
mod savings;
mod secrets;
mod sfn;
mod sfn_flow;
mod sqs;

use crate::tui::module::Module;

pub fn all() -> Vec<Box<dyn Module>> {
    vec![
        Box::new(pipelines::Pipelines::new()),
        Box::new(matrix::Matrix::new()),
        Box::new(approvals::Approvals::new()),
        Box::new(alarms::Alarms::new()),
        Box::new(costs::Costs::new()),
        Box::new(savings::Savings::new()),
        Box::new(parameters::Parameters::new()),
        Box::new(secrets::Secrets::new()),
        Box::new(lambda::Lambda::new()),
        Box::new(apigateway::ApiGateway::new()),
        Box::new(cwlogs::CwLogs::new()),
        Box::new(cfn::CloudFormation::new()),
        Box::new(s3::S3::new()),
        Box::new(sfn::StepFunctions::new()),
        Box::new(sqs::Sqs::new()),
        Box::new(dynamodb::DynamoDb::new()),
        Box::new(aurora::Aurora::new()),
        Box::new(redshift::Redshift::new()),
        Box::new(dms::Dms::new()),
        Box::new(events::EventBridge::new()),
        Box::new(appconfig::AppConfig::new()),
    ]
}
