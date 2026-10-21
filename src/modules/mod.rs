//! Feature modules, one per screen. To add one (CloudWatch, ECS, Secrets Manager…):
//!   1. add the AWS calls in `src/aws/<service>.rs` (a `Service` constant + typed structs),
//!   2. implement `tui::module::Module` in `src/modules/<name>.rs`,
//!   3. register it below. The header and the `:` palette list modules in this order.

mod appconfig;
mod common;
mod cwlogs;
mod dynamodb;
mod events;
mod lambda;
mod matrix;
mod parameters;
mod pipelines;
mod s3;
mod secrets;
mod sfn;
mod sfn_flow;

use crate::tui::module::Module;

pub fn all() -> Vec<Box<dyn Module>> {
    vec![
        Box::new(pipelines::Pipelines::new()),
        Box::new(matrix::Matrix::new()),
        Box::new(parameters::Parameters::new()),
        Box::new(secrets::Secrets::new()),
        Box::new(lambda::Lambda::new()),
        Box::new(cwlogs::CwLogs::new()),
        Box::new(s3::S3::new()),
        Box::new(sfn::StepFunctions::new()),
        Box::new(dynamodb::DynamoDb::new()),
        Box::new(events::EventBridge::new()),
        Box::new(appconfig::AppConfig::new()),
    ]
}
