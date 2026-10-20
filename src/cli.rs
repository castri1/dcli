//! Non-interactive commands, for scripts and CI. `--json` prints machine-readable output.

use std::collections::HashMap;
use std::io::{BufRead, IsTerminal, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use clap::{Parser, Subcommand};
use serde_json::{Value, json};

use crate::aws::credentials::effective_profile;
use crate::aws::profile::AwsFiles;
use crate::aws::{Aws, AwsError, codebuild, codepipeline as cp, lambda, logs, ssm};
use crate::config::{self, Config, Environment, Loaded, PipelineConfig};
use crate::pipeline::{self, StageView, Status};
use crate::tui::picker::{self, Item, Picker};
use crate::tui::theme;
use crate::util::{display_path, ellipsize, fmt_ago, fmt_duration, now_secs, program, strip_ansi};

#[derive(Parser)]
#[command(name = "dcli", version, about = "Deploy and explore AWS from your terminal")]
pub struct Cli {
    /// Config file (default: ~/.deployment-cli/config.yaml, ./deployment-cli.yaml, ./.deployment-cli.yaml)
    #[arg(long, global = true, value_name = "FILE")]
    pub config: Option<PathBuf>,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Subcommand)]
pub enum Command {
    /// Open the interactive UI (the default)
    #[command(alias = "watch")]
    Ui { env: Option<String> },
    /// Set the current environment (account) used when a command names none
    #[command(alias = "switch")]
    Use { env: Option<String> },
    /// Search the account's pipelines and add them to an environment
    Add {
        /// Environment; a new name creates it
        env: Option<String>,
        /// Pipelines to add without searching
        pipelines: Vec<String>,
    },
    /// Remove a pipeline from an environment (the pipeline in AWS is not touched)
    #[command(alias = "rm")]
    Remove { env: Option<String>, pipeline: Option<String> },
    /// Sign in with AWS SSO for an environment's profile
    Login { env: Option<String> },
    /// List environments and pipelines
    List {
        #[arg(long)]
        json: bool,
    },
    /// Status of a pipeline's latest execution
    Status {
        /// Environment (default: the current one, see `use`)
        env: Option<String>,
        pipeline: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Recent executions of a pipeline
    History {
        env: Option<String>,
        pipeline: Option<String>,
        #[arg(short = 'n', long, default_value_t = 10)]
        limit: u32,
        #[arg(long)]
        json: bool,
    },
    /// Start a pipeline (with its hooks) and follow it to the end
    Deploy {
        env: Option<String>,
        pipeline: Option<String>,
        /// Skip pre-deploy and post-deploy hooks
        #[arg(long)]
        skip_hooks: bool,
        /// Do not ask for confirmation
        #[arg(short, long)]
        yes: bool,
        /// Return right after starting
        #[arg(long)]
        no_wait: bool,
    },
    /// Approve or reject a pending manual approval
    Approve {
        env: Option<String>,
        pipeline: Option<String>,
        #[arg(long)]
        reject: bool,
        #[arg(short = 'm', long)]
        comment: Option<String>,
        #[arg(short, long)]
        yes: bool,
    },
    /// Stop the running execution
    Stop {
        env: Option<String>,
        pipeline: Option<String>,
        /// Stop without waiting for running actions
        #[arg(long)]
        abandon: bool,
        #[arg(long)]
        reason: Option<String>,
        #[arg(short, long)]
        yes: bool,
    },
    /// Markdown report of the latest failure (error, commit, build log excerpt) to share or
    /// give to an AI agent
    Report {
        env: Option<String>,
        pipeline: Option<String>,
        /// Include the whole build log instead of an excerpt
        #[arg(long)]
        full: bool,
    },
    /// Print CodeBuild logs of the latest execution
    Logs {
        env: Option<String>,
        pipeline: Option<String>,
        #[arg(short, long)]
        follow: bool,
    },
    /// Read a Parameter Store parameter: param [ENV] NAME
    Param {
        env: Option<String>,
        name: Option<String>,
        #[arg(long)]
        decrypt: bool,
        #[arg(long)]
        json: bool,
    },
    /// Show a Lambda function's configuration: lambda [ENV] NAME
    Lambda {
        env: Option<String>,
        name: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// `<env>` alone opens the UI there
    #[command(external_subcommand)]
    Env(Vec<String>),
}

// ---- output helpers ----------------------------------------------------------------

fn color_enabled() -> bool {
    std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none()
}

fn paint(text: &str, code: &str) -> String {
    if color_enabled() { format!("\u{1b}[{code}m{text}\u{1b}[0m") } else { text.to_string() }
}

fn status_code(s: Status) -> &'static str {
    match s {
        Status::Succeeded => "32",
        Status::Failed => "31",
        Status::InProgress => "94",
        Status::Stopping | Status::Stopped => "33",
        Status::Approval => "35",
        _ => "90",
    }
}

fn painted_status(s: Status) -> String {
    padded_status(s, 0)
}

/// Pads before colouring so escape codes do not break column alignment.
fn padded_status(s: Status, width: usize) -> String {
    paint(&format!("{:<width$}", format!("{} {}", s.symbol(), s.label())), status_code(s))
}

fn print_json(v: &Value) {
    println!("{}", serde_json::to_string_pretty(v).unwrap_or_default());
}

fn confirm(question: &str) -> bool {
    print!("{question} [y/N] ");
    let _ = std::io::stdout().flush();
    let mut answer = String::new();
    let _ = std::io::stdin().lock().read_line(&mut answer);
    matches!(answer.trim().to_lowercase().as_str(), "y" | "yes")
}

fn ask(question: &str) -> String {
    print!("{question} ");
    let _ = std::io::stdout().flush();
    let mut answer = String::new();
    let _ = std::io::stdin().lock().read_line(&mut answer);
    answer.trim().to_string()
}

// ---- context -----------------------------------------------------------------------

struct Target<'a> {
    env_name: String,
    pipeline: &'a PipelineConfig,
    aws: Arc<Aws>,
}

