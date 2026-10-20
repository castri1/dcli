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

#[derive(Debug, Clone, PartialEq)]
pub struct Attribute {
    pub name: String,
    /// Constraint type: `string`, `number`, `boolean`, `string[]`, `number[]`.
    pub kind: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Flag {
    pub key: String,
    pub name: String,
    pub description: Option<String>,
    pub attributes: Vec<Attribute>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FlagsDoc {
    doc: Value,
}

impl FlagsDoc {
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
                                kind: a.pointer("/constraints/type").and_then(Value::as_str).map(str::to_string),
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                for name in self.values(key).into_iter().flat_map(Map::keys) {
                    if name != "enabled" && !name.starts_with('_') && !attributes.iter().any(|a| a.name == *name) {
                        attributes.push(Attribute { name: name.clone(), kind: None });
                    }
                }
                Flag {
                    key: key.clone(),
                    name: def.get("name").and_then(Value::as_str).unwrap_or(key).to_string(),
                    description: def.get("description").and_then(Value::as_str).map(str::to_string),
                    attributes,
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

    /// `["flag Dark mode: off → on", "Dark mode.limit: 10 → 20"]` for flag `key`.
    pub fn diff(&self, newer: &FlagsDoc, key: &str) -> Vec<String> {
        let name = newer.flags().into_iter().find(|f| f.key == key).map_or_else(|| key.to_string(), |f| f.name);
        let mut out = Vec::new();
        let (was, now) = (self.enabled(key), newer.enabled(key));
        if was != now {
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
    let kind = kind.map(str::to_string).or_else(|| {
        current.map(|v| match v {
            Value::Number(_) => "number".to_string(),
            Value::Bool(_) => "boolean".to_string(),
            Value::Array(items) if items.iter().all(Value::is_number) && !items.is_empty() => "number[]".to_string(),
            Value::Array(_) => "string[]".to_string(),
            _ => "string".to_string(),
        })
    });
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
        assert_eq!(flags[0].attributes[0], Attribute { name: "limit".into(), kind: Some("number".into()) });
        assert_eq!(flags[2].attributes, vec![Attribute { name: "list".into(), kind: None }]);
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
        assert_eq!(doc.diff(&added, "brand_new"), ["Brand new: off → on"]);
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
