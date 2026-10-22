//! `dcli aurora`: Aurora clusters, their topology and performance, top SQL, a report for an AI agent.

use std::process::ExitCode;

use anyhow::{Result, bail};

use crate::config::Config;

#[derive(clap::Args)]
#[allow(dead_code)] // filled in by the Aurora work
pub struct Args {
    /// Environment (default: the current one, see `use`)
    env: Option<String>,
}

pub fn run(_config: &Config, _args: Args) -> Result<ExitCode> {
    bail!("dcli aurora is not built yet")
}
