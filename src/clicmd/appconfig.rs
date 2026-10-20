//! `dcli flags`: AppConfig feature flags per environment. `dcli flags` lists applications
//! and their profiles; `dcli flags [ENV] APP [PROFILE]` prints flags × AppConfig
//! environments (what each environment serves). Edits happen in the UI.

use std::collections::HashMap;
use std::process::ExitCode;

use anyhow::{Result, anyhow, bail};
use serde_json::{Map, Value, json};

use crate::aws::Aws;
use crate::aws::appconfig::{self as ac, Application, EnvState, FlagsDoc, Profile};
use crate::cli::{aws_for, current_env, paint, print_json};
use crate::config::{Config, is_production};
use crate::util::ellipsize;

#[derive(clap::Args)]
pub struct Args {
    /// Environment (default: the current one); then the AppConfig application
    #[arg(value_name = "ENV")]
    first: Option<String>,
    /// Application (name or id)
    #[arg(value_name = "APP")]
    second: Option<String>,
    /// Configuration profile (default: the application's only feature flag profile)
    #[arg(value_name = "PROFILE")]
    third: Option<String>,
    #[arg(long)]
    json: bool,
}

pub fn run(config: &Config, args: Args) -> Result<ExitCode> {
    let mut parts: Vec<String> = [args.first, args.second, args.third].into_iter().flatten().collect();
    let env = if parts.first().is_some_and(|p| config.environments.contains_key(p)) {
        parts.remove(0)
    } else {
        current_env(config)?
    };
    let aws = aws_for(config, &env)?;
    let apps = ac::list_applications(&aws)?;
    let Some(app_name) = parts.first() else { return list(&aws, &apps, args.json) };
    let app = find(&apps, app_name, |a| (&a.id, &a.name))
        .ok_or_else(|| anyhow!("no AppConfig application '{app_name}' in {env}. Applications: {}", names(&apps, |a| &a.name)))?;
    let profiles = ac::list_profiles(&aws, &app.id)?;
    let profile = match parts.get(1) {
        Some(p) => find(&profiles, p, |x| (&x.id, &x.name))
            .ok_or_else(|| anyhow!("no profile '{p}' in {}. Profiles: {}", app.name, names(&profiles, |x| &x.name)))?,
        None => {
            let flags: Vec<&Profile> = profiles.iter().filter(|p| p.is_feature_flags()).collect();
            match flags.as_slice() {
                [only] => *only,
                [] => bail!("{} has no feature flag profile. Profiles: {}", app.name, names(&profiles, |x| &x.name)),
                many => bail!(
                    "{} has several feature flag profiles; name one: {}",
                    app.name,
                    many.iter().map(|p| p.name.as_str()).collect::<Vec<_>>().join(", ")
                ),
            }
        }
    };
    if !profile.is_hosted() {
        bail!("{} is stored outside AppConfig (S3, Parameter Store…); dcli reads hosted profiles only", profile.name);
    }
    show(&aws, app, profile, args.json)
}

fn find<'a, T>(items: &'a [T], wanted: &str, keys: impl Fn(&T) -> (&String, &String)) -> Option<&'a T> {
    items.iter().find(|i| {
        let (id, name) = keys(i);
        id == wanted || name.eq_ignore_ascii_case(wanted)
    })
}

fn names<T>(items: &[T], name: impl Fn(&T) -> &String) -> String {
    items.iter().map(|i| name(i).as_str()).collect::<Vec<_>>().join(", ")
}

