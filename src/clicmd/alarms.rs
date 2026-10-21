//! `dcli alarms`: CloudWatch alarms of an environment (or all of them), those firing first.
//! Exits with 1 when any listed alarm is firing, so scripts can check it.

use std::process::ExitCode;

use anyhow::Result;
use serde_json::json;

use crate::aws::Aws;
use crate::aws::alarms::{self, Alarm, State};
use crate::cli::{aws_for, current_env, paint, print_json};
use crate::config::Config;
use crate::util::{fmt_ago, rfc3339, show_invisible};

#[derive(clap::Args)]
pub struct Args {
    /// Environment (default: the current one, see `use`)
    env: Option<String>,
    /// Every environment in the config
    #[arg(long, conflicts_with = "env")]
    all_envs: bool,
    /// Only alarms in this state
    #[arg(long, value_parser = ["alarm", "insufficient", "ok"])]
    state: Option<String>,
    #[arg(long)]
    json: bool,
}

pub fn run(config: &Config, args: Args) -> Result<ExitCode> {
    let envs: Vec<String> = match (&args.env, args.all_envs) {
        (_, true) => config.environments.keys().cloned().collect(),
        (Some(env), false) => vec![env.clone()],
        (None, false) => vec![current_env(config)?],
    };
    let mut rows: Vec<(String, Alarm)> = Vec::new();
    if let [env] = envs.as_slice() {
        let aws = aws_for(config, env)?;
        rows.extend(alarms::describe_alarms(&aws)?.into_iter().map(|a| (env.clone(), a)));
    } else {
        // Every environment at once; one with an expired session is reported and skipped.
        let results: Vec<(String, crate::aws::Result<Vec<Alarm>>)> = std::thread::scope(|s| {
            let jobs: Vec<_> = envs
                .iter()
                .map(|name| {
                    s.spawn(move || {
                        let result = config.env(name).map_err(|e| crate::aws::AwsError::Config(e.to_string())).and_then(|e| {
                            let aws = Aws::new(&e.profile, &e.region);
                            alarms::describe_alarms(&aws)
                        });
                        (name.clone(), result)
                    })
                })
                .collect();
            jobs.into_iter().filter_map(|j| j.join().ok()).collect()
        });
        for (env, result) in results {
            match result {
                Ok(list) => rows.extend(list.into_iter().map(|a| (env.clone(), a))),
                Err(e) if e.is_login_required() => {
                    eprintln!("{}", paint(&format!("! {env}: the AWS session expired; sign in with: dcli login {env}"), "33"))
                }
                Err(e) => eprintln!("{}", paint(&format!("! {env}: {}", e.short()), "33")),
            }
        }
    }
    if let Some(wanted) = &args.state {
        let wanted = match wanted.as_str() {
            "alarm" => State::Alarm,
            "ok" => State::Ok,
            _ => State::InsufficientData,
        };
        rows.retain(|(_, a)| a.state == wanted);
    }
    // Firing first, the most recent first; then the rest by name, environments in config order.
    let order = |env: &str| envs.iter().position(|e| e == env);
    rows.sort_by(|(ea, a), (eb, b)| {
        a.state.cmp(&b.state).then_with(|| match a.state {
            State::Alarm => b.since.unwrap_or(0.0).total_cmp(&a.since.unwrap_or(0.0)),
            _ => a.name.cmp(&b.name).then_with(|| order(ea).cmp(&order(eb))),
        })
    });
    let firing = rows.iter().filter(|(_, a)| a.state == State::Alarm).count();
    let code = if firing > 0 { ExitCode::from(1) } else { ExitCode::SUCCESS };

    if args.json {
        let list: Vec<serde_json::Value> = rows
            .iter()
            .map(|(env, a)| {
                json!({
                    "env": env,
                    "name": a.name,
                    "state": a.state,
                    "since": a.since.map(|t| rfc3339(t as i64)),
                    "condition": a.condition(),
                    "reason": a.reason,
                    "namespace": a.graphable().map(|m| m.namespace.clone()),
                    "metric": a.graphable().map(|m| m.name.clone()),
                    "dimensions": a.dimensions().iter().map(|(k, v)| json!({ "name": k, "value": v })).collect::<Vec<_>>(),
                    "composite": a.composite,
                    "arn": a.arn,
                })
            })
            .collect();
        print_json(&serde_json::Value::Array(list));
        return Ok(code);
    }
    if rows.is_empty() {
        println!("No alarms{}.", args.state.as_ref().map(|s| format!(" in state {s}")).unwrap_or_default());
        return Ok(code);
    }
    let many = envs.len() > 1;
    let name_w = rows.iter().map(|(_, a)| show_invisible(&a.name).chars().count()).max().unwrap_or(4).min(48);
    let env_w = if many { rows.iter().map(|(e, _)| e.chars().count()).max().unwrap_or(3) + 2 } else { 0 };
    let cond_w = rows.iter().map(|(_, a)| a.condition().chars().count()).max().unwrap_or(10).min(60);
    for (env, a) in &rows {
        let (label, color) = match a.state {
            State::Alarm => ("● ALARM", "31;1"),
            State::InsufficientData => ("◌ INSUFFICIENT", "33"),
            State::Ok => ("✓ OK", "32"),
        };
        let since = a.since.map(fmt_ago).unwrap_or_default();
        let env = if many { format!("{env:<env_w$}") } else { String::new() };
        println!(
            "{}  {:<name_w$}  {env}{:<cond_w$}  {}",
            paint(&format!("{label:<14}"), color),
            show_invisible(&a.name),
            a.condition(),
            paint(&since, "90")
        );
    }
    let insufficient = rows.iter().filter(|(_, a)| a.state == State::InsufficientData).count();
    let ok = rows.len() - firing - insufficient;
    println!("{}", paint(&format!("{firing} firing · {insufficient} insufficient data · {ok} ok"), "90"));
    Ok(code)
}
