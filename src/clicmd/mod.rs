//! One file per service command (`dcli s3`, `dcli sfn`, …), next to the shared helpers in
//! `cli.rs`. Each defines its clap `Args` and a `run`.

pub mod alarms;
pub mod apigw;
pub mod appconfig;
pub mod approvals;
pub mod aurora;
pub mod cfn;
pub mod costs;
pub mod dms;
pub mod dynamodb;
pub mod events;
pub mod insights;
pub mod lambda;
pub mod optimize;
pub mod promote;
pub mod redshift;
pub mod s3;
pub mod sfn;
pub mod sqs;