fn aws_for(config: &Config, env_name: &str) -> Result<Arc<Aws>> {
    let env = config.env(env_name)?;
    let aws = Aws::new(&env.profile, &env.region);
    ensure_session(&aws, env_name)?;
    Ok(aws)
}

fn interactive() -> bool {
    std::io::stdin().is_terminal() && std::io::stdout().is_terminal()
}

fn current_env(config: &Config) -> Result<String> {
    config.default_env().map(str::to_string).ok_or_else(|| anyhow!("no environments configured"))
}

/// `[ENV] [ITEM]` → environment and item. The environment defaults to the current one,
/// so a single argument that is not an environment name is the item.
fn env_and(config: &Config, env: Option<String>, item: Option<String>) -> Result<(String, Option<String>)> {
    match (env, item) {
        (Some(env), Some(item)) => Ok((env, Some(item))),
        (Some(env), None) if config.environments.contains_key(&env) => Ok((env, None)),
        (Some(item), None) => Ok((current_env(config)?, Some(item))),
        (None, _) => Ok((current_env(config)?, None)),
    }
}

/// Resolves `[ENV] [PIPELINE]`; without a pipeline, the only one, or a picker in a terminal.
fn target(config: &Config, env: Option<String>, pipeline: Option<String>) -> Result<Target<'_>> {
    let (env_name, pipeline) = env_and(config, env, pipeline)?;
    let env = config.env(&env_name)?;
    let pipeline = match pipeline {
        None if env.pipelines.len() > 1 && interactive() => {
            let items = env.pipelines.iter().map(|p| Item::new(p.name.as_str(), p.description.clone().unwrap_or_default()));
            let picker = Picker::new(format!("Pipeline in {env_name}"), "select", items.collect(), false);
            match picker::pick(picker, theme::env_color(&env_name))? {
                Some(chosen) => &env.pipelines[chosen[0]],
                None => bail!("cancelled"),
            }
        }
        None if env.pipelines.is_empty() => {
            bail!("'{env_name}' has no pipelines yet. Add some: {} add {env_name}", program())
        }
        other => config.pipeline(&env_name, other.as_deref())?,
    };
    Ok(Target { env_name: env_name.clone(), pipeline, aws: aws_for(config, &env_name)? })
}

/// `profile · region · account` for listings.
fn env_summary(files: &AwsFiles, env: &Environment) -> String {
    let profile = effective_profile(&env.profile);
    let mut text = format!("{profile} · {}", env.region);
    if let Some(account) = files.profile(&profile).and_then(|p| p.get("sso_account_id").cloned()) {
        text.push_str(&format!(" · {account}"));
    }
    text
}

const NEW_ENV: &str = "+ new environment";

