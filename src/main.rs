mod aws;
mod cli;
mod config;
mod hooks;
mod modules;
mod pipeline;
mod platform;
mod report;
mod tui;
mod util;

use std::process::ExitCode;

use clap::{CommandFactory, FromArgMatches};

fn main() -> ExitCode {
    // Usage and help show the name the binary was invoked as.
    let matches = cli::Cli::command().bin_name(util::program()).get_matches();
    let args = cli::Cli::from_arg_matches(&matches).unwrap_or_else(|e| e.exit());
    match cli::run(args) {
        Ok(code) => code,
        Err(e) => {
            // Errors can quote AWS responses or a config file; keep them from driving the terminal.
            eprintln!("error: {}", util::sanitize(&format!("{e:#}")));
            if e.downcast_ref::<aws::AwsError>().is_some_and(aws::AwsError::is_login_required) {
                eprintln!("hint: run `{} login <env>` (or press L in the UI)", util::program());
            }
            ExitCode::FAILURE
        }
    }
}
