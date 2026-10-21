//! `dcli insights`: run a CloudWatch Logs Insights query over log groups, wait for it, and
//! print the results as a table (or JSON, CSV).

use std::io::IsTerminal;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, bail};
use serde_json::{Map, Value};

use crate::aws::logs::{self, Table};
use crate::cli::{aws_for, current_env, paint, print_json};
use crate::config::Config;
use crate::util::{fmt_duration, now_secs, program, sanitize};

#[derive(clap::Args)]
pub struct Args {
    /// [ENV] then the log groups (the environment defaults to the current one)
    #[arg(value_name = "ENV|GROUP")]
    targets: Vec<String>,
    /// The query, in the Logs Insights language: 'fields @timestamp, @message | filter @message like /ERROR/'
    #[arg(short, long, allow_hyphen_values = true)]
    query: Option<String>,
    /// Run a saved query by name (with its log groups, unless some are given)
    #[arg(long, conflicts_with = "query")]
    saved: Option<String>,
    /// How far back to look: 30m, 2h, 1d, 1w
    #[arg(long, default_value = "1h")]
    since: String,
    /// Rows to return at most (Insights returns 10,000 at most)
    #[arg(long)]
    limit: Option<u32>,
    /// Print a JSON array of objects (values keep their line breaks)
    #[arg(long, conflicts_with = "csv")]
    json: bool,
    #[arg(long)]
    csv: bool,
}

pub fn run(config: &Config, args: Args) -> Result<ExitCode> {
    let mut targets = args.targets;
    let env = match targets.first() {
        Some(t) if config.environments.contains_key(t) => targets.remove(0),
        _ => current_env(config)?,
    };
    let aws = aws_for(config, &env)?;
    let (query, language, saved_groups) = match (args.query, &args.saved) {
        (Some(q), _) => (q, None, Vec::new()),
        (None, Some(name)) => {
            let saved = logs::query_definitions(&aws)?;
            let lower = name.to_lowercase();
            let partial: Vec<&logs::QueryDefinition> = saved.iter().filter(|d| d.name.to_lowercase().contains(&lower)).collect();
            let def = saved.iter().find(|d| &d.name == name).or(if partial.len() == 1 { partial.first().copied() } else { None });
            let Some(def) = def else {
                let names: Vec<&str> = saved.iter().map(|d| d.name.as_str()).collect();
                bail!("no saved query named '{name}'. Saved queries: {}", if names.is_empty() { "none".into() } else { names.join(", ") });
            };
            (def.query_string.clone(), def.query_language.clone(), def.log_group_names.clone())
        }
        (None, None) => bail!("what query? {} insights [ENV] GROUP... --query '…' (or --saved NAME)", program()),
    };
    let groups = if targets.is_empty() { saved_groups } else { targets };
    if groups.is_empty() {
        bail!("which log groups? {} insights [ENV] GROUP... --query '…'", program());
    }
    let secs = logs::parse_since(&args.since).ok_or_else(|| anyhow!("--since takes 30m, 2h, 1d…"))?;
    let end = now_secs() as i64;
    let id = logs::start_query(&aws, &groups, (end - secs, end), &query, language.as_deref(), args.limit)?;

    // Partial results arrive while it runs; a terminal sees the progress on stderr.
    let progress = std::io::stderr().is_terminal();
    let began = Instant::now();
    let mut wait = Duration::from_millis(400);
    let results = loop {
        std::thread::sleep(wait);
        wait = Duration::from_secs(1);
        let r = logs::get_query_results(&aws, &id)?;
        if !r.running() {
            break r;
        }
        if progress {
            eprint!("\r\u{1b}[2KRunning · {} · {}", r.summary(), fmt_duration(began.elapsed().as_secs_f64()));
        }
    };
    if progress {
        eprint!("\r\u{1b}[2K");
    }
    if results.status != "Complete" {
        bail!("the query ended {} ({})", results.status.to_lowercase(), results.summary());
    }

    let table = Table::new(&results.results);
    if args.json {
        let rows = results
            .results
            .iter()
            .map(|row| {
                let fields = row.iter().filter(|f| f.field != "@ptr").map(|f| (sanitize(&f.field), Value::String(sanitize(&f.value))));
                Value::Object(fields.collect::<Map<_, _>>())
            })
            .collect();
        print_json(&Value::Array(rows));
    } else if args.csv {
        print!("{}", table.csv());
    } else {
        print_table(&table);
    }
    let took = fmt_duration(began.elapsed().as_secs_f64());
    eprintln!("{}", paint(&format!("{} · {} · {took}", logs::rows(table.rows.len()), results.summary()), "90"));
    Ok(ExitCode::SUCCESS)
}

/// Columns padded to their widest value (at most 60, the last one whole).
fn print_table(table: &Table) {
    if table.columns.is_empty() {
        println!("No results.");
        return;
    }
    let last = table.columns.len() - 1;
    let widths: Vec<usize> = (0..table.columns.len())
        .map(|c| table.rows.iter().map(|r| r[c].chars().count()).chain([table.columns[c].chars().count()]).max().unwrap_or(0).min(60))
        .collect();
    let line = |values: &[String]| {
        let cells: Vec<String> = values
            .iter()
            .enumerate()
            .map(|(c, v)| if c == last { v.clone() } else { format!("{:<w$}", crate::util::ellipsize(v, widths[c]), w = widths[c]) })
            .collect();
        cells.join("  ").trim_end().to_string()
    };
    println!("{}", paint(&line(&table.columns), "1"));
    for row in &table.rows {
        println!("{}", line(row));
    }
}