/// Picks an environment (cursor on the current one). With `allow_new`, a last entry asks
/// for a new name, which is returned as is.
fn pick_env(config: &Config, title: &str, allow_new: bool) -> Result<Option<String>> {
    let files = AwsFiles::load();
    let current = config.default_env().unwrap_or_default().to_string();
    let mut names: Vec<String> = config.environments.keys().cloned().collect();
    let mut items: Vec<Item> = config
        .environments
        .iter()
        .map(|(name, env)| {
            let count = env.pipelines.len();
            let detail = format!("{} · {count} pipeline{}", env_summary(&files, env), if count == 1 { "" } else { "s" });
            Item::new(name.as_str(), detail).color(theme::env_color(name)).current(*name == current)
        })
        .collect();
    if allow_new {
        items.push(Item::new(NEW_ENV, "another AWS profile or region"));
        names.push(NEW_ENV.to_string());
    }
    let cursor = config.environments.get_index_of(&current).unwrap_or(0);
    let Some(chosen) = picker::pick(Picker::new(title, "select", items, false).with_cursor(cursor), theme::ACCENT)? else {
        return Ok(None);
    };
    let name = names.swap_remove(chosen[0]);
    if name != NEW_ENV {
        return Ok(Some(name));
    }
    let name = ask("Environment name (e.g. staging):");
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        bail!("use letters, numbers, '-' and '_' for the environment name");
    }
    Ok(Some(name))
}

/// Defines a new environment in memory (saved with its first pipeline): asks for the
/// AWS profile and region.
fn define_env(loaded: &mut Loaded, name: &str) -> Result<()> {
    let files = AwsFiles::load();
    let profiles = files.profile_names();
    if profiles.is_empty() {
        bail!("no AWS profiles in ~/.aws/config. Create one with: aws configure sso");
    }
    let items = profiles
        .iter()
        .map(|p| {
            let section = files.profile(p).unwrap_or_default();
            let detail = [section.get("sso_account_id"), section.get("sso_role_name"), section.get("region")]
                .into_iter()
                .flatten()
                .cloned()
                .collect::<Vec<_>>()
                .join(" · ");
            Item::new(p.as_str(), detail)
        })
        .collect();
    let cursor = profiles.iter().position(|p| p.eq_ignore_ascii_case(name)).unwrap_or(0);
    let picker = Picker::new(format!("AWS profile for '{name}'"), "select", items, false).with_cursor(cursor);
    let Some(chosen) = picker::pick(picker, theme::ACCENT)? else { bail!("cancelled") };
    let profile = profiles[chosen[0]].clone();
    let default_region = files.profile(&profile).and_then(|p| p.get("region").cloned()).unwrap_or_else(|| "us-east-1".into());
    let region = match ask(&format!("Region [{default_region}]:")) {
        r if r.is_empty() => default_region,
        r => r,
    };
    println!("{} {name} → profile {profile} · {region}", paint("+", "32"));
    loaded.config.environments.insert(name.to_string(), Environment { profile, region, pipelines: Vec::new() });
    Ok(())
}

/// Validates credentials; on an interactive terminal, offers an SSO login when needed.
fn ensure_session(aws: &Aws, env_name: &str) -> Result<()> {
    match aws.check_credentials() {
        Err(e) if e.is_login_required() && std::io::stdin().is_terminal() => {
            eprintln!("{}", paint(&format!("! {}", e.short().replace(" — press L to sign in", "")), "33"));
            if confirm(&format!("Sign in now with AWS SSO for '{env_name}'?")) {
                run_login(&aws.profile)?;
                aws.reset_credentials();
                aws.check_credentials()?;
                Ok(())
            } else {
                Err(e.into())
            }
        }
        other => Ok(other?),
    }
}

fn run_login(profile: &str) -> Result<()> {
    let status = crate::aws::credentials::sso_login_command(profile).status().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            anyhow::anyhow!("AWS CLI not found. Install it: https://docs.aws.amazon.com/cli/latest/userguide/getting-started-install.html")
        } else {
            e.into()
        }
    })?;
    if !status.success() {
        bail!("aws sso login exited with {status}");
    }
    Ok(())
}

fn latest(t: &Target) -> Result<Option<cp::ExecutionSummary>> {
    Ok(cp::list_executions(&t.aws, &t.pipeline.name, 1)?.into_iter().next())
}

fn stage_json(s: &StageView) -> Value {
    json!({
        "name": s.name,
        "status": s.status.label(),
        "startTime": s.started,
        "endTime": s.ended,
        "error": s.error(),
        "actions": s.actions.iter().map(|a| json!({
            "name": a.name,
            "status": a.status.label(),
            "provider": a.provider,
            "buildId": a.build_id,
            "url": a.url,
            "error": a.error,
        })).collect::<Vec<_>>(),
    })
}

// ---- commands ----------------------------------------------------------------------