/// Applications and their profiles.
fn list(aws: &Aws, apps: &[Application], json: bool) -> Result<ExitCode> {
    let mut out = Vec::new();
    for app in apps {
        out.push((app, ac::list_profiles(aws, &app.id)?));
    }
    if json {
        print_json(&Value::Array(
            out.iter()
                .map(|(app, profiles)| {
                    json!({
                        "application": app.name,
                        "id": app.id,
                        "profiles": profiles.iter().map(|p| json!({ "name": p.name, "id": p.id, "type": p.kind })).collect::<Vec<_>>(),
                    })
                })
                .collect(),
        ));
        return Ok(ExitCode::SUCCESS);
    }
    if out.is_empty() {
        println!("No AppConfig applications.");
    }
    for (app, profiles) in out {
        let desc = app.description.as_deref().map(|d| paint(&format!("  {d}"), "90")).unwrap_or_default();
        println!("{}{desc}", paint(&app.name, "1"));
        for p in profiles {
            let kind = if p.is_feature_flags() { "feature flags" } else { "freeform" };
            println!("  {}  {}", p.name, paint(kind, "90"));
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// Flags × environments, from the version each environment serves.
fn show(aws: &Aws, app: &Application, profile: &Profile, json: bool) -> Result<ExitCode> {
    let envs = ac::list_environments(aws, &app.id)?;
    let mut states = Vec::new();
    let mut docs: HashMap<i64, String> = HashMap::new();
    for e in &envs {
        let state = EnvState::of(&ac::list_deployments(aws, &app.id, &e.id, 2)?, &profile.id, &profile.name);
        if let Some(v) = state.version()
            && !docs.contains_key(&v)
        {
            docs.insert(v, ac::get_hosted_version(aws, &app.id, &profile.id, v)?);
        }
        states.push(state);
    }

    if !profile.is_feature_flags() {
        return show_freeform(&envs, &states, &docs, profile, json);
    }
    let parsed: HashMap<i64, FlagsDoc> = docs.iter().filter_map(|(v, text)| Some((*v, FlagsDoc::parse(text)?))).collect();
    let doc_of = |i: usize| states[i].version().and_then(|v| parsed.get(&v));
    let mut flags = Vec::new();
    for i in 0..envs.len() {
        for flag in doc_of(i).map(FlagsDoc::flags).unwrap_or_default() {
            if !flags.iter().any(|f: &ac::Flag| f.key == flag.key) {
                flags.push(flag);
            }
        }
    }

    if json {
        let environments: Vec<Value> = envs
            .iter()
            .zip(&states)
            .map(|(e, s)| {
                json!({
                    "name": e.name,
                    "id": e.id,
                    "version": s.version(),
                    "deployment": s.active.as_ref().map(|d| json!({
                        "number": d.deployment_number,
                        "state": d.state,
                        "percent": d.percentage_complete,
                        "version": d.version(),
                    })),
                })
            })
            .collect();
        let rows: Vec<Value> = flags
            .iter()
            .map(|f| {
                let mut values = Map::new();
                for (i, e) in envs.iter().enumerate() {
                    let v = doc_of(i).filter(|d| d.has(&f.key)).map(|d| {
                        let mut entry = Map::new();
                        entry.insert("enabled".into(), Value::Bool(d.enabled(&f.key)));
                        for a in &f.attributes {
                            if let Some(v) = d.value(&f.key, &a.name) {
                                entry.insert(a.name.clone(), v.clone());
                            }
                        }
                        Value::Object(entry)
                    });
                    values.insert(e.name.clone(), v.unwrap_or(Value::Null));
                }
                json!({ "key": f.key, "name": f.name, "description": f.description, "values": values })
            })
            .collect();
        print_json(&json!({ "application": app.name, "profile": profile.name, "environments": environments, "flags": rows }));
        return Ok(ExitCode::SUCCESS);
    }

    let flag_w = flags.iter().map(|f| f.name.chars().count()).max().unwrap_or(4).clamp(4, 30) + 2;
    let col_w = 26;
    let mut header = format!("{:<flag_w$}", "");
    let mut status = format!("{:<flag_w$}", "");
    for (e, s) in envs.iter().zip(&states) {
        header.push_str(&paint(&format!("{:<col_w$}", ellipsize(&e.name, col_w - 1)), if is_production(&e.name) { "31;1" } else { "32;1" }));
        let text = match (&s.active, s.version()) {
            (Some(a), _) => format!("{} → v{}", a.progress(), a.configuration_version),
            (None, Some(v)) => format!("v{v}"),
            (None, None) => "not deployed".into(),
        };
        status.push_str(&paint(&format!("{:<col_w$}", ellipsize(&text, col_w - 1)), if s.active.is_some() { "94" } else { "90" }));
    }
    println!("{}  {}", paint(&profile.name, "1"), paint(&format!("{} · AppConfig", app.name), "90"));
    println!("{header}");
    println!("{status}");
    if flags.is_empty() {
        println!("{}", paint("No flags deployed.", "90"));
    }
    for f in &flags {
        let mut line = paint(&format!("{:<flag_w$}", ellipsize(&f.name, flag_w - 2)), "1");
        for i in 0..envs.len() {
            let cell = match doc_of(i) {
                Some(d) if d.has(&f.key) => {
                    let on = d.enabled(&f.key);
                    let text = format!("{} {}", if on { "●" } else { "○" }, d.summary(&f.key, col_w - 3));
                    paint(&format!("{:<col_w$}", ellipsize(&text, col_w - 1)), if on { "32" } else { "90" })
                }
                _ => paint(&format!("{:<col_w$}", "—"), "90"),
            };
            line.push_str(&cell);
        }
        println!("{line}");
    }
    Ok(ExitCode::SUCCESS)
}

fn show_freeform(
    envs: &[ac::Environment],
    states: &[EnvState],
    docs: &HashMap<i64, String>,
    profile: &Profile,
    json: bool,
) -> Result<ExitCode> {
    if json {
        let out: Vec<Value> = envs
            .iter()
            .zip(states)
            .map(|(e, s)| {
                let content = s.version().and_then(|v| docs.get(&v)).map(|t| serde_json::from_str::<Value>(t).unwrap_or_else(|_| Value::String(t.clone())));
                json!({ "environment": e.name, "version": s.version(), "content": content })
            })
            .collect();
        print_json(&json!({ "profile": profile.name, "environments": out }));
        return Ok(ExitCode::SUCCESS);
    }
    for (e, s) in envs.iter().zip(states) {
        let version = s.version().map(|v| format!("v{v}")).unwrap_or_else(|| "not deployed".into());
        println!("{}  {}", paint(&e.name, "1"), paint(&version, "90"));
        if let Some(text) = s.version().and_then(|v| docs.get(&v)) {
            let pretty = crate::util::sanitize(&crate::util::pretty_json(text).unwrap_or_else(|| text.clone()));
            for line in crate::util::sanitize(&pretty).lines() {
                println!("  {line}");
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}
