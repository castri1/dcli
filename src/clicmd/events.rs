//! `dcli events`: buses and rules, send a test event.

use std::process::ExitCode;

use anyhow::{Result, bail};

use crate::config::Config;

#[derive(clap::Args)]
pub struct Args {
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    rest: Vec<String>,
}

pub fn run(_config: &Config, _args: Args) -> Result<ExitCode> {
    bail!("not implemented yet")
}
