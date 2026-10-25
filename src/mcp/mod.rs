//! `dcli mcp`: a Model Context Protocol server on stdin/stdout, for Claude and other AI agents.
//! It reads an environment's account (inventory, configuration, metrics, the costs last read,
//! what Savings finds) and never changes it; the one thing it writes is recommendations, to
//! the Advisor ([`crate::advisor`]), which the user reviews and acts on.

use std::process::ExitCode;

use anyhow::Result;

use crate::config::Loaded;

#[derive(clap::Args)]
pub struct Args {
    /// Only these environments (repeat or separate with commas); default: all of them
    #[arg(long = "env", value_delimiter = ',', value_name = "ENV")]
    envs: Vec<String>,
}

pub fn serve(_loaded: &Loaded, _args: Args) -> Result<ExitCode> {
    anyhow::bail!("not implemented yet")
}
