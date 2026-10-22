//! `dcli apigw`: API Gateway APIs, their routes and stages, metrics, testing a route, exporting OpenAPI.

use std::process::ExitCode;

use anyhow::{Result, bail};

use crate::config::Config;

#[derive(clap::Args)]
#[allow(dead_code)] // filled in by the API Gateway work
pub struct Args {
    /// Environment (default: the current one, see `use`)
    env: Option<String>,
}

pub fn run(_config: &Config, _args: Args) -> Result<ExitCode> {
    bail!("dcli apigw is not built yet")
}
