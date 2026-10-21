//! `dcli promote` (deploy the commit an environment runs to the next one) and `dcli
//! rollback` (go back to the last commit that deployed successfully).

use std::process::ExitCode;

use anyhow::{Result, bail};

use crate::config::Config;

#[derive(clap::Args)]
#[allow(dead_code)] // filled in by the promote work
pub struct Args {
    /// Application (as in `dcli matrix`)
    app: Option<String>,
}

#[derive(clap::Args)]
#[allow(dead_code)] // filled in by the rollback work
pub struct RollbackArgs {
    /// Environment (default: the current one, see `use`)
    env: Option<String>,
    /// Pipeline
    pipeline: Option<String>,
}

pub fn run(_config: &Config, _args: Args) -> Result<ExitCode> {
    bail!("dcli promote is not built yet")
}

pub fn rollback(_config: &Config, _args: RollbackArgs) -> Result<ExitCode> {
    bail!("dcli rollback is not built yet")
}