pub fn run(cli: Cli) -> Result<ExitCode> {
    let mut loaded = config::load(cli.config.as_deref())?;
    let command = cli.command.unwrap_or(Command::Ui { env: None });
    let config = &loaded.config;
    match command {
        Command::Ui { env } => {
            crate::tui::run(loaded, env.as_deref())?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Env(args) => match args.as_slice() {
            [env] if config.environments.contains_key(env) => {
                crate::tui::run(loaded, Some(env))?;
                Ok(ExitCode::SUCCESS)
            }
            _ => bail!(
                "'{}' is not a command or an environment. Environments: {}. See {} --help",
                args.join(" "),
                config.environments.keys().map(String::as_str).collect::<Vec<_>>().join(", "),
                program()
            ),
        },
        Command::Use { env } => use_env(config, env),
        Command::Add { env, pipelines } => add(&mut loaded, env, pipelines),
        Command::Remove { env, pipeline } => remove(&mut loaded, env, pipeline),
        Command::Login { env } => login(config, env.as_deref()),
        Command::List { json } => list(config, &loaded.source, json),
        Command::Status { env, pipeline, json } => status(&target(config, env, pipeline)?, json),
        Command::History { env, pipeline, limit, json } => history(&target(config, env, pipeline)?, limit, json),
        Command::Deploy { env, pipeline, skip_hooks, yes, no_wait } => {
            deploy(&target(config, env, pipeline)?, &loaded, skip_hooks, yes, no_wait)
        }
        Command::Approve { env, pipeline, reject, comment, yes } => {
            approve(&target(config, env, pipeline)?, !reject, comment, yes)
        }
        Command::Stop { env, pipeline, abandon, reason, yes } => stop(&target(config, env, pipeline)?, abandon, reason, yes),
        Command::Logs { env, pipeline, follow } => print_logs(&target(config, env, pipeline)?, follow),
        Command::Report { env, pipeline, full } => {
            let t = target(config, env, pipeline)?;
            let report = crate::report::failure_report(&t.aws, &t.env_name, &t.pipeline.name, full)?;
            print!("{}", report.markdown);
            Ok(ExitCode::SUCCESS)
        }
        Command::Param { env, name, decrypt, json } => {
            let (env, Some(name)) = env_and(config, env, name)? else { bail!("which parameter? {} param [ENV] NAME", program()) };
            param(aws_for(config, &env)?.as_ref(), &name, decrypt, json)
        }
        Command::Lambda { env, name, json } => {
            let (env, Some(name)) = env_and(config, env, name)? else { bail!("which function? {} lambda [ENV] NAME", program()) };
            lambda_show(aws_for(config, &env)?.as_ref(), &name, json)
        }
    }
}

fn use_env(config: &Config, env: Option<String>) -> Result<ExitCode> {
    let name = match env {
        Some(name) => {
            config.env(&name)?;
            name
        }
        None if interactive() => match pick_env(config, "Use environment", false)? {
            Some(name) => name,
            None => return Ok(ExitCode::FAILURE),
        },
        None => {
            // Not a terminal: print the list, current one first.
            let current = config.default_env().unwrap_or_default();
            for name in config.environments.keys() {
                println!("{} {name}", if name == current { "*" } else { " " });
            }
            return Ok(ExitCode::SUCCESS);
        }
    };
    config::set_current_env(&name)?;
    let env = config.env(&name)?;
    let color = if config::is_production(&name) { "31;1" } else { "32;1" };
    println!(
        "{} Using {}  {}",
        paint("✓", "32"),
        paint(&name, color),
        paint(&env_summary(&AwsFiles::load(), env), "90")
    );
    Ok(ExitCode::SUCCESS)
}

fn add(loaded: &mut Loaded, env: Option<String>, names: Vec<String>) -> Result<ExitCode> {
    let env_name = match env {
        Some(name) => name,
        None if interactive() => match pick_env(&loaded.config, "Add pipelines to", true)? {
            Some(name) => name,
            None => return Ok(ExitCode::FAILURE),
        },
        None => bail!("which environment? {} add <env> [pipeline…]", program()),
    };
    if !loaded.config.environments.contains_key(&env_name) {
        if !interactive() {
            loaded.config.env(&env_name)?;
        }
        println!("'{env_name}' is a new environment.");
        define_env(loaded, &env_name)?;
    }

    let names = if names.is_empty() {
        if !interactive() {
            bail!("name the pipelines, or run it in a terminal to search: {} add {env_name} <pipeline…>", program());
        }
        let aws = aws_for(&loaded.config, &env_name)?;
        let env = loaded.config.env(&env_name)?;
        eprint!("{}", paint(&format!("Listing pipelines in {} ({})…", env_name, env.region), "90"));
        let all = cp::list_pipelines(&aws);
        eprint!("\r\x1b[2K");
        let all = all?;
        let available: Vec<cp::PipelineSummary> =
            all.iter().filter(|p| !env.pipelines.iter().any(|c| c.name == p.name)).cloned().collect();
        if available.is_empty() {
            println!("All {} pipelines in this account are already in '{env_name}'.", all.len());
            return Ok(ExitCode::SUCCESS);
        }
        let items = available
            .iter()
            .map(|p| Item::new(p.name.as_str(), p.updated.map(|t| format!("updated {}", fmt_ago(t))).unwrap_or_default()))
            .collect();
        let picker = Picker::new(format!("Add pipelines to {env_name}"), "add", items, true);
        match picker::pick(picker, theme::env_color(&env_name))? {
            Some(chosen) => chosen.into_iter().map(|i| available[i].name.clone()).collect(),
            None => return Ok(ExitCode::FAILURE),
        }
    } else {
        names
    };

    let (path, added) = loaded.add_pipelines(&env_name, &names)?;
    for name in &names {
        println!("{} {name}", paint("+", "32"));
    }
    if added < names.len() {
        println!("{}", paint(&format!("{} were already there", names.len() - added), "90"));
    }
    println!("{}", paint(&format!("Saved {}", display_path(&path)), "90"));
    Ok(ExitCode::SUCCESS)
}

fn remove(loaded: &mut Loaded, env: Option<String>, pipeline: Option<String>) -> Result<ExitCode> {
    let env_name = match env {
        Some(name) => name,
        None if interactive() => match pick_env(&loaded.config, "Remove a pipeline from", false)? {
            Some(name) => name,
            None => return Ok(ExitCode::FAILURE),
        },
        None => bail!("which environment? {} remove <env> <pipeline>", program()),
    };
    let env = loaded.config.env(&env_name)?;
    let name = match pipeline {
        Some(name) => name,
        None if env.pipelines.is_empty() => bail!("'{env_name}' has no pipelines"),
        None if interactive() => {
            let items = env.pipelines.iter().map(|p| Item::new(p.name.as_str(), p.description.clone().unwrap_or_default()));
            let picker = Picker::new(format!("Remove from {env_name}"), "remove", items.collect(), false);
            match picker::pick(picker, theme::WARN)? {
                Some(chosen) => env.pipelines[chosen[0]].name.clone(),
                None => return Ok(ExitCode::FAILURE),
            }
        }
        None => bail!("which pipeline? {} remove {env_name} <pipeline>", program()),
    };
    let path = loaded.remove_pipeline(&env_name, &name)?;
    println!("{} {name} removed from {env_name} (the pipeline in AWS is untouched)", paint("−", "33"));
    println!("{}", paint(&format!("Saved {}", display_path(&path)), "90"));
    Ok(ExitCode::SUCCESS)
}

fn login(config: &Config, env: Option<&str>) -> Result<ExitCode> {
    let name = match env {
        Some(n) => n.to_string(),
        None => current_env(config)?,
    };
    let e = config.env(&name)?;
    let profile = crate::aws::credentials::effective_profile(&e.profile);
    println!("Signing in to AWS SSO for '{name}' (profile {profile})…");
    run_login(&e.profile)?;
    let aws = Aws::new(&e.profile, &e.region);
    aws.check_credentials()?;
    println!("{}", paint("✓ Signed in", "32"));
    Ok(ExitCode::SUCCESS)
}

fn list(config: &Config, source: &config::Source, json: bool) -> Result<ExitCode> {
    if json {
        print_json(&serde_json::to_value(config)?);
        return Ok(ExitCode::SUCCESS);
    }
    match source {
        config::Source::File(p) => println!("{}", paint(&format!("Config: {}", display_path(p)), "90")),
        config::Source::AwsProfiles => {
            println!("{}", paint("No config file — showing AWS profiles as environments", "90"))
        }
    }
    let files = AwsFiles::load();
    let current = config.default_env().unwrap_or_default();
    for (name, env) in &config.environments {
        let color = if config::is_production(name) { "31;1" } else { "32;1" };
        let marker = if name == current { paint(" ● current", "36") } else { String::new() };
        println!("\n{}  {}{marker}", paint(name, color), paint(&env_summary(&files, env), "90"));
        for p in &env.pipelines {
            let hooks = p.hooks.pre_deploy.len() + p.hooks.post_deploy.len();
            let extra = if hooks > 0 { format!(" · {hooks} hooks") } else { String::new() };
            println!(
                "  • {}{}",
                p.name,
                paint(&format!("{}{extra}", p.description.as_deref().map(|d| format!("  {d}")).unwrap_or_default()), "90")
            );
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn status(t: &Target, json: bool) -> Result<ExitCode> {
    let name = &t.pipeline.name;
    let state = cp::get_pipeline_state(&t.aws, name)?;
    let latest = latest(t)?;
    let details = match &latest {
        Some(l) => cp::list_action_executions(&t.aws, name, Some(&l.pipeline_execution_id), 100)?,
        None => Vec::new(),
    };
    let stages = pipeline::stages(&state, latest.as_ref(), &details);
    let approvals = pipeline::approvals(&stages);

    if json {
        let revision = latest.as_ref().map(|l| pipeline::revision(l, &details));
        print_json(&json!({
            "environment": t.env_name,
            "pipeline": name,
            "execution": latest.as_ref().map(|l| json!({
                "id": l.pipeline_execution_id,
                "status": l.status,
                "trigger": pipeline::trigger_label(l),
                "startTime": l.start_time,
                "lastUpdateTime": l.last_update_time,
            })),
            "revision": revision.map(|r| json!({
                "commit": r.commit, "message": r.message, "branch": r.branch, "repo": r.repo,
            })),
            "stages": stages.iter().map(stage_json).collect::<Vec<_>>(),
            "pendingApprovals": approvals.iter().map(|a| json!({
                "stage": a.stage, "action": a.action, "summary": a.summary,
            })).collect::<Vec<_>>(),
        }));
        return Ok(ExitCode::SUCCESS);
    }

    println!("{}  {}", paint(name, "1"), paint(&format!("{} · {}", t.env_name, t.aws.region), "90"));
    let Some(l) = &latest else {
        println!("  No executions yet");
        return Ok(ExitCode::SUCCESS);
    };
    let s = Status::parse(&l.status);
    let took = l
        .start_time
        .map(|st| fmt_duration(l.last_update_time.filter(|_| !s.is_running()).unwrap_or_else(now_secs) - st))
        .unwrap_or_default();
    println!(
        "  {}  {}  {}",
        painted_status(s),
        &l.pipeline_execution_id[..8.min(l.pipeline_execution_id.len())],
        paint(
            &format!(
                "{} · started {} · {took}",
                pipeline::trigger_label(l),
                l.start_time.map(fmt_ago).unwrap_or_default()
            ),
            "90"
        )
    );
    let rev = pipeline::revision(l, &details);
    if rev.commit.is_some() || rev.message.is_some() {
        println!("  {}", paint(&rev.one_line(), "90"));
    }
    println!();
    let width = stages.iter().map(|s| s.name.len()).max().unwrap_or(5);
    let now = now_secs();
    for s in &stages {
        let time = s.elapsed(now).map(fmt_duration).unwrap_or_default();
        let extra = match s.status {
            Status::Failed => paint(&ellipsize(s.error().unwrap_or(""), 80), "31"),
            Status::Approval => paint("waiting for approval", "35"),
            _ => paint(
                &s.actions.iter().filter_map(|a| a.provider.clone()).collect::<Vec<_>>().join(", "),
                "90",
            ),
        };
        println!("  {:<width$}  {} {:>8}  {extra}", s.name, padded_status(s.status, 16), time);
    }
    for a in &approvals {
        println!(
            "\n{}",
            paint(
                &format!(
                    "◆ Approval pending at {}/{}. Run: {} approve {} {}",
                    a.stage,
                    a.action,
                    crate::util::program(),
                    t.env_name,
                    name
                ),
                "35"
            )
        );
    }
    Ok(ExitCode::SUCCESS)
}

fn history(t: &Target, limit: u32, json: bool) -> Result<ExitCode> {
    let execs = cp::list_executions(&t.aws, &t.pipeline.name, limit.clamp(1, 100))?;
    if json {
        print_json(&serde_json::to_value(&execs)?);
        return Ok(ExitCode::SUCCESS);
    }
    for e in &execs {
        let s = Status::parse(&e.status);
        let duration = e.start_time.zip(e.last_update_time).map(|(a, b)| fmt_duration(b - a)).unwrap_or_default();
        let rev = pipeline::revision(e, &[]);
        println!(
            "{} {}  {:<8} {:<10} {:>8}  {}",
            padded_status(s, 16),
            &e.pipeline_execution_id[..8.min(e.pipeline_execution_id.len())],
            pipeline::trigger_label(e),
            e.start_time.map(fmt_ago).unwrap_or_default(),
            duration,
            paint(&rev.one_line(), "90"),
        );
    }
    Ok(ExitCode::SUCCESS)
}

fn deploy(t: &Target, loaded: &Loaded, skip_hooks: bool, yes: bool, no_wait: bool) -> Result<ExitCode> {
    let config = &loaded.config;
    let name = &t.pipeline.name;
    let hooks = &t.pipeline.hooks;
    if !skip_hooks && !hooks.is_empty() {
        // Show what will run on this machine, and flag a config picked up from the current
        // directory (a cloned repository, say).
        let header = match &loaded.source {
            config::Source::File(p) if loaded.from_cwd => {
                paint(&format!("Hooks from {}, found in the current directory:", display_path(p)), "33")
            }
            _ => paint("Hooks:", "90"),
        };
        println!("{header}");
        for (phase, list) in [("pre-deploy", &hooks.pre_deploy), ("post-deploy", &hooks.post_deploy)] {
            for h in list {
                println!("  {} {}", paint(&format!("{phase:<12}"), "90"), crate::util::show_invisible(&h.command));
            }
        }
    }
    if let Some(l) = latest(t)?
        && Status::parse(&l.status).is_running()
    {
        println!(
            "{}",
            paint(&format!("! {name} is running now ({}); a new execution supersedes it.", &l.pipeline_execution_id[..8]), "33")
        );
    }
    if !yes {
        if !std::io::stdin().is_terminal() {
            bail!("refusing to deploy without confirmation; pass --yes");
        }
        if config::is_production(&t.env_name) {
            let typed = ask(&format!("Deploy {name} to {}? Type '{}' to confirm:", t.env_name.to_uppercase(), t.env_name));
            if typed != t.env_name {
                println!("Cancelled.");
                return Ok(ExitCode::FAILURE);
            }
        } else if !confirm(&format!("Deploy {name} to {}?", t.env_name)) {
            println!("Cancelled.");
            return Ok(ExitCode::FAILURE);
        }
    }

    if !skip_hooks && !run_hooks("pre-deploy", &t.pipeline.hooks.pre_deploy) {
        bail!("pre-deploy hook failed — deployment aborted (use --skip-hooks to bypass)");
    }

    let exec = cp::start(&t.aws, name)?;
    println!("{} {name} started · execution {exec}", paint("✓", "32"));
    if no_wait {
        return Ok(ExitCode::SUCCESS);
    }
    println!("{}", paint("Following… Ctrl+C stops following; the pipeline keeps running.", "90"));

    let poll = Duration::from_secs(config.defaults.poll_interval_seconds.clamp(3, 10));
    let mut seen: HashMap<String, Status> = HashMap::new();
    let mut announced_approvals = Vec::new();
    let result = loop {
        std::thread::sleep(poll);
        let execution = cp::get_execution(&t.aws, name, &exec)?;
        let state = cp::get_pipeline_state(&t.aws, name)?;
        let details = cp::list_action_executions(&t.aws, name, Some(&exec), 100)?;
        let stages = pipeline::stages(&state, Some(&execution), &details);
        let now = now_secs();
        for s in &stages {
            if seen.get(&s.name) != Some(&s.status) && !matches!(s.status, Status::Pending | Status::NotRun) {
                let time = s.elapsed(now).filter(|_| !s.status.is_running()).map(fmt_duration).unwrap_or_default();
                println!("  {} {:<16} {}", padded_status(s.status, 16), s.name, paint(&time, "90"));
                if let Some(e) = s.error() {
                    println!("    {}", paint(e, "31"));
                }
                seen.insert(s.name.clone(), s.status);
            }
        }
        for a in pipeline::approvals(&stages) {
            if !announced_approvals.contains(&a.token) {
                println!(
                    "{}",
                    paint(&format!("  ◆ Waiting for approval at {}/{} — {} approve {} {name}", a.stage, a.action, crate::util::program(), t.env_name), "35")
                );
                announced_approvals.push(a.token);
            }
        }
        let status = Status::parse(&execution.status);
        if !status.is_running() && status != Status::Unknown {
            let took = execution.start_time.zip(execution.last_update_time).map(|(a, b)| fmt_duration(b - a)).unwrap_or_default();
            println!("\n{} {name} {} in {took}", painted_status(status), t.env_name);
            break status;
        }
    };

    if result != Status::Succeeded {
        return Ok(ExitCode::FAILURE);
    }
    if !skip_hooks && !run_hooks("post-deploy", &t.pipeline.hooks.post_deploy) {
        return Ok(ExitCode::FAILURE);
    }
    Ok(ExitCode::SUCCESS)
}

fn run_hooks(phase: &str, hooks: &[crate::config::Hook]) -> bool {
    if hooks.is_empty() {
        return true;
    }
    println!("{}", paint(&format!("Running {phase} hooks…"), "90"));
    for hook in hooks {
        println!("{} {}", paint("▸", "36"), hook.label());
        let outcome = crate::hooks::run_inherited(hook);
        let took = fmt_duration(outcome.duration.as_secs_f64());
        if !outcome.success {
            println!("{} {} — {} ({took})", paint("✗", "31"), hook.label(), outcome.detail);
            return false;
        }
        println!("{} {} ({took})", paint("✓", "32"), hook.label());
    }
    true
}

fn approve(t: &Target, approved: bool, comment: Option<String>, yes: bool) -> Result<ExitCode> {
    let name = &t.pipeline.name;
    let state = cp::get_pipeline_state(&t.aws, name)?;
    let latest = latest(t)?;
    let stages = pipeline::stages(&state, latest.as_ref(), &[]);
    let Some(a) = pipeline::approvals(&stages).into_iter().next() else {
        bail!("{name} has no pending approval in {}", t.env_name);
    };
    let verb = if approved { "Approve" } else { "Reject" };
    println!("{} {}/{} in {name} ({})", paint("◆", "35"), a.stage, a.action, t.env_name);
    if let Some(l) = &latest {
        let id = &l.pipeline_execution_id;
        println!("  {}", paint(&format!("execution {} · {}", &id[..8.min(id.len())], pipeline::revision(l, &[]).one_line()), "90"));
    }
    if let Some(s) = &a.summary {
        println!("  {s}");
    }
    if !yes && !confirm(&format!("{verb}?")) {
        println!("Cancelled.");
        return Ok(ExitCode::FAILURE);
    }
    let summary = comment.unwrap_or_else(|| format!("{verb}d via dcli"));
    cp::put_approval(&t.aws, name, &a.stage, &a.action, &a.token, approved, &summary)?;
    println!("{} {verb}d", paint("✓", "32"));
    Ok(ExitCode::SUCCESS)
}

fn stop(t: &Target, abandon: bool, reason: Option<String>, yes: bool) -> Result<ExitCode> {
    let name = &t.pipeline.name;
    let Some(l) = latest(t)?.filter(|l| Status::parse(&l.status) == Status::InProgress) else {
        bail!("{name} is not running in {}", t.env_name);
    };
    let mode = if abandon { "abandon (no waiting)" } else { "stop and wait for running actions" };
    if !yes && !confirm(&format!("Stop {name} ({}) — {mode}?", &l.pipeline_execution_id[..8])) {
        println!("Cancelled.");
        return Ok(ExitCode::FAILURE);
    }
    let reason = reason.unwrap_or_else(|| "Stopped via dcli".into());
    cp::stop(&t.aws, name, &l.pipeline_execution_id, abandon, &reason)?;
    println!("{} Stop requested", paint("✓", "32"));
    Ok(ExitCode::SUCCESS)
}

fn print_logs(t: &Target, follow: bool) -> Result<ExitCode> {
    let name = &t.pipeline.name;
    let Some(l) = latest(t)? else { bail!("{name} has no executions") };
    let state = cp::get_pipeline_state(&t.aws, name)?;
    let details = cp::list_action_executions(&t.aws, name, Some(&l.pipeline_execution_id), 100)?;
    let stages = pipeline::stages(&state, Some(&l), &details);
    let action = stages
        .iter()
        .flat_map(|s| s.actions.iter().filter(|a| a.build_id.is_some()).map(move |a| (s, a)))
        .max_by_key(|(_, a)| match a.status {
            Status::InProgress => 3,
            Status::Failed => 2,
            _ => 1,
        });
    let Some((stage, action)) = action else { bail!("no CodeBuild action in the latest execution of {name}") };
    let build_id = action.build_id.clone().unwrap_or_default();
    eprintln!("{}", paint(&format!("Logs · {} / {} · {build_id}", stage.name, action.name), "90"));

    let mut token: Option<String> = None;
    loop {
        let build = codebuild::get_build(&t.aws, &build_id)?;
        let done = build.as_ref().is_none_or(|b| b.build_status != "IN_PROGRESS");
        let stream = build.as_ref().and_then(|b| b.logs.as_ref()).and_then(|l| Some((l.group_name.clone()?, l.stream_name.clone()?)));
        let mut printed = false;
        if let Some((group, stream)) = stream {
            loop {
                let page = match logs::get_log_events(&t.aws, &group, &stream, token.as_deref()) {
                    Err(AwsError::Api { code, .. }) if code == "ResourceNotFoundException" => break,
                    other => other?,
                };
                let empty = page.events.is_empty();
                for e in &page.events {
                    for line in e.message.lines() {
                        println!("{}", strip_ansi(line.trim_end()));
                    }
                }
                printed |= !empty;
                let same = page.next_forward_token == token;
                token = page.next_forward_token;
                if empty || same {
                    break;
                }
            }
        }
        if !follow || (done && !printed) {
            break;
        }
        std::thread::sleep(Duration::from_secs(3));
    }
    Ok(ExitCode::SUCCESS)
}

fn param(aws: &Aws, name: &str, decrypt: bool, json: bool) -> Result<ExitCode> {
    let p = ssm::get_parameter(aws, name, decrypt)?;
    if json {
        print_json(&serde_json::to_value(&p)?);
    } else {
        println!("{}", p.value);
    }
    Ok(ExitCode::SUCCESS)
}

fn lambda_show(aws: &Aws, name: &str, json: bool) -> Result<ExitCode> {
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
