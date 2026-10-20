//! The YAML config. Same format and search paths as the Node version (deploy-cli), so an
//! existing `~/.deployment-cli/config.yaml` works unchanged.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::aws::profile::AwsFiles;
use crate::util::home_dir;

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct Config {
    #[serde(default)]
    pub environments: IndexMap<String, Environment>,
    #[serde(default)]
    pub defaults: Defaults,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct Environment {
    /// AWS profile; empty means environment variables / `AWS_PROFILE` / `default`.
    #[serde(default)]
    pub profile: String,
    /// Empty means the profile's own region.
    #[serde(default)]
    pub region: String,
    #[serde(default)]
    pub pipelines: Vec<PipelineConfig>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct PipelineConfig {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Groups pipelines of different environments into one row of the matrix when their
    /// names do not already match (`orders-uat` and `orders-prod` match as `orders`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app: Option<String>,
    #[serde(default, skip_serializing_if = "Hooks::is_empty")]
    pub hooks: Hooks,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct Hooks {
    #[serde(rename = "pre-deploy", default, skip_serializing_if = "Vec::is_empty")]
    pub pre_deploy: Vec<Hook>,
    #[serde(rename = "post-deploy", default, skip_serializing_if = "Vec::is_empty")]
    pub post_deploy: Vec<Hook>,
}

impl Hooks {
    pub fn is_empty(&self) -> bool {
        self.pre_deploy.is_empty() && self.post_deploy.is_empty()
    }
}

/// A hook is either a bare command string or `{ command, description }`.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(from = "HookRepr")]
pub struct Hook {
    pub command: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum HookRepr {
    Command(String),
    Full { command: String, description: Option<String> },
}

impl From<HookRepr> for Hook {
    fn from(r: HookRepr) -> Self {
        match r {
            HookRepr::Command(command) => Hook { command, description: None },
            HookRepr::Full { command, description } => Hook { command, description },
        }
    }
}

impl Hook {
    /// The description, else the command (made safe to print).
    pub fn label(&self) -> String {
        match &self.description {
            Some(d) => crate::util::sanitize(d),
            None => crate::util::show_invisible(&self.command),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Defaults {
    pub poll_interval_seconds: u64,
    pub notifications: bool,
}

impl Default for Defaults {
    fn default() -> Self {
        Defaults { poll_interval_seconds: 10, notifications: true }
    }
}

/// Where the config came from.
#[derive(Debug, Clone)]
pub enum Source {
    File(PathBuf),
    /// No config file: one environment per AWS profile, without pipelines.
    AwsProfiles,
}

#[derive(Debug, Clone)]
pub struct Loaded {
    pub config: Config,
    pub source: Source,
    /// Found in the current directory rather than the home directory or `--config`: it may
    /// come from a cloned repository, so its hooks are shown before they run.
    pub from_cwd: bool,
}

fn config_dir() -> Option<PathBuf> {
    Some(home_dir()?.join(".deployment-cli"))
}

pub fn primary_path() -> Option<PathBuf> {
    Some(config_dir()?.join("config.yaml"))
}

/// Holds the environment chosen with `use`. Kept out of config.yaml because the Node
/// version rewrites that file and would drop unknown keys.
fn current_env_path() -> Option<PathBuf> {
    Some(config_dir()?.join("current-env"))
}

fn current_env_name() -> Option<String> {
    let text = std::fs::read_to_string(current_env_path()?).ok()?;
    Some(text.trim().to_string()).filter(|s| !s.is_empty())
}

pub fn set_current_env(name: &str) -> Result<()> {
    let path = current_env_path().ok_or_else(|| anyhow!("cannot find the home directory"))?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&path, format!("{name}\n")).with_context(|| format!("cannot write {}", path.display()))
}

fn search_paths() -> Vec<PathBuf> {
    let cwd = std::env::current_dir().unwrap_or_default();
    primary_path()
        .into_iter()
        .chain([cwd.join("deployment-cli.yaml"), cwd.join(".deployment-cli.yaml")])
        .collect()
}

pub fn load(explicit: Option<&Path>) -> Result<Loaded> {
    let path = match explicit {
        Some(p) => Some(p.to_path_buf()),
        None => search_paths().into_iter().find(|p| p.exists()),
    };
    let Some(path) = path else {
        return Ok(Loaded { config: from_aws_profiles(), source: Source::AwsProfiles, from_cwd: false });
    };
    let from_cwd = explicit.is_none() && Some(&path) != primary_path().as_ref();
    let text = std::fs::read_to_string(&path).with_context(|| format!("cannot read {}", path.display()))?;
    let mut config: Config =
        serde_saphyr::from_str(&text).map_err(|e| anyhow!("invalid config {}:\n{e}", path.display()))?;
    config.sanitize_text();
    config.resolve_regions();
    config.validate().with_context(|| format!("invalid config {}", path.display()))?;
    Ok(Loaded { config, source: Source::File(path), from_cwd })
}

fn from_aws_profiles() -> Config {
    let files = AwsFiles::load();
    let mut config = Config::default();
    for name in files.profile_names() {
        config.environments.insert(name.clone(), Environment { profile: name, ..Default::default() });
    }
    config.resolve_regions();
    config
}

impl Config {
    /// Names and descriptions end up in the terminal; strip control sequences from them.
    /// Hook commands are left as written (they run, and are sanitized when shown).
    fn sanitize_text(&mut self) {
        use crate::util::sanitize;
        self.environments = std::mem::take(&mut self.environments)
            .into_iter()
            .map(|(name, mut env)| {
                env.profile = sanitize(&env.profile);
                env.region = sanitize(&env.region);
                for p in &mut env.pipelines {
                    p.name = sanitize(&p.name);
                    p.description = p.description.as_deref().map(sanitize);
                    p.app = p.app.as_deref().map(sanitize);
                    for h in p.hooks.pre_deploy.iter_mut().chain(&mut p.hooks.post_deploy) {
                        h.description = h.description.as_deref().map(sanitize);
                    }
                }
                (sanitize(&name), env)
            })
            .collect();
    }

    /// Fills empty regions from the AWS profile (or `AWS_REGION`).
    fn resolve_regions(&mut self) {
        let files = AwsFiles::load();
        let fallback = std::env::var("AWS_REGION").or_else(|_| std::env::var("AWS_DEFAULT_REGION")).ok();
        for env in self.environments.values_mut() {
            if env.region.is_empty() {
                let profile = crate::aws::credentials::effective_profile(&env.profile);
                env.region = files
                    .profile(&profile)
                    .and_then(|p| p.get("region").cloned())
                    .or_else(|| fallback.clone())
                    .unwrap_or_default();
            }
        }
    }

    fn validate(&self) -> Result<()> {
        if self.environments.is_empty() {
            bail!("no environments defined");
        }
        for (name, env) in &self.environments {
            if env.region.is_empty() {
                bail!("environment '{name}' has no region (set 'region' or a region in its AWS profile)");
            }
            if let Some(p) = env.pipelines.iter().find(|p| p.name.is_empty()) {
                bail!("environment '{name}' has a pipeline without a name ({p:?})");
            }
        }
        Ok(())
    }

    /// The environment used when none is given: the one chosen with `use`, else the first.
    pub fn default_env(&self) -> Option<&str> {
        current_env_name()
            .and_then(|n| self.environments.get_key_value(&n).map(|(k, _)| k.as_str()))
            .or_else(|| self.environments.keys().next().map(String::as_str))
    }

    pub fn env(&self, name: &str) -> Result<&Environment> {
        self.environments.get(name).ok_or_else(|| {
            let names: Vec<&str> = self.environments.keys().map(String::as_str).collect();
            anyhow!("unknown environment '{name}'. Available: {}", names.join(", "))
        })
    }

    pub fn pipeline<'a>(&'a self, env_name: &str, pipeline: Option<&str>) -> Result<&'a PipelineConfig> {
        let env = self.env(env_name)?;
        let names = || env.pipelines.iter().map(|p| p.name.as_str()).collect::<Vec<_>>().join(", ");
        match pipeline {
            None if env.pipelines.len() == 1 => Ok(&env.pipelines[0]),
            None if env.pipelines.is_empty() => bail!("environment '{env_name}' has no pipelines configured"),
            None => bail!("environment '{env_name}' has several pipelines; pick one: {}", names()),
            Some(name) => env
                .pipelines
                .iter()
                .find(|p| p.name == name)
                .ok_or_else(|| anyhow!("pipeline '{name}' not found in '{env_name}'. Available: {}", names())),
        }
    }
}

// ---------------------------------------------------------------------------
// Editing. Changes go through a generic YAML document, so keys this version does not
// know survive; comments and formatting do not (the Node version also rewrites the file).
// ---------------------------------------------------------------------------

impl Loaded {
    /// The file edits are written to: the loaded one, or the primary path when there was none.
    pub fn edit_path(&self) -> Result<PathBuf> {
        match &self.source {
            Source::File(p) => Ok(p.clone()),
            Source::AwsProfiles => primary_path().ok_or_else(|| anyhow!("cannot find the home directory")),
        }
    }

    /// Adds pipelines to an environment, saves and reloads. An environment that is not in
    /// the file yet (a new one, or an AWS profile shown without config) is written from
    /// its in-memory definition. Returns the file written and how many were new.
    pub fn add_pipelines(&mut self, env_name: &str, names: &[String]) -> Result<(PathBuf, usize)> {
        let env = self.config.env(env_name)?.clone();
        let mut added = 0;
        let path = self.edit(|root| {
            let pipelines = env_pipelines(root, env_name, &env)?;
            for name in names {
                if !pipelines.iter().any(|p| p.get("name").and_then(Value::as_str) == Some(name)) {
                    pipelines.push(json!({ "name": name }));
                    added += 1;
                }
            }
            Ok(())
        })?;
        Ok((path, added))
    }

    /// Removes a pipeline from an environment, saves and reloads.
    pub fn remove_pipeline(&mut self, env_name: &str, name: &str) -> Result<PathBuf> {
        let env = self.config.env(env_name)?.clone();
        self.edit(|root| {
            let pipelines = env_pipelines(root, env_name, &env)?;
            let before = pipelines.len();
            pipelines.retain(|p| p.get("name").and_then(Value::as_str) != Some(name));
            if pipelines.len() == before {
                bail!("pipeline '{name}' is not in '{env_name}'");
            }
            Ok(())
        })
    }

    fn edit(&mut self, change: impl FnOnce(&mut Map<String, Value>) -> Result<()>) -> Result<PathBuf> {
        let path = self.edit_path()?;
        let original = std::fs::read_to_string(&path).ok();
        let mut doc: Value = match &original {
            Some(text) => {
                serde_saphyr::from_str(text).map_err(|e| anyhow!("invalid config {}:\n{e}", path.display()))?
            }
            None => json!({ "environments": {}, "defaults": self.config.defaults }),
        };
        let root = doc.as_object_mut().ok_or_else(|| anyhow!("{} is not a YAML mapping", path.display()))?;
        change(root)?;
        let yaml = serde_saphyr::to_string(&doc).map_err(|e| anyhow!("cannot write YAML: {e}"))?;

        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let write = |p: &Path, text: &str| std::fs::write(p, text).with_context(|| format!("cannot write {}", p.display()));
        if let Some(text) = &original {
            write(&path.with_extension("yaml.bak"), text)?;
        }
        // Replace atomically, unless the config is a symlink (dotfiles) that must stay one.
        let is_link = std::fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_symlink());
        if is_link {
            write(&path, &yaml)?;
        } else {
            let tmp = path.with_extension("yaml.tmp");
            write(&tmp, &yaml)?;
            std::fs::rename(&tmp, &path).with_context(|| format!("cannot replace {}", path.display()))?;
        }
        let from_cwd = self.from_cwd;
        *self = load(Some(&path))?;
        self.from_cwd = from_cwd;
        Ok(path)
    }
}

/// `environments.<name>.pipelines` of a YAML document, creating what is missing.
fn env_pipelines<'a>(root: &'a mut Map<String, Value>, name: &str, env: &Environment) -> Result<&'a mut Vec<Value>> {
    let envs = root.entry("environments").or_insert_with(|| json!({}));
    if envs.is_null() {
        *envs = json!({});
    }
    let envs = envs.as_object_mut().ok_or_else(|| anyhow!("'environments' is not a mapping"))?;
    let entry = envs
        .entry(name)
        .or_insert_with(|| json!({ "profile": env.profile, "region": env.region, "pipelines": [] }))
        .as_object_mut()
        .ok_or_else(|| anyhow!("environment '{name}' is not a mapping"))?;
    let pipelines = entry.entry("pipelines").or_insert_with(|| json!([]));
    if pipelines.is_null() {
        *pipelines = json!([]);
    }
    pipelines.as_array_mut().ok_or_else(|| anyhow!("'{name}.pipelines' is not a list"))
}

/// Environments whose name suggests production get an extra typed confirmation.
pub fn is_production(env_name: &str) -> bool {
    let n = env_name.to_ascii_lowercase();
    n.contains("prod") || n == "prd" || n.contains("live")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_node_version_config() {
        let yaml = r#"
environments:
  qa:
    profile: corp-qa
    region: us-east-1
    pipelines:
      - name: app-qa
        description: "Main app"
        hooks:
          pre-deploy:
            - npm test
            - command: npm run lint
              description: Lint
  dev:
    profile: corp-dev
    region: us-west-2
    pipelines:
      - name: app-dev
defaults:
  pollIntervalSeconds: 5
"#;
        let c: Config = serde_saphyr::from_str(yaml).unwrap();
        // Order is preserved: qa before dev.
        assert_eq!(c.environments.keys().collect::<Vec<_>>(), vec!["qa", "dev"]);
        let qa = &c.environments["qa"].pipelines[0];
        assert_eq!(qa.hooks.pre_deploy.len(), 2);
        assert_eq!(qa.hooks.pre_deploy[0].command, "npm test");
        assert_eq!(qa.hooks.pre_deploy[1].label(), "Lint");
        assert_eq!(c.defaults.poll_interval_seconds, 5);
        assert!(c.defaults.notifications);
        assert!(c.pipeline("qa", None).is_ok());
        assert!(c.pipeline("dev", Some("nope")).is_err());
    }

    #[test]
    fn edits_keep_unknown_keys_and_hooks() {
        let dir = std::env::temp_dir().join(format!("dcli-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.yaml");
        std::fs::write(
            &path,
            r#"# my comment
environments:
  dev:
    profile: corp-dev
    region: us-east-1
    team: platform
    pipelines:
      - name: app-dev
        hooks:
          pre-deploy:
            - npm test
  prod:
    profile: "123"
    region: us-east-1
    pipelines: []
defaults:
  pollIntervalSeconds: 5
  notifications: false
"#,
        )
        .unwrap();
        let mut loaded = load(Some(&path)).unwrap();
        let (_, added) = loaded.add_pipelines("dev", &["api-dev".into(), "app-dev".into()]).unwrap();
        assert_eq!(added, 1);
        loaded.add_pipelines("prod", &["true".into()]).unwrap();
        loaded.remove_pipeline("dev", "nope").unwrap_err();
        loaded.add_pipelines("dev", &["worker-dev".into()]).unwrap();
        loaded.remove_pipeline("dev", "worker-dev").unwrap();

        let c = &loaded.config;
        let names: Vec<&str> = c.environments["dev"].pipelines.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["app-dev", "api-dev"]);
        assert_eq!(c.environments["dev"].pipelines[0].hooks.pre_deploy[0].command, "npm test");
        assert_eq!(c.environments["prod"].profile, "123");
        assert_eq!(c.environments["prod"].pipelines[0].name, "true");
        assert_eq!(c.defaults.poll_interval_seconds, 5);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("team: platform"), "{text}");
        assert!(path.with_extension("yaml.bak").exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
