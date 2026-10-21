//! `dcli promote` (deploy what an environment runs to another one) and `dcli rollback`
//! (go back to the previous successful deploy), through the deploy path and its hooks.

use std::process::ExitCode;

use anyhow::{Result, anyhow, bail};

use crate::aws::codepipeline::{self as cp, ExecutionSummary, Pin};
use crate::cli::{Target, aws_for, confirm_write, deploy_pinned, follow, paint, run_hooks, target};
use crate::config::Loaded;
use crate::matrix::{self, Cell, Drift};
use crate::pipeline::{self, Status};
use crate::util::fmt_ago;

#[derive(clap::Args)]
pub struct Args {
    /// Application (a row of `dcli matrix`)
    app: String,
    /// Environment to deploy to
    #[arg(long)]
    to: String,
    /// Environment whose revision is deployed (default: the one before --to that runs the app)
    #[arg(long)]
    from: Option<String>,
    /// Source action that takes the revision, when the pipeline has several
    #[arg(long, value_name = "NAME")]
    source_action: Option<String>,
    /// Skip pre-deploy and post-deploy hooks
    #[arg(long, alias = "no-hooks")]
    skip_hooks: bool,
    /// Do not ask for confirmation
    #[arg(short, long)]
    yes: bool,
    /// Return right after starting
    #[arg(long)]
    no_wait: bool,
}

#[derive(clap::Args)]
pub struct RollbackArgs {
    /// Environment (default: the current one, see `use`)
    env: Option<String>,
    /// Pipeline
    pipeline: Option<String>,
    /// Stage to roll back, in a V2 pipeline (default: the last stage that deploys)
    #[arg(long)]
    stage: Option<String>,
    /// Execution to go back to (default: the previous successful one with another revision)
    #[arg(long, value_name = "EXECUTION_ID")]
    to: Option<String>,
    /// Run the whole pipeline again at the earlier revision, even when a stage rollback is possible
    #[arg(long)]
    rerun: bool,
    /// Skip pre-deploy and post-deploy hooks
    #[arg(long, alias = "no-hooks")]
    skip_hooks: bool,
    /// Do not ask for confirmation
    #[arg(short, long)]
    yes: bool,
    /// Return right after starting
    #[arg(long)]
    no_wait: bool,
}

fn describe(e: &ExecutionSummary) -> String {
    let when = e.last_update_time.or(e.start_time).map(|t| format!(" · deployed {}", fmt_ago(t))).unwrap_or_default();
    format!("{}{when}", pipeline::revision(e, &[]).one_line())
}

fn row(key: &str, value: &str) {
    println!("  {} {value}", paint(&format!("{key:<9}"), "90"));
}

/// The source action that takes the revision: `named` when given, else as [`pipeline::pick_source`].
fn source<'a>(declaration: &'a cp::Declaration, named: Option<&str>, wanted: &str) -> Result<&'a cp::ActionDeclaration> {
    let sources = declaration.source_actions();
    let index = match named {
        Some(n) => sources.iter().position(|a| a.name == n),
        None => pipeline::pick_source(&sources, wanted),
    };
    let names: Vec<&str> = sources.iter().map(|a| a.name.as_str()).collect();
    match index {
        Some(i) => Ok(sources[i]),
        None if sources.is_empty() => bail!("{} has no source action to run a given revision with", declaration.name),
        None => bail!("{} has several source actions ({}): choose one with --source-action", declaration.name, names.join(", ")),
    }
}

pub fn run(loaded: &Loaded, args: Args) -> Result<ExitCode> {
    let config = &loaded.config;
    let (envs, apps) = matrix::layout(config);
    let Some(app) = apps.iter().find(|a| a.name == args.app) else {
        let names: Vec<&str> = apps.iter().map(|a| a.name.as_str()).collect();
        bail!("no application '{}' (see `matrix`). Applications: {}", args.app, names.join(", "));
    };
    let column = |env: &str| envs.iter().position(|e| e == env).ok_or_else(|| anyhow!("no environment '{env}'"));
    let to = column(&args.to)?;
    let Some(to_pipeline) = app.pipelines[to].clone() else { bail!("{} has no pipeline in {}", app.name, args.to) };
    let from = match &args.from {
        Some(env) => column(env)?,
        None => (0..to)
            .rev()
            .find(|&c| app.pipelines[c].is_some())
            .ok_or_else(|| anyhow!("{} runs in no environment before {}: pass --from", app.name, args.to))?,
    };
    if from == to {
        bail!("--from and --to are the same environment");
    }
    let (from_env, Some(from_pipeline)) = (&envs[from], app.pipelines[from].clone()) else {
        bail!("{} has no pipeline in {}", app.name, envs[from])
    };

    let theirs = Cell::fetch(aws_for(config, from_env)?.as_ref(), &from_pipeline)?;
    let Some(deployed) = theirs.deployed() else {
        bail!("{from_env} has no successful deploy of {} in its last {} runs", app.name, matrix::HISTORY)
    };
    let revision = matrix::commit_of(deployed).ok_or_else(|| anyhow!("{from_env}'s deploy of {} has no revision", app.name))?;
    let t = target(config, Some(args.to.clone()), Some(to_pipeline.clone()))?;
    let ours = Cell::fetch(&t.aws, &to_pipeline)?;
    let behind = match matrix::drift(&theirs, &ours) {
        Some(Drift::InSync) => {
            println!("{} {} already runs what {from_env} runs ({})", paint("✓", "32"), args.to, &revision[..7.min(revision.len())]);
            return Ok(ExitCode::SUCCESS);
        }
        Some(Drift::Behind(n)) => Some(n),
        _ => None,
    };
    let declaration = cp::get_pipeline(&t.aws, &to_pipeline)?;
    let wanted = deployed.source_revisions.first().map(|r| r.action_name.as_str()).unwrap_or_default();
    let action = source(&declaration, args.source_action.as_deref(), wanted)?;
    let pin = Pin::new(action, revision);

    println!("{} {}: what {from_env} runs → {}", paint("Promote", "1"), app.name, args.to);
    row("revision", &describe(deployed));
    row("pipeline", &format!("{to_pipeline} ({})", args.to));
    match (behind, ours.deployed()) {
        (Some(n), _) => row("moves", &format!("{n} deploy{} forward", if n == 1 { "" } else { "s" })),
        (None, Some(now)) => row("replaces", &format!("{} (not in {from_env}'s recent history)", describe(now))),
        (None, None) => row("replaces", "nothing yet: no successful deploy in its recent history"),
    }
    row("source", &format!("{} ({})", action.name, action.action_type_id.provider));
    deploy_pinned(&t, loaded, args.skip_hooks, args.yes, args.no_wait, Some(&pin))
}

