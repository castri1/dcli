//! Feature modules, one per screen. To add one (CloudWatch, ECS, Secrets Manager…):
//!   1. add the AWS calls in `src/aws/<service>.rs` (a `Service` constant + typed structs),
//!   2. implement `tui::module::Module` in `src/modules/<name>.rs`,
//!   3. register it below. The header and the `:` palette list modules in this order.

mod common;
mod cwlogs;
mod lambda;
mod parameters;
mod pipelines;
mod secrets;

use crate::tui::module::Module;

pub fn all() -> Vec<Box<dyn Module>> {
    vec![
        Box::new(pipelines::Pipelines::new()),
        Box::new(parameters::Parameters::new()),
        Box::new(secrets::Secrets::new()),
        Box::new(lambda::Lambda::new()),
        Box::new(cwlogs::CwLogs::new()),
    ]
}
