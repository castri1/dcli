//! `dcli sqs`: queues with their counts and dead-letter queues, the messages of a
//! dead-letter queue (`peek`), and `redrive` back to the source queues.

use std::process::ExitCode;
use std::time::Duration;

use anyhow::{Result, bail};
use clap::Subcommand;
use serde_json::{Value, json};

use crate::aws::Aws;
use crate::aws::sqs::{self, Queue};
use crate::cli::{aws_for, confirm_write, env_and, paint, print_json};
use crate::config::Config;
use crate::util::{fmt_ago, fmt_utc, program, sanitize};

#[derive(clap::Args)]
#[command(args_conflicts_with_subcommands = true)]
pub struct Args {
    #[command(subcommand)]
    action: Option<Action>,
    /// Environment (default: the current one, see `use`)
    env: Option<String>,
    /// Queue name or URL; without it, lists the queues
    queue: Option<String>,
    #[arg(long)]
    json: bool,
}

#[derive(Subcommand)]
enum Action {
    /// Show the messages of a dead-letter queue without consuming them (DLQs only)
    Peek {
        env: Option<String>,
        /// Dead-letter queue name or URL
        queue: Option<String>,
        /// How many messages at most
        #[arg(short = 'n', long, default_value_t = 10)]
        limit: usize,
        #[arg(long)]
        json: bool,
    },
    /// Move the messages of a dead-letter queue back to their source queues (or --to another)
    Redrive {
        env: Option<String>,
        /// Dead-letter queue name or URL
        queue: Option<String>,
        /// Destination queue (default: the queues the messages came from)
        #[arg(long, value_name = "QUEUE")]
        to: Option<String>,
        /// Do not ask for confirmation
        #[arg(short, long)]
        yes: bool,
    },
}

pub fn run(config: &Config, args: Args) -> Result<ExitCode> {
    match args.action {
        Some(Action::Peek { env, queue, limit, json }) => {
            let (env, Some(queue)) = env_and(config, env, queue)? else { bail!("which queue? {} sqs peek [ENV] DLQ", program()) };
            peek(aws_for(config, &env)?.as_ref(), &queue, limit, json)
        }
        Some(Action::Redrive { env, queue, to, yes }) => {
            let (env, Some(queue)) = env_and(config, env, queue)? else { bail!("which queue? {} sqs redrive [ENV] DLQ", program()) };
            redrive(aws_for(config, &env)?.as_ref(), &env, &queue, to.as_deref(), yes)
        }
        None => {
            let (env, queue) = env_and(config, args.env, args.queue)?;
            let aws = aws_for(config, &env)?;
            match queue {
                Some(q) => show(&aws, &q, args.json),
                None => list(&aws, &env, args.json),
            }
        }
    }
}

fn queue_json(q: &Queue) -> Value {
    json!({
        "name": q.name,
        "url": q.url,
        "arn": q.arn(),
        "fifo": q.fifo(),
        "visible": q.visible(),
        "inFlight": q.in_flight(),
        "delayed": q.delayed(),
        "deadLetterQueue": q.redrive().map(|r| json!({ "arn": r.dead_letter_arn, "maxReceiveCount": r.max_receive_count })),
        "attributes": q.attributes,
    })
}

fn list(aws: &Aws, env: &str, json: bool) -> Result<ExitCode> {
    let queues = sqs::list_queues(aws)?;
    if json {
        print_json(&Value::Array(queues.iter().map(queue_json).collect()));
        return Ok(ExitCode::SUCCESS);
    }
    if queues.is_empty() {
        println!("No queues in {env}.");
        return Ok(ExitCode::SUCCESS);
    }
    let is_dlq = |q: &Queue| queues.iter().any(|s| s.redrive().is_some_and(|r| r.dead_letter_arn == q.arn()));
    let width = queues.iter().map(|q| q.name.chars().count()).max().unwrap_or(4) + 2;
    println!("{}", paint(&format!("{:<width$}{:>9} {:>10} {:>8}", "queue", "visible", "in flight", "delayed"), "90"));
    for q in &queues {
        let dlq = is_dlq(q);
        let visible = format!("{:>9}", q.visible());
        let visible = if dlq && q.visible() > 0 { paint(&visible, "31;1") } else { visible };
        let link = match q.redrive() {
            Some(r) => paint(&format!("  → {} after {} receives", sqs::queue_name(&r.dead_letter_arn), r.max_receive_count), "90"),
            None if dlq => paint("  dead-letter queue", "35"),
            None => String::new(),
        };
        println!("{:<width$}{visible} {:>10} {:>8}{link}", q.name, q.in_flight(), q.delayed());
    }
    Ok(ExitCode::SUCCESS)
}

fn show(aws: &Aws, id: &str, json: bool) -> Result<ExitCode> {
    let q = sqs::queue(aws, &sqs::resolve(aws, id)?)?;
    let sources = sqs::dead_letter_sources(aws, &q.url)?;
    if json {
        let mut v = queue_json(&q);
        v["deadLetterSources"] = json!(sources);
        print_json(&v);
        return Ok(ExitCode::SUCCESS);
    }
    let row = |k: &str, v: String| println!("  {:<14} {v}", paint(k, "90"));
    println!("{}", paint(&q.name, "1"));
    row("Messages", format!("{} visible · {} in flight · {} delayed", q.visible(), q.in_flight(), q.delayed()));
    row("Type", if q.fifo() { "FIFO".into() } else { "Standard".into() });
    if let Some(v) = q.attr("MessageRetentionPeriod") {
        row("Retention", sqs::fmt_seconds(v));
    }
    if let Some(v) = q.attr("VisibilityTimeout") {
        row("Visibility", sqs::fmt_seconds(v));
    }
    row("Encryption", q.encryption());
    row("URL", q.url.clone());
    row("ARN", q.arn().to_string());
    if let Some(r) = q.redrive() {
        row("DLQ", format!("{} after {} receives", sqs::queue_name(&r.dead_letter_arn), r.max_receive_count));
    }
    if !sources.is_empty() {
        let names: Vec<&str> = sources.iter().map(|s| sqs::queue_name(s)).collect();
        row("DLQ of", names.join(", "));
        println!(
            "{}",
            paint(&format!("Its messages: {} sqs peek {} · send them back: {} sqs redrive {}", program(), q.name, program(), q.name), "90")
        );
    }
    Ok(ExitCode::SUCCESS)
}

