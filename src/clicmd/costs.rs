//! `dcli costs`: what an environment's account costs, by service and usage type, commitments
//! and their coverage, from Cost Explorer (each request billed; results kept on disk).

use std::process::ExitCode;

use anyhow::{Result, bail};

use crate::config::Config;

#[derive(clap::Args)]
#[allow(dead_code)] // filled in by the costs work
pub struct Args {
    /// Environment (default: the current one, see `use`)
    env: Option<String>,
}

pub fn run(_config: &Config, _args: Args) -> Result<ExitCode> {
    bail!("dcli costs is not built yet")
}
