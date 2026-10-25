//! `dcli advisor`: the recommendations Claude left through `dcli mcp` for an environment, with
//! what each saves or costs at the bill's rates. `--md` for a Markdown document, `--json` for
//! scripts; `--done`, `--dismiss` and `--restore` record the user's decision.

use std::process::ExitCode;

use anyhow::Result;

use crate::config::Config;

#[derive(clap::Args)]
pub struct Args {
    /// Environment (default: the current one, see `use`)
    env: Option<String>,
}

pub fn run(_config: &Config, _args: Args) -> Result<ExitCode> {
    anyhow::bail!("not implemented yet")
}
