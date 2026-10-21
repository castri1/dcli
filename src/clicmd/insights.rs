//! `dcli insights`: run a CloudWatch Logs Insights query.

use std::process::ExitCode;

use anyhow::{Result, bail};

use crate::config::Config;

#[derive(clap::Args)]
#[allow(dead_code)] // filled in by the insights work
pub struct Args {
    /// Environment (default: the current one, see `use`)
    env: Option<String>,
}

pub fn run(_config: &Config, _args: Args) -> Result<ExitCode> {
    bail!("dcli insights is not built yet")
}
