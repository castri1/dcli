//! `dcli approvals`: manual approvals waiting in every environment.

use std::process::ExitCode;

use anyhow::{Result, bail};
use serde_json::{Value, json};

use crate::aws::{Aws, AwsError};
use crate::cli::{paint, print_json};
use crate::config::Config;
use crate::pipeline::{self, Waiting};
use crate::util::{fmt_ago, program};

#[derive(clap::Args)]
pub struct Args {
    /// Only this environment (default: every one)
    env: Option<String>,
    #[arg(long)]
    json: bool,
}

pub fn run(config: &Config, args: Args) -> Result<ExitCode> {
    if let Some(env) = &args.env
        && !config.environments.contains_key(env)
    {
        bail!("no environment '{env}'");
    }
    let envs: Vec<&String> = config.environments.keys().filter(|e| args.env.as_ref().is_none_or(|only| only == *e)).collect();
    // Every pipeline in parallel; an expired session marks its environment, without prompting.
    type Scanned<'a> = (&'a String, String, Result<Vec<Waiting>, AwsError>);
    let results: Vec<Scanned> = std::thread::scope(|scope| {
        let handles: Vec<_> = envs
            .iter()
            .flat_map(|env| {
                let e = &config.environments[*env];
                let aws = Aws::new(&e.profile, &e.region);
                e.pipelines.iter().map(move |p| {
                    let (aws, name) = (aws.clone(), p.name.clone());
                    (*env, p.name.clone(), scope.spawn(move || pipeline::waiting_approvals(&aws, &name)))
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|(env, name, h)| (env, name, h.join().unwrap_or_else(|_| Err(AwsError::Network("panicked".into())))))
            .collect()
    });

    if args.json {
        let mut waiting = Vec::new();
        let mut errors = Vec::new();
        for (env, name, result) in &results {
            match result {
                Ok(list) => waiting.extend(list.iter().map(|w| to_json(env, w))),
                Err(e) => errors.push(json!({ "environment": env, "pipeline": name, "error": e.short(), "loginRequired": e.is_login_required() })),
            }
        }
        print_json(&json!({ "waiting": waiting, "errors": errors }));
        return Ok(ExitCode::SUCCESS);
    }

    let width = envs.iter().map(|e| e.chars().count()).max().unwrap_or(3);
    let mut count = 0;
    let mut login = Vec::new();
    for (env, name, result) in &results {
        let list = match result {
            Ok(list) => list,
            Err(e) if e.is_login_required() => {
                if !login.contains(env) {
                    login.push(*env);
                }
                continue;
            }
            Err(e) => {
                println!("{} {:<width$}  {name}: {}", paint("✗", "31"), env, paint(&e.short(), "31"));
                continue;
            }
        };
        for w in list {
            count += 1;
            let since = w.since.map(|t| format!("waiting {}", fmt_ago(t).replace(" ago", ""))).unwrap_or_default();
            println!("{} {}  {}  {} / {}  {}", paint("◆", "35"), paint(&format!("{env:<width$}"), "1"), w.pipeline, w.stage, w.action, paint(&since, "90"));
            if let Some(e) = &w.execution {
                println!("  {}", paint(&pipeline::revision(e, &[]).one_line(), "90"));
            }
            for line in w.custom_data.iter().flat_map(|d| d.lines()) {
                println!("  {line}");
            }
            if let Some(link) = &w.link {
                println!("  {}", paint(link, "36"));
            }
        }
    }
    if count == 0 {
        println!("Nothing waiting for approval.");
    } else {
        println!("\n{}", paint(&format!("Approve or reject: {} approve <env> <pipeline> [--reject] [-m comment]", program()), "90"));
    }
    if !login.is_empty() {
        let list: Vec<&str> = login.iter().map(|e| e.as_str()).collect();
        println!("{}", paint(&format!("Session expired for {}: {} login <env>", list.join(", "), program()), "33"));
    }
    Ok(ExitCode::SUCCESS)
}

fn to_json(env: &str, w: &Waiting) -> Value {
    let revision = w.execution.as_ref().map(|e| pipeline::revision(e, &[]));
    json!({
        "environment": env,
        "pipeline": w.pipeline,
        "stage": w.stage,
        "action": w.action,
        "since": w.since,
        "execution": w.execution.as_ref().map(|e| &e.pipeline_execution_id),
        "revision": revision.map(|r| json!({ "commit": r.commit, "message": r.message })),
        "customData": w.custom_data,
        "link": w.link,
    })
}
