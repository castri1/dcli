//! One file per service command (`dcli s3`, `dcli sfn`, …), next to the shared helpers in
//! `cli.rs`. Each defines its clap `Args` and a `run`.

pub mod appconfig;
pub mod dynamodb;
pub mod events;
pub mod s3;
pub mod sfn;
