//! AppConfig (restJson1): applications, environments, configuration profiles, hosted
//! configuration versions (the documents, stored exactly as sent) and deployments. Below
//! the calls, the feature flags document: values per flag, edits and their diff.

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use super::{Aws, Result, Service};
use crate::util::parse_rfc3339;

pub const SERVICE: Service = Service { endpoint: "appconfig", signing_name: "appconfig", target_prefix: "" };

/// The profile type whose documents are feature flags.
pub const FEATURE_FLAGS: &str = "AWS.AppConfig.FeatureFlags";
/// The predefined strategy that deploys at once.
pub const ALL_AT_ONCE: &str = "AppConfig.AllAtOnce";

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct Application {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct Environment {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    /// `READY_FOR_DEPLOYMENT`, `DEPLOYING`, `ROLLING_BACK`, `ROLLED_BACK`.
    pub state: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct Profile {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    /// `AWS.AppConfig.FeatureFlags` or `AWS.Freeform`.
    #[serde(rename = "Type")]
    pub kind: Option<String>,
    pub location_uri: Option<String>,
}

impl Profile {
    pub fn is_feature_flags(&self) -> bool {
        self.kind.as_deref() == Some(FEATURE_FLAGS)
    }

    /// Hosted in AppConfig (versions dcli can read and create), not in S3 or SSM.
    pub fn is_hosted(&self) -> bool {
        self.location_uri.as_deref().is_none_or(|l| l == "hosted")
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct Deployment {
    pub deployment_number: i64,
    /// The profile's name (deployment summaries carry no profile id).
    pub configuration_name: String,
    pub configuration_profile_id: Option<String>,
    pub configuration_version: String,
    pub deployment_strategy_id: Option<String>,
    pub description: Option<String>,
    /// `DEPLOYING`, `BAKING`, `VALIDATING`, `COMPLETE`, `ROLLING_BACK`, `ROLLED_BACK`, `REVERTED`.
    pub state: String,
    pub percentage_complete: Option<f64>,
    pub deployment_duration_in_minutes: Option<f64>,
    pub final_bake_time_in_minutes: Option<f64>,
    /// ISO 8601 strings (or epoch seconds): see [`Deployment::started`].
    pub started_at: Option<Value>,
    pub completed_at: Option<Value>,
}

fn timestamp(v: &Option<Value>) -> Option<f64> {
    match v.as_ref()? {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => parse_rfc3339(s),
        _ => None,
    }
}

impl Deployment {
    pub fn version(&self) -> Option<i64> {
        self.configuration_version.trim().parse().ok()
    }

    pub fn started(&self) -> Option<f64> {
        timestamp(&self.started_at)
    }

    pub fn completed(&self) -> Option<f64> {
        timestamp(&self.completed_at)
    }

    /// Still changing what the environment serves (or able to roll back).
    pub fn is_active(&self) -> bool {
        matches!(self.state.as_str(), "DEPLOYING" | "VALIDATING" | "BAKING" | "ROLLING_BACK")
    }

    /// The version is fully out: `COMPLETE`, or `BAKING` (at 100%, watched for alarms).
    pub fn is_deployed(&self) -> bool {
        matches!(self.state.as_str(), "COMPLETE" | "BAKING")
    }

    /// Can be stopped (which rolls it back).
    pub fn can_stop(&self) -> bool {
        matches!(self.state.as_str(), "DEPLOYING" | "VALIDATING" | "BAKING")
    }

    /// `deploying 40%`, `baking`, `rolling back`…
    pub fn progress(&self) -> String {
        match self.state.as_str() {
            "DEPLOYING" => format!("deploying {:.0}%", self.percentage_complete.unwrap_or(0.0)),
            "VALIDATING" => "validating".into(),
            "BAKING" => "baking".into(),
            "ROLLING_BACK" => "rolling back".into(),
            other => other.to_lowercase().replace('_', " "),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct HostedVersion {
    pub version_number: i64,
    pub description: Option<String>,
    pub content_type: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct Strategy {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    pub deployment_duration_in_minutes: Option<f64>,
    /// `LINEAR` or `EXPONENTIAL`.
    pub growth_type: Option<String>,
    pub growth_factor: Option<f64>,
    pub final_bake_time_in_minutes: Option<f64>,
}

impl Strategy {
    /// `all at once`, `20 min · linear 10% · bake 10 min`.
    pub fn summary(&self) -> String {
        let minutes = self.deployment_duration_in_minutes.unwrap_or(0.0);
        let bake = self.final_bake_time_in_minutes.unwrap_or(0.0);
        let mut text = if minutes <= 0.0 {
            "all at once".to_string()
        } else {
            let growth = self.growth_type.as_deref().unwrap_or("LINEAR").to_lowercase();
            format!("{minutes:.0} min · {growth} {:.0}%", self.growth_factor.unwrap_or(100.0))
        };
        if bake > 0.0 {
            text.push_str(&format!(" · bake {bake:.0} min"));
        }
        text
    }
}

/// Every page of a list operation (`Items` / `NextToken`).
fn list<T: DeserializeOwned>(aws: &Aws, segments: &[&str], max_pages: usize) -> Result<Vec<T>> {
    #[derive(Deserialize)]
    #[serde(rename_all = "PascalCase")]
    struct Page<T> {
        #[serde(default = "Vec::new")]
        items: Vec<T>,
        #[serde(default)]
        next_token: Option<String>,
    }
    let mut all = Vec::new();
    let mut token: Option<String> = None;
    for _ in 0..max_pages {
        let mut query = vec![("max_results".to_string(), "50".to_string())];
        if let Some(t) = &token {
            query.push(("next_token".to_string(), t.clone()));
        }
        let page: Page<T> = aws.rest(&SERVICE, "GET", segments, &query, None)?;
        all.extend(page.items);
        match page.next_token.filter(|t| !t.is_empty()) {
            Some(t) => token = Some(t),
            None => break,
        }
    }
    Ok(all)
}

pub fn list_applications(aws: &Aws) -> Result<Vec<Application>> {
    let mut apps: Vec<Application> = list(aws, &["applications"], 20)?;
    apps.sort_by_key(|a| a.name.to_lowercase());
    Ok(apps)
}

pub fn list_environments(aws: &Aws, app: &str) -> Result<Vec<Environment>> {
    list(aws, &["applications", app, "environments"], 20)
}

/// Feature-flag profiles first, then the rest, each group by name.
pub fn list_profiles(aws: &Aws, app: &str) -> Result<Vec<Profile>> {
    let mut profiles: Vec<Profile> = list(aws, &["applications", app, "configurationprofiles"], 20)?;
    profiles.sort_by_key(|p| (!p.is_feature_flags(), p.name.to_lowercase()));
    Ok(profiles)
}

/// Deployments of an environment, newest first (the last `max_pages` × 50).
pub fn list_deployments(aws: &Aws, app: &str, env: &str, max_pages: usize) -> Result<Vec<Deployment>> {
    let mut deployments: Vec<Deployment> = list(aws, &["applications", app, "environments", env, "deployments"], max_pages)?;
    deployments.sort_by_key(|d| std::cmp::Reverse(d.deployment_number));
    Ok(deployments)
}

pub fn get_deployment(aws: &Aws, app: &str, env: &str, number: i64) -> Result<Deployment> {
    let n = number.to_string();
    aws.rest(&SERVICE, "GET", &["applications", app, "environments", env, "deployments", &n], &[], None)
}

pub fn list_hosted_versions(aws: &Aws, app: &str, profile: &str) -> Result<Vec<HostedVersion>> {
    list(aws, &["applications", app, "configurationprofiles", profile, "hostedconfigurationversions"], 20)
}

/// Number of the newest hosted version, if any.
pub fn latest_version(aws: &Aws, app: &str, profile: &str) -> Result<Option<i64>> {
    Ok(list_hosted_versions(aws, app, profile)?.into_iter().map(|v| v.version_number).max())
}

/// The document of a hosted version, exactly as stored.
pub fn get_hosted_version(aws: &Aws, app: &str, profile: &str, version: i64) -> Result<String> {
    let n = version.to_string();
    let segments = ["applications", app, "configurationprofiles", profile, "hostedconfigurationversions", &n];
    let resp = aws.raw(&SERVICE, "GET", &segments, &[], Vec::new(), Vec::new())?;
    Ok(String::from_utf8_lossy(&resp.body).into_owned())
}

/// Stores `content` as a new hosted version and returns its number. `latest` is the
/// newest version the caller knows: AppConfig refuses the write when another version was
/// created meanwhile.
pub fn create_hosted_version(
    aws: &Aws,
    app: &str,
    profile: &str,
    content: &str,
    description: &str,
    latest: Option<i64>,
) -> Result<i64> {
    let segments = ["applications", app, "configurationprofiles", profile, "hostedconfigurationversions"];
    let mut headers = vec![("content-type".to_string(), "application/json".to_string())];
    // Header values must stay printable ASCII.
    let description: String = description.chars().filter(|c| c.is_ascii() && !c.is_ascii_control()).take(1024).collect();
    if !description.is_empty() {
        headers.push(("description".to_string(), description));
    }
    if let Some(n) = latest {
        headers.push(("latest-version-number".to_string(), n.to_string()));
    }
    let resp = aws.raw(&SERVICE, "POST", &segments, &[], headers, content.as_bytes().to_vec())?;
    resp.header("version-number")
        .and_then(|v| v.trim().parse().ok())
        .ok_or_else(|| super::AwsError::Network("AppConfig did not return the new version number".into()))
}

/// A configuration profile name AppConfig takes: 1 to 128 characters on one line.
pub fn valid_profile_name(name: &str) -> std::result::Result<(), String> {
    match name.trim().chars().count() {
        0 => Err("The profile needs a name".into()),
        n if n > 128 || !plain(name) => Err("A profile name takes up to 128 characters on one line".into()),
        _ => Ok(()),
    }
}

/// Creates a feature flags profile hosted in AppConfig.
pub fn create_flags_profile(aws: &Aws, app: &str, name: &str, description: &str) -> Result<Profile> {
    let mut body = json!({ "Name": name.trim(), "LocationUri": "hosted", "Type": FEATURE_FLAGS });
    if !description.trim().is_empty() {
        body["Description"] = json!(description.trim());
    }
    // Sent once: a retry after a lost answer would create a second profile.
    aws.rest_once(&SERVICE, "POST", &["applications", app, "configurationprofiles"], &[], Some(&body))
}

pub fn list_strategies(aws: &Aws) -> Result<Vec<Strategy>> {
    list(aws, &["deploymentstrategies"], 10)
}

/// Starts deploying `version` of `profile` to `env` with `strategy`.
pub fn start_deployment(
    aws: &Aws,
    app: &str,
    env: &str,
    profile: &str,
    version: i64,
    strategy: &str,
    description: &str,
) -> Result<Deployment> {
    let body = json!({
        "DeploymentStrategyId": strategy,
        "ConfigurationProfileId": profile,
        "ConfigurationVersion": version.to_string(),
        "Description": description,
    });
    aws.rest(&SERVICE, "POST", &["applications", app, "environments", env, "deployments"], &[], Some(&body))
}

/// Stops a deployment; AppConfig rolls the environment back to the previous version.
pub fn stop_deployment(aws: &Aws, app: &str, env: &str, number: i64) -> Result<()> {
    let n = number.to_string();
    let _: Value = aws.rest(&SERVICE, "DELETE", &["applications", app, "environments", env, "deployments", &n], &[], None)?;
    Ok(())
}

/// What an environment serves for a profile, from its deployments (newest first).
#[derive(Debug, Clone, Default)]
pub struct EnvState {
    /// The newest fully deployed version (`COMPLETE` or `BAKING`).
    pub deployed: Option<Deployment>,
    /// A deployment still in progress (or rolling back), at least as new as `deployed`.
    pub active: Option<Deployment>,
}

impl EnvState {
    /// Matches deployments by profile id when AWS reports it, by name otherwise (two
    /// profiles may share a name across applications' history).
    pub fn of(deployments: &[Deployment], profile_id: &str, profile_name: &str) -> EnvState {
        let mine = || {
            deployments.iter().filter(|d| match d.configuration_profile_id.as_deref() {
                Some(id) => id == profile_id,
                None => d.configuration_name == profile_name,
            })
        };
        let deployed = mine().find(|d| d.is_deployed()).cloned();
        let floor = deployed.as_ref().map_or(i64::MIN, |d| d.deployment_number);
        let active = mine().find(|d| d.is_active() && d.deployment_number >= floor).cloned();
        EnvState { deployed, active }
    }

    pub fn version(&self) -> Option<i64> {
        self.deployed.as_ref().and_then(Deployment::version)
    }
}

// ---------------------------------------------------------------------------------------
// Feature flags documents:
//   {"version": "1",
//    "flags":  {"<key>": {"name": …, "description": …, "attributes": {"<attr>": {"constraints": {"type": …}}}}},
//    "values": {"<key>": {"enabled": true, "<attr>": <value>, "_variants": […]}}}
// Kept as JSON values, so keys dcli does not know (`_createdAt`, variants…) survive an edit.
// ---------------------------------------------------------------------------------------

/// Attribute types as dcli names them. AppConfig stores the arrays as `{"type": "array",
/// "elements": {"type": "string"}}`.
pub const ATTRIBUTE_TYPES: &[&str] = &["string", "number", "boolean", "string[]", "number[]"];
/// Most attributes a flag can have.
pub const MAX_ATTRIBUTES: usize = 25;

/// A flag or attribute key as AppConfig takes it: a lowercase letter, then up to 63
/// letters, digits, `_` or `-`.
pub fn valid_key(key: &str) -> bool {
    let mut chars = key.chars();
    chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && key.len() <= 64
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// Text typed for AppConfig: no control characters (it is shown in terminals).
fn plain(text: &str) -> bool {
    !text.chars().any(char::is_control)
}

/// `YYYY-MM-DD`.
fn is_date(text: &str) -> bool {
    let b = text.as_bytes();
    b.len() == 10
        && b.iter().enumerate().all(|(i, c)| if i == 4 || i == 7 { *c == b'-' } else { c.is_ascii_digit() })
        && (1..=12).contains(&text[5..7].parse::<u32>().unwrap_or(0))
        && (1..=31).contains(&text[8..10].parse::<u32>().unwrap_or(0))
}

/// What an attribute's `constraints` allow; AppConfig checks values against them when a
/// version is created. For arrays, `allowed`, `pattern` and the range apply to each element.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Constraints {
    /// `string`, `number`, `boolean`, `string[]`, `number[]`, or `array` (elements of any
    /// type); `None` when the attribute has no constraints.
    pub kind: Option<String>,
    pub required: bool,
    /// Allowed values (`enum`): strings and string arrays.
    pub allowed: Vec<Value>,
    /// A regular expression for strings and string arrays (AppConfig checks it).
    pub pattern: Option<String>,
    /// Numbers and number arrays.
    pub minimum: Option<i64>,
    pub maximum: Option<i64>,
}

impl Constraints {
    pub fn of(kind: &str) -> Constraints {
        Constraints { kind: Some(kind.to_string()), ..Default::default() }
    }

    /// Reads `{"type", "required", "enum", "pattern", "minimum", "maximum", "elements"}`.
    pub fn parse(c: &Value) -> Constraints {
        let text = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).map(str::to_string);
        // An array's limits are its elements'.
        let (kind, limits) = match (text(c, "type").as_deref(), c.get("elements")) {
            (Some("array"), Some(e)) => {
                let kind = match text(e, "type").as_deref() {
                    Some("number") => "number[]",
                    Some("string") => "string[]",
                    _ => "array",
                };
                (Some(kind.to_string()), e)
            }
            (kind, _) => (kind.map(str::to_string), c),
        };
        Constraints {
            kind,
            required: c.get("required").and_then(Value::as_bool).unwrap_or(false),
            allowed: limits.get("enum").and_then(Value::as_array).cloned().unwrap_or_default(),
            pattern: text(limits, "pattern"),
            minimum: limits.get("minimum").and_then(Value::as_i64),
            maximum: limits.get("maximum").and_then(Value::as_i64),
        }
    }

    /// The `constraints` object AppConfig takes.
    pub fn to_json(&self) -> Value {
        let limits = |kind: &str| {
            let mut m = Map::new();
            m.insert("type".into(), json!(kind));
            if !self.allowed.is_empty() {
                m.insert("enum".into(), Value::Array(self.allowed.clone()));
            }
            if let Some(p) = &self.pattern {
                m.insert("pattern".into(), json!(p));
            }
            if let Some(n) = self.minimum {
                m.insert("minimum".into(), json!(n));
            }
            if let Some(n) = self.maximum {
                m.insert("maximum".into(), json!(n));
            }
            m
        };
        let kind = self.kind.as_deref().unwrap_or("string");
        let mut out = match kind.strip_suffix("[]") {
            Some(element) => {
                let mut m = Map::new();
                m.insert("type".into(), json!("array"));
                m.insert("elements".into(), Value::Object(limits(element)));
                m
            }
            None => limits(kind),
        };
        if self.required {
            out.insert("required".into(), Value::Bool(true));
        }
        Value::Object(out)
    }

    /// `number · required · 0..100`, `string[] · one of dhl, fedex`.
    pub fn summary(&self) -> String {
        let mut parts = vec![self.kind.clone().unwrap_or_else(|| "any".into())];
        if self.required {
            parts.push("required".into());
        }
        if !self.allowed.is_empty() {
            parts.push(format!("one of {}", self.allowed.iter().map(value_text).collect::<Vec<_>>().join(", ")));
        }
        if let Some(p) = &self.pattern {
            parts.push(format!("pattern {p}"));
        }
        match (self.minimum, self.maximum) {
            (Some(a), Some(b)) => parts.push(format!("{a}..{b}")),
            (Some(a), None) => parts.push(format!("≥ {a}")),
            (None, Some(b)) => parts.push(format!("≤ {b}")),
            _ => {}
        }
        parts.join(" · ")
    }

    /// The constraints themselves as AppConfig takes them.
    pub fn validate(&self) -> std::result::Result<(), String> {
        let kind = self.kind.as_deref().unwrap_or("");
        if !ATTRIBUTE_TYPES.contains(&kind) {
            return Err(format!("unknown type '{kind}' (string, number, boolean, string[], number[])"));
        }
        let strings = matches!(kind, "string" | "string[]");
        let numbers = matches!(kind, "number" | "number[]");
        if !strings && (!self.allowed.is_empty() || self.pattern.is_some()) {
            return Err(format!("allowed values and patterns are for text, not {kind}"));
        }
        if !numbers && (self.minimum.is_some() || self.maximum.is_some()) {
            return Err(format!("a minimum or maximum is for numbers, not {kind}"));
        }
        if !self.allowed.is_empty() && self.pattern.is_some() {
            return Err("give allowed values or a pattern, not both".into());
        }
        if self.allowed.len() > 100 {
            return Err("at most 100 allowed values".into());
        }
        if self.allowed.iter().any(|v| !v.as_str().is_some_and(|s| !s.is_empty() && s.chars().count() <= 1024 && plain(s))) {
            return Err("allowed values are text of 1 to 1024 characters".into());
        }
        if self.pattern.as_deref().is_some_and(|p| p.is_empty() || p.chars().count() > 1024 || !plain(p)) {
            return Err("the pattern takes 1 to 1024 characters".into());
        }
        if let (Some(a), Some(b)) = (self.minimum, self.maximum)
            && a > b
        {
            return Err(format!("the minimum ({a}) is above the maximum ({b})"));
        }
        Ok(())
    }

    /// Whether `value` fits; the pattern is left to AppConfig.
    pub fn check(&self, value: Option<&Value>) -> std::result::Result<(), String> {
        let Some(value) = value else {
            return if self.required { Err("a value is required".into()) } else { Ok(()) };
        };
        let element = |v: &Value, kind: &str| -> std::result::Result<(), String> {
            match kind {
                "string" => {
                    let Some(s) = v.as_str() else { return Err(format!("{} is not text", value_text(v))) };
                    if s.chars().count() > 1024 || !plain(s) {
                        return Err("text takes up to 1024 characters on one line".into());
                    }
                    if !self.allowed.is_empty() && !self.allowed.contains(v) {
                        let allowed: Vec<String> = self.allowed.iter().map(value_text).collect();
                        return Err(format!("'{s}' is not one of {}", allowed.join(", ")));
                    }
                    Ok(())
                }
                "number" => {
                    let Some(n) = v.as_f64() else { return Err(format!("{} is not a number", value_text(v))) };
                    if let Some(min) = self.minimum.filter(|m| n < *m as f64) {
                        return Err(format!("{} is below the minimum, {min}", value_text(v)));
                    }
                    if let Some(max) = self.maximum.filter(|m| n > *m as f64) {
                        return Err(format!("{} is above the maximum, {max}", value_text(v)));
                    }
                    Ok(())
                }
                "boolean" if !v.is_boolean() => Err(format!("{} is not true or false", value_text(v))),
                _ => Ok(()),
            }
        };
        match self.kind.as_deref() {
            Some(k @ ("string[]" | "number[]")) => {
                let Some(items) = value.as_array() else { return Err(format!("{} is not a list", value_text(value))) };
                items.iter().try_for_each(|v| element(v, &k[..k.len() - 2]))
            }
            Some("array") if !value.is_array() => Err(format!("{} is not a list", value_text(value))),
            Some(k) => element(value, k),
            None => Ok(()),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Attribute {
    pub name: String,
    pub description: Option<String>,
    pub constraints: Constraints,
}

impl Attribute {
    /// `string`, `number`, `boolean`, `string[]`, `number[]` or `array`, when constrained.
    pub fn kind(&self) -> Option<&str> {
        self.constraints.kind.as_deref()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Flag {
    pub key: String,
    pub name: String,
    pub description: Option<String>,
    pub attributes: Vec<Attribute>,
    /// A short-term flag (`_deprecation`): the date to retire it by, empty when none was set.
    pub retire_by: Option<String>,
}

/// A flag to add to a version: its definition and its value there.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct NewFlag {
    pub key: String,
    /// The name shown in consoles; the key when empty.
    pub name: String,
    pub description: String,
    pub enabled: bool,
    /// A short-term flag: the date to retire it by (`YYYY-MM-DD`), empty for none.
    pub retire_by: Option<String>,
    pub attributes: Vec<NewAttribute>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct NewAttribute {
    pub name: String,
    pub description: String,
    pub constraints: Constraints,
    pub value: Option<Value>,
}

impl NewAttribute {
    /// What AppConfig would refuse about this attribute.
    pub fn validate(&self) -> std::result::Result<(), String> {
        if self.name == "enabled" {
            return Err("'enabled' is reserved: name the attribute otherwise".into());
        }
        if !valid_key(&self.name) {
            return Err(format!(
                "'{}' is not a valid attribute name: a lowercase letter, then up to 63 letters, digits, _ or -",
                crate::util::sanitize(&self.name)
            ));
        }
        if self.description.chars().count() > 1024 || !plain(&self.description) {
            return Err(format!("{}: the description takes up to 1024 characters on one line", self.name));
        }
        self.constraints.validate().map_err(|e| format!("{}: {e}", self.name))?;
        self.constraints.check(self.value.as_ref()).map_err(|e| format!("{}: {e}", self.name))
    }
}

impl NewFlag {
    pub fn name(&self) -> &str {
        match self.name.trim() {
            "" => &self.key,
            name => name,
        }
    }

    /// What AppConfig would refuse, checked before anything is sent.
    pub fn validate(&self) -> std::result::Result<(), String> {
        if !valid_key(&self.key) {
            return Err(format!(
                "'{}' is not a valid key: a lowercase letter, then up to 63 letters, digits, _ or -",
                crate::util::sanitize(&self.key)
            ));
        }
        if self.name().chars().count() > 64 || !plain(self.name()) {
            return Err("The name takes up to 64 characters on one line".into());
        }
        if self.description.chars().count() > 1024 || !plain(&self.description) {
            return Err("The description takes up to 1024 characters on one line".into());
        }
        if self.retire_by.as_deref().is_some_and(|d| !d.is_empty() && !is_date(d)) {
            return Err("Retire by takes a date: YYYY-MM-DD".into());
        }
        if self.attributes.len() > MAX_ATTRIBUTES {
            return Err(format!("A flag takes at most {MAX_ATTRIBUTES} attributes"));
        }
        for (i, a) in self.attributes.iter().enumerate() {
            if self.attributes[..i].iter().any(|b| b.name == a.name) {
                return Err(format!("Two attributes are named {}", a.name));
            }
            a.validate()?;
        }
        Ok(())
    }

    /// The flag's entry under `flags`.
    pub fn definition(&self) -> Value {
        let mut def = Map::new();
        def.insert("name".into(), json!(self.name()));
        if !self.description.trim().is_empty() {
            def.insert("description".into(), json!(self.description.trim()));
        }
        if let Some(date) = &self.retire_by {
            let mut d = Map::new();
            d.insert("status".into(), json!("planned"));
            if !date.is_empty() {
                d.insert("date".into(), json!(date));
            }
            def.insert("_deprecation".into(), Value::Object(d));
        }
        if !self.attributes.is_empty() {
            let attrs = self.attributes.iter().map(|a| {
                let mut entry = Map::new();
                if !a.description.trim().is_empty() {
                    entry.insert("description".into(), json!(a.description.trim()));
                }
                entry.insert("constraints".into(), a.constraints.to_json());
                (a.name.clone(), Value::Object(entry))
            });
            def.insert("attributes".into(), Value::Object(attrs.collect()));
        }
        Value::Object(def)
    }

    /// The flag's entry under `values`.
    pub fn values(&self) -> Value {
        let mut values = Map::new();
        values.insert("enabled".into(), Value::Bool(self.enabled));
        for a in &self.attributes {
            if let Some(v) = &a.value {
                values.insert(a.name.clone(), v.clone());
            }
        }
        Value::Object(values)
    }

    /// `["new flag New checkout (new_checkout): off", "new_checkout.limit: number · 0..100 = 10"]`
    pub fn describe(&self) -> Vec<String> {
        let mut out = vec![format!("new flag {} ({}): {}", self.name(), self.key, on_off(self.enabled))];
        match self.retire_by.as_deref() {
            Some("") => out.push("short-term flag".into()),
            Some(date) => out.push(format!("short-term flag, retire by {date}")),
            None => {}
        }
        for a in &self.attributes {
            let value = a.value.as_ref().map_or_else(|| "—".to_string(), value_text);
            out.push(format!("{}.{}: {} = {value}", self.key, a.name, a.constraints.summary()));
        }
        out
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct FlagsDoc {
    doc: Value,
}

impl FlagsDoc {
    /// A document without flags, for a profile that has no version yet.
    pub fn empty() -> FlagsDoc {
        FlagsDoc { doc: json!({ "version": "1", "flags": {}, "values": {} }) }
    }

    /// `None` when `text` is not a feature flags document.
    /// Parses a document; its strings are sanitized, since names, descriptions and values
    /// are shown in the terminal (an edit writes them back sanitized).
    pub fn parse(text: &str) -> Option<FlagsDoc> {
        let mut doc: Value = serde_json::from_str(text).ok()?;
        crate::util::sanitize_json(&mut doc);
        doc.get("flags")?.as_object()?;
        Some(FlagsDoc { doc })
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string(&self.doc).unwrap_or_default()
    }

    fn values(&self, key: &str) -> Option<&Map<String, Value>> {
        self.doc.get("values")?.get(key)?.as_object()
    }

    /// Flags in document order; attributes from the definition plus any value without one.
    pub fn flags(&self) -> Vec<Flag> {
        let Some(defs) = self.doc.get("flags").and_then(Value::as_object) else { return Vec::new() };
        defs.iter()
            .map(|(key, def)| {
                let mut attributes: Vec<Attribute> = def
                    .get("attributes")
                    .and_then(Value::as_object)
                    .map(|attrs| {
                        attrs
                            .iter()
                            .map(|(name, a)| Attribute {
                                name: name.clone(),
                                description: a.get("description").and_then(Value::as_str).map(str::to_string),
                                constraints: a.get("constraints").map(Constraints::parse).unwrap_or_default(),
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                for name in self.values(key).into_iter().flat_map(Map::keys) {
                    if name != "enabled" && !name.starts_with('_') && !attributes.iter().any(|a| a.name == *name) {
                        attributes.push(Attribute { name: name.clone(), description: None, constraints: Constraints::default() });
                    }
                }
                Flag {
                    key: key.clone(),
                    name: def.get("name").and_then(Value::as_str).unwrap_or(key).to_string(),
                    description: def.get("description").and_then(Value::as_str).map(str::to_string),
                    attributes,
                    retire_by: def.get("_deprecation").filter(|d| d.is_object()).map(|d| {
                        d.get("date").and_then(Value::as_str).unwrap_or_default().to_string()
                    }),
                }
            })
            .collect()
    }

    pub fn has(&self, key: &str) -> bool {
        self.doc.get("flags").and_then(|f| f.get(key)).is_some()
    }

    /// A flag without a value is off.
    pub fn enabled(&self, key: &str) -> bool {
        self.values(key).and_then(|v| v.get("enabled")).and_then(Value::as_bool).unwrap_or(false)
    }

    pub fn value(&self, key: &str, attr: &str) -> Option<&Value> {
        self.values(key)?.get(attr)
    }

    /// The definition of flag `key` (name, description, attributes), to carry it into a
    /// version that does not have it yet.
    pub fn definition(&self, key: &str) -> Option<Value> {
        self.doc.get("flags")?.get(key).cloned()
    }

    /// A copy with flag `key` set to `enabled` and the given attribute values. `definition`
    /// is added when this version does not define the flag yet.
    pub fn with_values(&self, key: &str, enabled: bool, attrs: &[(String, Value)], definition: Option<&Value>) -> FlagsDoc {
        let mut doc = self.doc.clone();
        if !self.has(key) {
            if !doc.get("flags").is_some_and(Value::is_object) {
                doc["flags"] = json!({});
            }
            doc["flags"][key] = definition.cloned().unwrap_or_else(|| json!({ "name": key }));
        }
        if !doc.get("values").is_some_and(Value::is_object) {
            doc["values"] = json!({});
        }
        let values = &mut doc["values"];
        if !values.get(key).is_some_and(Value::is_object) {
            values[key] = json!({});
        }
        let entry = &mut values[key];
        entry["enabled"] = Value::Bool(enabled);
        for (name, v) in attrs {
            entry[name.as_str()] = v.clone();
        }
        FlagsDoc { doc }
    }

    /// A copy with `flag` added: its definition and its value. `Err` when the flag is not
    /// valid or this version already has its key.
    pub fn with_new_flag(&self, flag: &NewFlag) -> std::result::Result<FlagsDoc, String> {
        flag.validate()?;
        if self.has(&flag.key) {
            return Err(format!("This version already has a flag {}", flag.key));
        }
        let mut doc = self.doc.clone();
        for part in ["flags", "values"] {
            if !doc.get(part).is_some_and(Value::is_object) {
                doc[part] = json!({});
            }
        }
        doc["flags"][flag.key.as_str()] = flag.definition();
        doc["values"][flag.key.as_str()] = flag.values();
        Ok(FlagsDoc { doc })
    }

    /// `["flag Dark mode: off → on", "Dark mode.limit: 10 → 20"]` for flag `key`.
    pub fn diff(&self, newer: &FlagsDoc, key: &str) -> Vec<String> {
        let name = newer.flags().into_iter().find(|f| f.key == key).map_or_else(|| key.to_string(), |f| f.name);
        let mut out = Vec::new();
        let (was, now) = (self.enabled(key), newer.enabled(key));
        if !self.has(key) && newer.has(key) {
            out.push(format!("{name}: added here, {}", on_off(now)));
        } else if was != now {
            out.push(format!("{name}: {} → {}", on_off(was), on_off(now)));
        }
        let mut names: Vec<String> = newer.values(key).into_iter().flat_map(Map::keys).cloned().collect();
        names.extend(self.values(key).into_iter().flat_map(Map::keys).cloned());
        names.dedup();
        let mut seen = Vec::new();
        for attr in names.into_iter().filter(|n| n != "enabled" && !n.starts_with('_')) {
            if seen.contains(&attr) {
                continue;
            }
            let (a, b) = (self.value(key, &attr), newer.value(key, &attr));
            if a != b {
                let show = |v: Option<&Value>| v.map_or_else(|| "—".to_string(), value_text);
                out.push(format!("{name}.{attr}: {} → {}", show(a), show(b)));
            }
            seen.push(attr);
        }
        out
    }

    /// `on · limit=20, theme=dark` (attributes cut to `max` characters).
    pub fn summary(&self, key: &str, max: usize) -> String {
        let mut text = on_off(self.enabled(key)).to_string();
        let attrs: Vec<String> = self
            .values(key)
            .into_iter()
            .flat_map(|m| m.iter())
            .filter(|(k, _)| *k != "enabled" && !k.starts_with('_'))
            .map(|(k, v)| format!("{k}={}", value_text(v)))
            .collect();
        if !attrs.is_empty() {
            text.push_str(" · ");
            text.push_str(&attrs.join(", "));
        }
        crate::util::ellipsize(&text, max)
    }
}

pub fn on_off(enabled: bool) -> &'static str {
    if enabled { "on" } else { "off" }
}

/// A value as typed in a form: strings without quotes, the rest as JSON.
pub fn value_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Reads a typed attribute value: by its constraint type, else like its current value,
/// else whatever it looks like (number, boolean, JSON array, string).
pub fn parse_value(text: &str, kind: Option<&str>, current: Option<&Value>) -> std::result::Result<Value, String> {
    let text = text.trim();
    let infer = |v: &Value| match v {
        Value::Number(_) => "number".to_string(),
        Value::Bool(_) => "boolean".to_string(),
        Value::Array(items) if items.iter().all(Value::is_number) && !items.is_empty() => "number[]".to_string(),
        Value::Array(_) => "string[]".to_string(),
        _ => "string".to_string(),
    };
    let kind = match kind {
        // An array without an element type: like its current value, text otherwise.
        Some("array") => Some(current.filter(|v| v.is_array()).map_or_else(|| "string[]".to_string(), infer)),
        kind => kind.map(str::to_string).or_else(|| current.map(infer)),
    };
    let number = |s: &str| -> std::result::Result<Value, String> {
        let n: f64 = s.trim().parse().map_err(|_| format!("'{s}' is not a number"))?;
        Ok(if n.fract() == 0.0 && n.abs() < 9e15 { json!(n as i64) } else { json!(n) })
    };
    let list = |s: &str| -> Vec<String> {
        if let Ok(Value::Array(items)) = serde_json::from_str::<Value>(s) {
            return items.iter().map(value_text).collect();
        }
        s.split(',').map(|p| p.trim().to_string()).filter(|p| !p.is_empty()).collect()
    };
    match kind.as_deref() {
        Some("number") => number(text),
        Some("boolean") => match text {
            "true" | "on" | "yes" => Ok(Value::Bool(true)),
            "false" | "off" | "no" => Ok(Value::Bool(false)),
            _ => Err(format!("'{text}' is not true or false")),
        },
        Some("string[]") => Ok(Value::Array(list(text).into_iter().map(Value::String).collect())),
        Some("number[]") => list(text).iter().map(|s| number(s)).collect::<std::result::Result<Vec<_>, _>>().map(Value::Array),
        Some(_) => Ok(Value::String(text.to_string())),
        None => Ok(if let Ok(n @ Value::Number(_)) = number(text) {
            n
        } else if text == "true" || text == "false" {
            Value::Bool(text == "true")
        } else {
            Value::String(text.to_string())
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DOC: &str = r#"{
        "version": "1",
        "flags": {
            "dark_mode": {"name": "Dark mode", "description": "New theme",
                          "attributes": {"limit": {"constraints": {"type": "number"}}, "theme": {"constraints": {"type": "string"}}}},
            "beta_checkout": {"name": "Beta checkout", "_createdAt": "2026-01-01T00:00:00Z"},
            "regions": {"name": "Regions"}
        },
        "values": {
            "dark_mode": {"enabled": false, "limit": 10, "theme": "night"},
            "beta_checkout": {"enabled": true, "_updatedAt": "x"},
            "regions": {"enabled": true, "list": ["us", "co"]}
        }
    }"#;

    #[test]
    fn reads_flags_and_values() {
        let doc = FlagsDoc::parse(DOC).unwrap();
        let flags = doc.flags();
        assert_eq!(flags.iter().map(|f| f.key.as_str()).collect::<Vec<_>>(), ["dark_mode", "beta_checkout", "regions"]);
        assert_eq!(flags[0].attributes[0].name, "limit");
        assert_eq!(flags[0].attributes[0].kind(), Some("number"));
        assert_eq!(flags[2].attributes, vec![Attribute { name: "list".into(), description: None, constraints: Constraints::default() }]);
        assert_eq!(flags[1].retire_by, None);
        assert!(!doc.enabled("dark_mode") && doc.enabled("beta_checkout") && !doc.enabled("missing"));
        assert_eq!(doc.summary("dark_mode", 40), "off · limit=10, theme=night");
        assert_eq!(doc.summary("beta_checkout", 40), "on");
        assert!(FlagsDoc::parse(r#"{"timeout": 5}"#).is_none());
    }

    #[test]
    fn edits_keep_unknown_keys_and_diff() {
        let doc = FlagsDoc::parse(DOC).unwrap();
        let newer = doc.with_values("dark_mode", true, &[("limit".into(), json!(20)), ("theme".into(), json!("night"))], None);
        assert_eq!(doc.diff(&newer, "dark_mode"), ["Dark mode: off → on", "Dark mode.limit: 10 → 20"]);
        assert!(newer.to_json().contains("_createdAt") && newer.to_json().contains("_updatedAt"));
        assert!(doc.diff(&doc.clone(), "beta_checkout").is_empty());
        let definition = json!({ "name": "Brand new", "attributes": {} });
        let added = doc.with_values("brand_new", true, &[], Some(&definition));
        assert_eq!(doc.diff(&added, "brand_new"), ["Brand new: added here, on"]);
        let off = doc.with_values("brand_new", false, &[("limit".into(), json!(3))], Some(&definition));
        assert_eq!(doc.diff(&off, "brand_new"), ["Brand new: added here, off", "Brand new.limit: — → 3"]);
        assert_eq!(added.definition("brand_new"), Some(definition));
        assert!(FlagsDoc::parse(&added.to_json()).unwrap().has("brand_new"));
    }

    #[test]
    fn parses_typed_values() {
        assert_eq!(parse_value("20", Some("number"), None), Ok(json!(20)));
        assert_eq!(parse_value("2.5", None, Some(&json!(1))), Ok(json!(2.5)));
        assert!(parse_value("abc", Some("number"), None).is_err());
        assert_eq!(parse_value("off", Some("boolean"), None), Ok(json!(false)));
        assert_eq!(parse_value("us, co", None, Some(&json!(["us"]))), Ok(json!(["us", "co"])));
        assert_eq!(parse_value(r#"[1, 2]"#, Some("number[]"), None), Ok(json!([1, 2])));
        assert_eq!(parse_value("night", None, Some(&json!("day"))), Ok(json!("night")));
        assert_eq!(parse_value("42", None, None), Ok(json!(42)));
    }

    #[test]
    fn reads_constraints_as_appconfig_stores_them() {
        let c = Constraints::parse(&json!({"type": "array", "required": true, "elements": {"type": "string", "enum": ["dhl", "fedex"]}}));
        assert_eq!(c.kind.as_deref(), Some("string[]"));
        assert!(c.required);
        assert_eq!(c.summary(), "string[] · required · one of dhl, fedex");
        assert_eq!(Constraints::parse(&c.to_json()), c);
        assert!(c.check(Some(&json!(["dhl"]))).is_ok());
        assert_eq!(c.check(Some(&json!(["ups"]))), Err("'ups' is not one of dhl, fedex".into()));
        assert_eq!(c.check(None), Err("a value is required".into()));
        let n = Constraints::parse(&json!({"type": "number", "minimum": 0, "maximum": 100}));
        assert_eq!(n.summary(), "number · 0..100");
        assert_eq!(n.to_json(), json!({"type": "number", "minimum": 0, "maximum": 100}));
        assert!(n.check(Some(&json!(100))).is_ok() && n.check(None).is_ok());
        assert_eq!(n.check(Some(&json!(101))), Err("101 is above the maximum, 100".into()));
        assert_eq!(n.check(Some(&json!("x"))), Err("x is not a number".into()));
        let numbers = Constraints::parse(&json!({"type": "array", "elements": {"type": "number", "minimum": 1}}));
        assert_eq!(numbers.kind.as_deref(), Some("number[]"));
        assert!(numbers.check(Some(&json!([0]))).is_err());
        // Arrays without an element type take their values like the current one.
        let any = Constraints::parse(&json!({"type": "array"}));
        assert_eq!(any.kind.as_deref(), Some("array"));
        assert_eq!(parse_value("1, 2", Some("array"), Some(&json!([5]))), Ok(json!([1, 2])));
        assert_eq!(parse_value("a, b", Some("array"), None), Ok(json!(["a", "b"])));
        assert_eq!(parse_value("us, co", Some("string[]"), None), Ok(json!(["us", "co"])));
    }

    #[test]
    fn checks_constraints_before_sending() {
        let bad = |c: Constraints| c.validate().unwrap_err();
        assert!(bad(Constraints::of("date")).contains("unknown type"));
        assert!(bad(Constraints { pattern: Some("^a".into()), ..Constraints::of("number") }).contains("for text"));
        assert!(bad(Constraints { minimum: Some(1), ..Constraints::of("string") }).contains("for numbers"));
        let both = Constraints { pattern: Some("^a".into()), allowed: vec![json!("a")], ..Constraints::of("string") };
        assert!(bad(both).contains("not both"));
        assert!(bad(Constraints { minimum: Some(5), maximum: Some(1), ..Constraints::of("number[]") }).contains("above the maximum"));
        assert!(Constraints { allowed: vec![json!("a")], ..Constraints::of("string[]") }.validate().is_ok());
    }

    fn new_flag() -> NewFlag {
        NewFlag {
            key: "new_search".into(),
            name: "New search".into(),
            description: "Search v2".into(),
            enabled: false,
            retire_by: Some("2026-12-31".into()),
            attributes: vec![
                NewAttribute {
                    name: "rollout".into(),
                    constraints: Constraints { required: true, minimum: Some(0), maximum: Some(100), ..Constraints::of("number") },
                    value: Some(json!(10)),
                    ..Default::default()
                },
                NewAttribute { name: "engine".into(), constraints: Constraints::of("string"), ..Default::default() },
            ],
        }
    }

    #[test]
    fn adds_a_new_flag() {
        let doc = FlagsDoc::parse(DOC).unwrap();
        let flag = new_flag();
        let added = doc.with_new_flag(&flag).unwrap();
        let reread = FlagsDoc::parse(&added.to_json()).unwrap();
        let f = reread.flags().into_iter().find(|f| f.key == "new_search").unwrap();
        assert_eq!(f.name, "New search");
        assert_eq!(f.retire_by.as_deref(), Some("2026-12-31"));
        assert_eq!(f.attributes[0].constraints, flag.attributes[0].constraints);
        assert!(!reread.enabled("new_search"));
        assert_eq!(reread.value("new_search", "rollout"), Some(&json!(10)));
        assert_eq!(reread.value("new_search", "engine"), None);
        // What was there stays, in order, and the new flag goes last.
        assert_eq!(reread.flags().len(), 4);
        assert!(added.to_json().contains("_createdAt"));
        assert_eq!(
            added.definition("new_search").unwrap()["attributes"]["rollout"],
            json!({"constraints": {"type": "number", "minimum": 0, "maximum": 100, "required": true}})
        );
        assert_eq!(flag.describe()[0], "new flag New search (new_search): off");
        assert_eq!(flag.describe()[2], "new_search.rollout: number · required · 0..100 = 10");
        // Taken keys, bad keys, reserved and repeated attributes, values out of range.
        assert!(doc.with_new_flag(&NewFlag { key: "dark_mode".into(), ..flag.clone() }).unwrap_err().contains("already has"));
        for key in ["Dark", "1flag", "", "a b", &"x".repeat(65)] {
            assert!(doc.with_new_flag(&NewFlag { key: key.into(), ..flag.clone() }).is_err(), "{key}");
        }
        let mut reserved = flag.clone();
        reserved.attributes[1].name = "enabled".into();
        assert!(reserved.validate().unwrap_err().contains("reserved"));
        let mut twice = flag.clone();
        twice.attributes[1].name = "rollout".into();
        assert!(twice.validate().unwrap_err().contains("Two attributes"));
        let mut high = flag.clone();
        high.attributes[0].value = Some(json!(150));
        assert_eq!(high.validate(), Err("rollout: 150 is above the maximum, 100".into()));
        let mut missing = flag.clone();
        missing.attributes[0].value = None;
        assert_eq!(missing.validate(), Err("rollout: a value is required".into()));
        assert!(NewFlag { retire_by: Some("31/12/2026".into()), ..flag.clone() }.validate().is_err());
        assert!(NewFlag { name: "two\nlines".into(), ..flag.clone() }.validate().is_err());
        // The key names it when no name is given; an empty document takes it.
        let bare = NewFlag { key: "quick".into(), ..Default::default() };
        assert_eq!(bare.definition(), json!({ "name": "quick" }));
        assert_eq!(FlagsDoc::empty().with_new_flag(&bare).unwrap().to_json(), r#"{"version":"1","flags":{"quick":{"name":"quick"}},"values":{"quick":{"enabled":false}}}"#);
    }

    #[test]
    fn checks_profile_names() {
        assert!(valid_profile_name("checkout-flags").is_ok());
        assert!(valid_profile_name("  ").is_err());
        assert!(valid_profile_name(&"p".repeat(129)).is_err());
        assert!(valid_profile_name("a\u{1b}[31m").is_err());
    }

    fn deployment(number: i64, profile: &str, version: &str, state: &str) -> Deployment {
        Deployment {
            deployment_number: number,
            configuration_name: profile.into(),
            configuration_version: version.into(),
            state: state.into(),
            ..Default::default()
        }
    }

    #[test]
    fn finds_what_an_environment_serves() {
        let deployments = vec![
            deployment(9, "flags", "7", "DEPLOYING"),
            deployment(8, "other", "3", "COMPLETE"),
            deployment(7, "flags", "6", "ROLLED_BACK"),
            deployment(6, "flags", "5", "COMPLETE"),
            deployment(5, "flags", "4", "COMPLETE"),
        ];
        let state = EnvState::of(&deployments, "p1", "flags");
        assert_eq!(state.version(), Some(5));
        assert_eq!(state.active.as_ref().map(|d| d.deployment_number), Some(9));
        let baking = vec![deployment(3, "flags", "2", "BAKING"), deployment(2, "flags", "1", "COMPLETE")];
        let state = EnvState::of(&baking, "p1", "flags");
        assert_eq!(state.version(), Some(2));
        assert!(state.active.is_some());
        assert!(EnvState::of(&[], "p1", "flags").deployed.is_none());
        // With profile ids, a same-named profile of another id is not mixed in.
        let mut other = deployment(10, "flags", "9", "COMPLETE");
        other.configuration_profile_id = Some("p2".into());
        let mut mine = deployment(4, "flags", "3", "COMPLETE");
        mine.configuration_profile_id = Some("p1".into());
        assert_eq!(EnvState::of(&[other, mine], "p1", "flags").version(), Some(3));
    }

    #[test]
    fn describes_strategies() {
        let s = Strategy {
            deployment_duration_in_minutes: Some(20.0),
            growth_type: Some("LINEAR".into()),
            growth_factor: Some(10.0),
            final_bake_time_in_minutes: Some(10.0),
            ..Default::default()
        };
        assert_eq!(s.summary(), "20 min · linear 10% · bake 10 min");
        assert_eq!(Strategy::default().summary(), "all at once");
    }
}