/// The queue and the queues that send their failures to it; refuses queues that are not
/// dead-letter queues.
fn dead_letter_queue(aws: &Aws, id: &str, reading: bool) -> Result<(Queue, Vec<String>)> {
    let q = sqs::queue(aws, &sqs::resolve(aws, id)?)?;
    let sources = sqs::dead_letter_sources(aws, &q.url)?;
    if sources.is_empty() {
        if !reading {
            bail!("{} is not a dead-letter queue: no queue sends its failed messages to it, so there is nowhere to redrive them", q.name);
        }
        let why = match q.redrive() {
            Some(r) => format!(" and could move them to {}", sqs::queue_name(&r.dead_letter_arn)),
            None => String::new(),
        };
        bail!(
            "{} is not a dead-letter queue: dcli only reads the messages of dead-letter queues, because reading one counts as a receive{why}",
            q.name
        );
    }
    Ok((q, sources))
}

fn peek(aws: &Aws, id: &str, limit: usize, json: bool) -> Result<ExitCode> {
    let (q, _) = dead_letter_queue(aws, id, true)?;
    let messages = sqs::peek(aws, &q.url, limit.clamp(1, sqs::PEEK_MAX))?;
    if json {
        let rows: Vec<Value> = messages
            .iter()
            .map(|m| {
                let body = serde_json::from_str::<Value>(&m.body).unwrap_or_else(|_| Value::String(m.body.clone()));
                let attributes: serde_json::Map<String, Value> = m
                    .message_attributes
                    .iter()
                    .map(|(k, a)| (k.clone(), json!({ "dataType": a.data_type, "stringValue": a.string_value, "binaryValue": a.binary_value })))
                    .collect();
                json!({ "messageId": m.message_id, "attributes": m.attributes, "messageAttributes": attributes, "body": body })
            })
            .collect();
        print_json(&Value::Array(rows));
        return Ok(ExitCode::SUCCESS);
    }
    println!("{}", paint(&format!("{} · {} visible · showing {} (not consumed)", q.name, q.visible(), messages.len()), "90"));
    for m in &messages {
        let sent = m.sent().map(|t| format!("{} · {}", fmt_ago(t), fmt_utc(t))).unwrap_or_default();
        println!();
        println!("{}  {}", paint(&m.message_id, "1"), paint(&format!("sent {sent} · received {} times", m.receive_count()), "90"));
        if let Some(g) = m.group() {
            println!("  {} {}", paint("group", "90"), sanitize(g));
        }
        for (name, a) in &m.message_attributes {
            let value = a.string_value.clone().unwrap_or_else(|| format!("({} bytes, base64)", a.binary_value.as_deref().unwrap_or_default().len()));
            println!("  {} {}", paint(&sanitize(name), "90"), sanitize(&value));
        }
        for line in sanitize(&m.pretty_body()).lines() {
            println!("  {line}");
        }
    }
    if messages.is_empty() {
        println!("No messages came back.");
    }
    Ok(ExitCode::SUCCESS)
}

fn redrive(aws: &Aws, env: &str, id: &str, to: Option<&str>, yes: bool) -> Result<ExitCode> {
    let (q, sources) = dead_letter_queue(aws, id, false)?;
    if q.visible() == 0 {
        println!("{} is empty: nothing to redrive.", q.name);
        return Ok(ExitCode::SUCCESS);
    }
    let destination = match to {
        Some(t) => Some(sqs::queue(aws, &sqs::resolve(aws, t)?)?),
        None => None,
    };
    let where_to = match &destination {
        Some(d) => d.name.clone(),
        None => format!("their source queues ({})", sources.iter().map(|s| sqs::queue_name(s)).collect::<Vec<_>>().join(", ")),
    };
    let question = format!("Move about {} messages from {} to {where_to} in {env}?", q.visible(), q.name);
    if !confirm_write(env, &question, yes)? {
        println!("Cancelled.");
        return Ok(ExitCode::FAILURE);
    }
    sqs::start_move(aws, q.arn(), destination.as_ref().map(Queue::arn))?;
    println!("{} Redrive started", paint("✓", "32"));
    // Follow it until it ends.
    loop {
        std::thread::sleep(Duration::from_secs(2));
        let Some(task) = sqs::list_moves(aws, q.arn())?.into_iter().next() else { bail!("the redrive disappeared") };
        let total = task.approximate_number_of_messages_to_move.map(|n| format!(" of {n}")).unwrap_or_default();
        eprint!("\r  {}: {}{total} moved   ", task.status.to_lowercase(), task.approximate_number_of_messages_moved);
        if !task.running() {
            eprintln!();
            return match task.status.as_str() {
                "COMPLETED" => {
                    println!("{} Moved {} messages to {}", paint("✓", "32"), task.approximate_number_of_messages_moved, task.destination());
                    Ok(ExitCode::SUCCESS)
                }
                other => bail!("redrive {}: {}", other.to_lowercase(), task.failure_reason.unwrap_or_default()),
            };
        }
    }
}
