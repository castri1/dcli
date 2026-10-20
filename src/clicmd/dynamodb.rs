//! `dcli ddb`: describe a table, query it by key, or scan it. Items print as plain JSON,
//! one per line (or a JSON array with `--json`).

use std::process::ExitCode;

use anyhow::{Result, bail};
use serde_json::Value;

use crate::aws::dynamodb::{self as ddb, KeyQuery, SortCondition, TableDescription};
use crate::cli::{aws_for, env_and, paint, print_json};
use crate::config::Config;
use crate::util::{fmt_bytes, program};

#[derive(clap::Args)]
pub struct Args {
    /// Environment (default: the current one, see `use`)
    env: Option<String>,
    /// Table name
    table: Option<String>,
    /// Partition key value: read the items with this key (Query, or GetItem for a whole key)
    #[arg(long, value_name = "VALUE", allow_hyphen_values = true)]
    pk: Option<String>,
    /// Sort key condition: =:x, begins_with:2026-, between:a,b, <:x, <=:x, >:x, >=:x
    #[arg(long, value_name = "OP:VALUE", allow_hyphen_values = true)]
    sk: Option<String>,
    /// Query this index (GSI or LSI) instead of the table
    #[arg(long, value_name = "NAME")]
    index: Option<String>,
    /// Read the whole table, page by page (up to --limit items)
    #[arg(long, conflicts_with_all = ["pk", "sk", "index"])]
    scan: bool,
    /// Items to print at most
    #[arg(long, default_value_t = 100)]
    limit: usize,
    /// Print a JSON array (or the table description as JSON)
    #[arg(long)]
    json: bool,
}

pub fn run(config: &Config, args: Args) -> Result<ExitCode> {
    let (env, Some(table)) = env_and(config, args.env, args.table)? else {
        bail!("which table? {} ddb [ENV] TABLE [--pk VALUE [--sk OP:VALUE]] [--scan]", program())
    };
    let aws = aws_for(config, &env)?;
    let desc = ddb::describe_table(&aws, &table)?;

    let query = match (&args.pk, args.scan) {
        (Some(pk), _) => {
            if let Some(i) = &args.index
                && desc.keys(Some(i)).is_none()
            {
                let names: Vec<&str> = desc.indexes().map(|(_, i)| i.index_name.as_str()).collect();
                bail!("{table} has no index '{i}'. Indexes: {}", if names.is_empty() { "none".into() } else { names.join(", ") });
            }
            let sort = args.sk.as_deref().map(SortCondition::parse).transpose().map_err(anyhow::Error::msg)?;
            if sort.is_some() && desc.keys(args.index.as_deref()).and_then(|k| k.sort).is_none() {
                bail!("--sk: that key has no sort key");
            }
            Some(KeyQuery { index: args.index.clone(), partition: pk.clone(), sort })
        }
        (None, true) => None,
        (None, false) => {
            if args.sk.is_some() || args.index.is_some() {
                bail!("--sk and --index need --pk");
            }
            return describe(&desc, args.json);
        }
    };

    let mut items: Vec<Value> = Vec::new();
    let mut start: Option<Value> = None;
    let more = loop {
        let want = (args.limit - items.len()).clamp(1, 1000) as u32;
        let page = match &query {
            Some(q) => ddb::query(&aws, &desc, q, want, start.as_ref())?,
            None => ddb::scan(&aws, &table, want, start.as_ref())?,
        };
        items.extend(page.items);
        start = page.next;
        if start.is_none() {
            break false;
        }
        if items.len() >= args.limit {
            break true;
        }
    };
    items.truncate(args.limit);

    if args.json {
        print_json(&Value::Array(items));
    } else {
        for item in &items {
            println!("{item}");
        }
    }
    if more {
        eprintln!("{}", paint(&format!("… more items: raise --limit (now {})", args.limit), "90"));
    }
    Ok(ExitCode::SUCCESS)
}

fn describe(desc: &TableDescription, json: bool) -> Result<ExitCode> {
    if json {
        print_json(&serde_json::to_value(desc)?);
        return Ok(ExitCode::SUCCESS);
    }
    let row = |k: &str, v: String| println!("  {:<10} {v}", paint(k, "90"));
    println!("{}", paint(&desc.table_name, "1"));
    row("Keys", desc.key_label(None));
    row("Items", desc.item_count.map(|n| n.to_string()).unwrap_or_else(|| "—".into()));
    row("Size", desc.table_size_bytes.map(fmt_bytes).unwrap_or_else(|| "—".into()));
    row("Billing", desc.billing());
    row("Status", desc.table_status.clone().unwrap_or_default());
    for (kind, i) in desc.indexes() {
        let projection = i.projection.as_ref().and_then(|p| p.projection_type.clone()).unwrap_or_default();
        row(kind, format!("{}  {} · {projection}", i.index_name, desc.key_label(Some(&i.index_name))));
    }
    println!(
        "\n{}",
        paint(
            &format!("Query: {} ddb {} --pk VALUE [--sk begins_with:x] [--index NAME]   Scan: --scan", program(), desc.table_name),
            "90"
        )
    );
    Ok(ExitCode::SUCCESS)
}