pub fn rollback(loaded: &Loaded, args: RollbackArgs) -> Result<ExitCode> {
    let config = &loaded.config;
    let t = target(config, args.env.clone(), args.pipeline.clone())?;
    let name = &t.pipeline.name;
    let declaration = cp::get_pipeline(&t.aws, name)?;
    let history = cp::list_executions(&t.aws, name, matrix::HISTORY)?;
    let Some(plan) = pipeline::rollback_plan(&history) else {
        bail!("nothing to roll back to: no earlier successful run of {name} with another revision in its last {} runs", history.len());
    };
    let back_to = match &args.to {
        Some(id) => plan.targets.iter().find(|e| e.pipeline_execution_id.starts_with(id.as_str())).ok_or_else(|| {
            let ids: Vec<&str> = plan.targets.iter().map(|e| &e.pipeline_execution_id[..8.min(e.pipeline_execution_id.len())]).collect();
            anyhow!("{id} is not an earlier successful run with another revision. Candidates: {}", ids.join(", "))
        })?,
        None => &plan.targets[0],
    };
    println!("{} {name} in {}", paint("Rollback", "1"), t.env_name);
    row("now", &describe(&plan.current));
    row("back to", &describe(back_to));

    if declaration.is_v2() && !args.rerun {
        let stages = declaration.rollback_stages();
        let stage = match &args.stage {
            Some(s) => stages.iter().find(|x| **x == s.as_str()).copied().ok_or_else(|| anyhow!("no stage '{s}'. Stages: {}", stages.join(", ")))?,
            None => declaration.deploy_stage().ok_or_else(|| anyhow!("{name} has no stage to roll back"))?,
        };
        row(
            "method",
            &format!("roll back stage {stage} (a V2 pipeline): it gets again what that run deployed there; nothing is rebuilt and other stages do not run"),
        );
        return native(&t, loaded, &args, stage, &back_to.pipeline_execution_id);
    }
    if args.stage.is_some() {
        bail!("--stage needs a stage rollback: {name} is a V1 pipeline (or --rerun was given), so the whole pipeline runs again");
    }
    let why = if declaration.is_v2() { "--rerun" } else { "a V1 pipeline cannot roll a stage back" };
    row("method", &format!("run the whole pipeline again at that revision ({why}): build, tests and approvals run again"));
    let wanted = plan.current.source_revisions.first().map(|r| r.action_name.as_str()).unwrap_or_default();
    let action = source(&declaration, None, wanted)?;
    let revision = pipeline::revision_of(back_to, &action.name).ok_or_else(|| anyhow!("that run has no revision to deploy again"))?;
    deploy_pinned(&t, loaded, args.skip_hooks, args.yes, args.no_wait, Some(&Pin::new(action, revision)))
}

/// A stage rollback: confirmed, followed to the end, then the post-deploy hooks.
fn native(t: &Target, loaded: &Loaded, args: &RollbackArgs, stage: &str, execution: &str) -> Result<ExitCode> {
    let name = &t.pipeline.name;
    let post = &t.pipeline.hooks.post_deploy;
    if !args.skip_hooks && !post.is_empty() {
        println!("{}", paint("Post-deploy hooks run after it succeeds (pre-deploy hooks do not: nothing is built):", "90"));
        for h in post {
            println!("  {}", crate::util::show_invisible(&h.command));
        }
    }
    if !confirm_write(&t.env_name, &format!("Roll back {stage} of {name} in {}?", t.env_name), args.yes)? {
        println!("Cancelled.");
        return Ok(ExitCode::FAILURE);
    }
    let exec = cp::rollback_stage(&t.aws, name, stage, execution).map_err(|e| anyhow!("could not roll back {stage} of {name}: {}", e.short()))?;
    println!("{} rolling back {stage} of {name} · execution {exec}", paint("✓", "32"));
    if args.no_wait {
        return Ok(ExitCode::SUCCESS);
    }
    if follow(t, &loaded.config, &exec)? != Status::Succeeded {
        return Ok(ExitCode::FAILURE);
    }
    if !args.skip_hooks && !run_hooks("post-deploy", post) {
        return Ok(ExitCode::FAILURE);
    }
    Ok(ExitCode::SUCCESS)
}
