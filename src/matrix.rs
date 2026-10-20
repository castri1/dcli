//! "What is deployed where": one row per application, one column per environment (in
//! config order, which is the promotion order). A cell is the pipeline of that application
//! in that environment: the commit of its last successful execution, what runs now, and
//! how far it trails the environment to its left. Shared by the Matrix module and
//! `dcli matrix`.

use crate::aws::{self, Aws, codepipeline as cp};
use crate::config::{Config, PipelineConfig};
use crate::pipeline::Status;

/// Executions read per pipeline: enough to find the last success and count drift.
pub const HISTORY: u32 = 25;

/// An application: the pipeline that deploys it in each environment (`None` where it has none).
#[derive(Debug, Clone, PartialEq)]
pub struct App {
    pub name: String,
    pub pipelines: Vec<Option<String>>,
}

/// The application a pipeline deploys: its `app` in the config, else its name without the
/// environment (`orders-uat` in `uat` → `orders`), else its name.
pub fn app_name(pipeline: &PipelineConfig, env_name: &str) -> String {
    if let Some(app) = pipeline.app.as_deref().filter(|a| !a.trim().is_empty()) {
        return app.trim().to_string();
    }
    let name = &pipeline.name;
    let (lower, env) = (name.to_lowercase(), env_name.to_lowercase());
    for sep in ['-', '_', '.'] {
        if let Some(stem) = lower.strip_suffix(&format!("{sep}{env}")).filter(|s| !s.is_empty()) {
            return name[..stem.len()].to_string();
        }
        if lower.starts_with(&format!("{env}{sep}")) && name.len() > env.len() + 1 {
            return name[env.len() + 1..].to_string();
        }
    }
    name.clone()
}

/// Environment names (columns) and applications (rows, in order of first appearance).
pub fn layout(config: &Config) -> (Vec<String>, Vec<App>) {
    let envs: Vec<String> = config.environments.keys().cloned().collect();
    let mut apps: Vec<App> = Vec::new();
    for (column, (env_name, env)) in config.environments.iter().enumerate() {
        for p in &env.pipelines {
            let name = app_name(p, env_name);
            let index = match apps.iter().position(|a| a.name == name && a.pipelines[column].is_none()) {
                Some(i) => i,
                None => {
                    apps.push(App { name, pipelines: vec![None; envs.len()] });
                    apps.len() - 1
                }
            };
            apps[index].pipelines[column] = Some(p.name.clone());
        }
    }
    (envs, apps)
}

/// What one pipeline has deployed and is doing now.
#[derive(Debug, Clone, Default)]
pub struct Cell {
    /// Newest first.
    pub history: Vec<cp::ExecutionSummary>,
}

impl Cell {
    pub fn fetch(aws: &Aws, pipeline: &str) -> aws::Result<Cell> {
        Ok(Cell { history: cp::list_executions(aws, pipeline, HISTORY)? })
    }

    /// The last successful execution: what is running in that environment.
    pub fn deployed(&self) -> Option<&cp::ExecutionSummary> {
        self.history.iter().find(|e| Status::parse(&e.status) == Status::Succeeded)
    }

    /// The newest execution, when it is not the deployed one (running, failed, stopped…).
    pub fn activity(&self) -> Option<&cp::ExecutionSummary> {
        self.history.first().filter(|e| Status::parse(&e.status) != Status::Succeeded)
    }

    /// Full commit id of the deployed execution.
    pub fn commit(&self) -> Option<&str> {
        commit_of(self.deployed()?)
    }
}

pub fn commit_of(e: &cp::ExecutionSummary) -> Option<&str> {
    e.source_revisions.first()?.revision_id.as_deref()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Drift {
    /// Same commit as the environment to the left.
    InSync,
    /// The left environment deployed this many newer commits since this one.
    Behind(usize),
    /// Different commits, and this one is not in the left environment's recent history
    /// (a hotfix, or too far back to tell).
    Differs,
}

/// How `cell` compares with `left`, the previous environment in the promotion order.
pub fn drift(left: &Cell, cell: &Cell) -> Option<Drift> {
    let (theirs, ours) = (left.commit()?, cell.commit()?);
    if theirs == ours {
        return Some(Drift::InSync);
    }
    let successes: Vec<&str> = left
        .history
        .iter()
        .filter(|e| Status::parse(&e.status) == Status::Succeeded)
        .filter_map(commit_of)
        .collect();
    let Some(at) = successes.iter().position(|c| *c == ours) else { return Some(Drift::Differs) };
    let mut newer: Vec<&str> = successes[..at].to_vec();
    newer.dedup();
    Some(Drift::Behind(newer.len()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aws::codepipeline::{ExecutionSummary, SourceRevision};
    use crate::config::Environment;

    fn pipeline(name: &str) -> PipelineConfig {
        PipelineConfig { name: name.into(), ..Default::default() }
    }

    #[test]
    fn groups_pipelines_into_apps() {
        assert_eq!(app_name(&pipeline("orders-uat"), "uat"), "orders");
        assert_eq!(app_name(&pipeline("Orders_PROD"), "prod"), "Orders");
        assert_eq!(app_name(&pipeline("dev-orders"), "dev"), "orders");
        assert_eq!(app_name(&pipeline("buyer-support"), "dev"), "buyer-support");
        assert_eq!(app_name(&pipeline("prod"), "prod"), "prod");
        let mut explicit = pipeline("legacy-billing-live");
        explicit.app = Some("billing".into());
        assert_eq!(app_name(&explicit, "prod"), "billing");

        let mut config = Config::default();
        let env = |names: &[&str]| Environment {
            pipelines: names.iter().map(|n| pipeline(n)).collect(),
            ..Default::default()
        };
        config.environments.insert("uat".into(), env(&["orders-uat", "support"]));
        config.environments.insert("prod".into(), env(&["support", "orders-prod", "billing-prod"]));
        let (envs, apps) = layout(&config);
        assert_eq!(envs, ["uat", "prod"]);
        assert_eq!(
            apps,
            vec![
                App { name: "orders".into(), pipelines: vec![Some("orders-uat".into()), Some("orders-prod".into())] },
                App { name: "support".into(), pipelines: vec![Some("support".into()), Some("support".into())] },
                App { name: "billing".into(), pipelines: vec![None, Some("billing-prod".into())] },
            ]
        );
    }

    fn cell(runs: &[(&str, &str)]) -> Cell {
        Cell {
            history: runs
                .iter()
                .map(|(status, sha)| ExecutionSummary {
                    status: status.to_string(),
                    source_revisions: vec![SourceRevision { revision_id: Some(sha.to_string()), ..Default::default() }],
                    ..Default::default()
                })
                .collect(),
        }
    }

    #[test]
    fn measures_drift() {
        let dev = cell(&[("InProgress", "e"), ("Succeeded", "d"), ("Failed", "x"), ("Succeeded", "c"), ("Succeeded", "c"), ("Succeeded", "b")]);
        assert_eq!(dev.commit(), Some("d"));
        assert_eq!(dev.activity().map(|e| e.status.as_str()), Some("InProgress"));
        assert_eq!(drift(&dev, &cell(&[("Succeeded", "d")])), Some(Drift::InSync));
        assert_eq!(drift(&dev, &cell(&[("Succeeded", "c")])), Some(Drift::Behind(1)));
        assert_eq!(drift(&dev, &cell(&[("Failed", "e"), ("Succeeded", "b")])), Some(Drift::Behind(2)));
        assert_eq!(drift(&dev, &cell(&[("Succeeded", "z")])), Some(Drift::Differs));
        assert_eq!(drift(&dev, &cell(&[("Failed", "z")])), None);
    }
}
