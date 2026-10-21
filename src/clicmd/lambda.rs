//! `dcli lambda`: a function's configuration.

use std::process::ExitCode;

use anyhow::{Result, bail};

use crate::aws::{Aws, lambda};
use crate::cli::{aws_for, env_and, paint, print_json};
use crate::config::Config;
use crate::util::program;

#[derive(clap::Args)]
pub struct Args {
    /// Environment (default: the current one, see `use`)
    env: Option<String>,
    /// Function name
    name: Option<String>,
    #[arg(long)]
    json: bool,
}

pub fn run(config: &Config, args: Args) -> Result<ExitCode> {
    let (env, Some(name)) = env_and(config, args.env, args.name)? else { bail!("which function? {} lambda [ENV] NAME", program()) };
    show(aws_for(config, &env)?.as_ref(), &name, args.json)
}

fn show(aws: &Aws, name: &str, json: bool) -> Result<ExitCode> {
    let f = lambda::get_function(aws, name)?;
    if json {
        print_json(&serde_json::to_value(&f)?);
        return Ok(ExitCode::SUCCESS);
    }
    let c = &f.configuration;
    let row = |k: &str, v: String| println!("  {:<14} {v}", paint(k, "90"));
    println!("{}", paint(&c.function_name, "1"));
    row("ARN", c.function_arn.clone());
    row("Runtime", c.runtime.clone().unwrap_or_else(|| "—".into()));
    row("Handler", c.handler.clone().unwrap_or_default());
    row("Memory", c.memory_size.map(|m| format!("{m} MB")).unwrap_or_default());
    row("Timeout", c.timeout.map(|t| format!("{t} s")).unwrap_or_default());
    row("Architecture", c.architectures.join(", "));
    row("State", c.state.clone().unwrap_or_default());
    row("Modified", c.last_modified.clone().unwrap_or_default());
    row("Role", c.role.clone().unwrap_or_default());
    if let Some(env) = &c.environment {
        row("Env vars", env.variables.keys().cloned().collect::<Vec<_>>().join(", "));
    }
    Ok(ExitCode::SUCCESS)
}
