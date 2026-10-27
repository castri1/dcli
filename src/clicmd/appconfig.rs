//! `dcli flags`: AppConfig feature flags per environment. `dcli flags` lists applications
//! and their profiles; `dcli flags [ENV] APP [PROFILE]` prints flags × AppConfig
//! environments (what each environment serves). `--create KEY --in ENV` adds a flag, with
//! typed attributes, to the version an AppConfig environment serves and deploys it there;
//! `--new-profile NAME` creates a feature flag profile. Values of existing flags are edited
//! in the UI.

use std::collections::HashMap;
use std::io::IsTerminal;
use std::process::ExitCode;

use anyhow::{Result, anyhow, bail};
use serde_json::{Map, Value, json};

use crate::aws::Aws;
use crate::aws::appconfig::{
    self as ac, ALL_AT_ONCE, Application, Constraints, EnvState, FlagsDoc, NewAttribute, NewFlag, Profile, Strategy,
};
use crate::cli::{ask, aws_for, confirm, confirm_write, current_env, paint, print_json};
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
    /// Create a feature flag with this key (a lowercase letter, then letters, digits, _ or -)
    /// in the version --in serves, and deploy it there
    #[arg(long, value_name = "KEY", requires = "in_env", conflicts_with = "new_profile")]
    create: Option<String>,
    /// With --create: the AppConfig environment (name or id) the new flag is deployed to
    #[arg(long = "in", value_name = "APPCONFIG_ENV", requires = "create")]
    in_env: Option<String>,
    /// With --create: the flag's name (default: its key)
    #[arg(long, requires = "create")]
    name: Option<String>,
    /// Description of the new flag (--create) or profile (--new-profile)
    #[arg(long)]
    description: Option<String>,
    /// With --create: the flag starts enabled (default: off)
    #[arg(long, requires = "create")]
    on: bool,
    /// With --create: a short-term flag (AppConfig's deprecation status "planned")
    #[arg(long, requires = "create")]
    short_term: bool,
    /// With --create: a short-term flag to retire by this date (YYYY-MM-DD)
    #[arg(long, value_name = "DATE", requires = "create")]
    retire_by: Option<String>,
    /// With --create, repeatable: an attribute NAME:TYPE[!][=VALUE]; TYPE is string, number,
    /// boolean, string[] or number[], ! makes it required (rollout:number!=10, carriers:string[]=dhl,fedex)
    #[arg(long = "attr", value_name = "SPEC", requires = "create")]
    attrs: Vec<String>,
    /// With --create, repeatable: the values an --attr of text (or a text list) allows: NAME=a,b,c
    #[arg(long, value_name = "NAME=A,B", requires = "create")]
    allowed: Vec<String>,
    /// With --create, repeatable: a regular expression an --attr of text (or a text list) matches: NAME=REGEX
    #[arg(long, value_name = "NAME=REGEX", requires = "create")]
    pattern: Vec<String>,
    /// With --create, repeatable: the whole-number range of an --attr of numbers: NAME=MIN..MAX, either side optional
    #[arg(long, value_name = "NAME=MIN..MAX", requires = "create")]
    range: Vec<String>,
    /// With --create: the deployment strategy (name or id; default: all at once, or in
    /// production the quickest gradual one)
    #[arg(long, value_name = "NAME|ID", requires = "create")]
    strategy: Option<String>,
    /// Create a feature flag profile (hosted in AppConfig) with this name in APP
    #[arg(long, value_name = "NAME")]
    new_profile: Option<String>,
    /// With --create or --new-profile: do not ask for confirmation
    #[arg(short, long)]
    yes: bool,
}

pub fn run(config: &Config, args: Args) -> Result<ExitCode> {
    let writing = args.create.is_some() || args.new_profile.is_some();
    if !writing && (args.description.is_some() || args.yes) {
        bail!("--description and --yes go with --create or --new-profile");
    }
    // Checked before anything is asked of AWS.
    let flag = args.create.as_ref().map(|key| new_flag(key, &args)).transpose()?;
    if let Some(name) = &args.new_profile {
        ac::valid_profile_name(name).map_err(|e| anyhow!(e))?;
        let description = args.description.as_deref().unwrap_or_default();
        if description.chars().count() > 1024 || description.chars().any(char::is_control) {
            bail!("The description takes up to 1024 characters on one line");
        }
    }
    let mut parts: Vec<String> = [args.first, args.second, args.third].into_iter().flatten().collect();
    let env = if parts.first().is_some_and(|p| config.environments.contains_key(p)) {
        parts.remove(0)
    } else {
        current_env(config)?
    };
    if writing && parts.is_empty() {
        bail!("name the AppConfig application: dcli flags [ENV] APP …");
    }
    if args.new_profile.is_some() && parts.len() > 1 {
        bail!("--new-profile takes the profile's name; drop PROFILE ({})", parts[1]);
    }
    let aws = aws_for(config, &env)?;
    let apps = ac::list_applications(&aws)?;
    let Some(app_name) = parts.first() else { return list(&aws, &apps, args.json) };
    let app = find(&apps, app_name, |a| (&a.id, &a.name))
        .ok_or_else(|| anyhow!("no AppConfig application '{app_name}' in {env}. Applications: {}", names(&apps, |a| &a.name)))?;
    let profiles = ac::list_profiles(&aws, &app.id)?;
    if let Some(name) = &args.new_profile {
        let description = args.description.as_deref().unwrap_or_default();
        return new_profile(&aws, &env, app, &profiles, name, description, args.yes, args.json);
    }
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
    if let (Some(flag), Some(in_env)) = (flag, &args.in_env) {
        if !profile.is_feature_flags() {
            bail!("{} is a freeform profile: flags are created in feature flag profiles", profile.name);
        }
        let target = Target { env: &env, app, profile };
        return create(&aws, &target, flag, in_env, args.strategy.as_deref(), args.yes, args.json);
    }
    show(&aws, app, profile, args.json)
}

// ---- creating ------------------------------------------------------------------------

/// `NAME:TYPE[!][=VALUE]`: the attribute's name, its type (`!` when required) and value.
fn parse_attr(spec: &str) -> Result<NewAttribute> {
    let (head, value) = match spec.split_once('=') {
        Some((head, value)) => (head, Some(value)),
        None => (spec, None),
    };
    let Some((name, kind)) = head.split_once(':') else {
        bail!("--attr {spec}: write NAME:TYPE[!][=VALUE], e.g. rollout:number!=10");
    };
    let (kind, required) = match kind.trim().strip_suffix('!') {
        Some(kind) => (kind.trim(), true),
        None => (kind.trim(), false),
    };
    if !ac::ATTRIBUTE_TYPES.contains(&kind) {
        bail!("--attr {spec}: the type is one of {}", ac::ATTRIBUTE_TYPES.join(", "));
    }
    let name = name.trim().to_string();
    let value = value.map(|v| ac::parse_value(v, Some(kind), None).map_err(|e| anyhow!("--attr {name}: {e}"))).transpose()?;
    let constraints = Constraints { required, ..Constraints::of(kind) };
    Ok(NewAttribute { name, constraints, value, ..Default::default() })
}

/// `NAME=REST` of --allowed, --pattern and --range: the rest, and which attribute it names.
fn attr_option<'a>(option: &str, text: &'a str, attrs: &[NewAttribute]) -> Result<(&'a str, usize)> {
    let Some((name, rest)) = text.split_once('=') else { bail!("{option} {text}: write NAME=…") };
    let name = name.trim();
    match attrs.iter().position(|a| a.name == name) {
        Some(i) => Ok((rest, i)),
        None => bail!("{option} {text}: no --attr named {name}"),
    }
}

/// `MIN..MAX`, either side optional.
fn parse_range(text: &str) -> Result<(Option<i64>, Option<i64>)> {
    let Some((min, max)) = text.split_once("..") else { bail!("write the range as MIN..MAX (0..100, 1.., ..50)") };
    let side = |s: &str| -> Result<Option<i64>> {
        match s.trim() {
            "" => Ok(None),
            n => n.parse().map(Some).map_err(|_| anyhow!("'{n}' is not a whole number (AppConfig takes whole-number limits)")),
        }
    };
    let range = (side(min)?, side(max)?);
    if range == (None, None) {
        bail!("give a minimum, a maximum or both: MIN..MAX");
    }
    Ok(range)
}

/// The flag `--create` describes, checked as AppConfig would.
fn new_flag(key: &str, args: &Args) -> Result<NewFlag> {
    let mut attributes = args.attrs.iter().map(|s| parse_attr(s)).collect::<Result<Vec<_>>>()?;
    for text in &args.allowed {
        let (rest, i) = attr_option("--allowed", text, &attributes)?;
        let values: Vec<Value> = rest.split(',').map(str::trim).filter(|v| !v.is_empty()).map(|v| json!(v)).collect();
        if values.is_empty() {
            bail!("--allowed {text}: list the values, separated by commas");
        }
        attributes[i].constraints.allowed = values;
    }
    for text in &args.pattern {
        let (rest, i) = attr_option("--pattern", text, &attributes)?;
        attributes[i].constraints.pattern = Some(rest.to_string());
    }
    for text in &args.range {
        let (rest, i) = attr_option("--range", text, &attributes)?;
        let (min, max) = parse_range(rest).map_err(|e| anyhow!("--range {text}: {e}"))?;
        attributes[i].constraints.minimum = min;
        attributes[i].constraints.maximum = max;
    }
    let flag = NewFlag {
        key: key.trim().to_string(),
        name: args.name.clone().unwrap_or_default(),
        description: args.description.clone().unwrap_or_default(),
        enabled: args.on,
        retire_by: match (&args.retire_by, args.short_term) {
            (Some(date), _) => Some(date.trim().to_string()),
            (None, true) => Some(String::new()),
            (None, false) => None,
        },
        attributes,
    };
    flag.validate().map_err(|e| anyhow!(e))?;
    Ok(flag)
}

/// The strategy to deploy with: the one asked for, else all at once, or in production the
/// quickest gradual one (`strategies` sorted by duration, then name), as in the UI.
fn pick_strategy(strategies: &[Strategy], wanted: Option<&str>, prod: bool) -> Result<Strategy> {
    if let Some(w) = wanted {
        return strategies
            .iter()
            .find(|s| s.id == w || s.name.eq_ignore_ascii_case(w))
            .cloned()
            .ok_or_else(|| anyhow!("no deployment strategy '{w}'. Strategies: {}", names(strategies, |s| &s.name)));
    }
    let chosen = if prod {
        strategies.iter().find(|s| s.deployment_duration_in_minutes.unwrap_or(0.0) > 0.0)
    } else {
        strategies.iter().find(|s| s.id == ALL_AT_ONCE || s.name.contains("AllAtOnce"))
    };
    Ok(chosen.or(strategies.first()).cloned().unwrap_or_else(|| all_at_once(ALL_AT_ONCE)))
}

fn all_at_once(id: &str) -> Strategy {
    Strategy { id: id.into(), name: id.into(), ..Default::default() }
}

/// A word for a command line, quoted when it needs to be.
fn word(s: &str) -> String {
    if !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || "_-./:@".contains(c)) {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

struct Target<'a> {
    /// The dcli environment.
    env: &'a str,
    app: &'a Application,
    profile: &'a Profile,
}

/// Adds `flag` to the version AppConfig environment `in_env` serves (else the latest, else
/// an empty document) and deploys it there, after asking.
fn create(aws: &Aws, t: &Target, flag: NewFlag, in_env: &str, strategy: Option<&str>, yes: bool, json: bool) -> Result<ExitCode> {
    let (app, profile) = (t.app, t.profile);
    let envs = ac::list_environments(aws, &app.id)?;
    let target = envs
        .iter()
        .find(|e| e.id == in_env || e.name.eq_ignore_ascii_case(in_env))
        .ok_or_else(|| anyhow!("no AppConfig environment '{in_env}' in {}. Environments: {}", app.name, names(&envs, |e| &e.name)))?;
    let mut states = Vec::new();
    for e in &envs {
        states.push(EnvState::of(&ac::list_deployments(aws, &app.id, &e.id, 2)?, &profile.id, &profile.name));
    }
    let state = envs.iter().position(|e| e.id == target.id).map(|i| states[i].clone()).unwrap_or_default();
    if let Some(active) = &state.active {
        bail!("{} has a deployment of {} running ({}); try again once it ends", target.name, profile.name, active.progress());
    }
    let latest = ac::latest_version(aws, &app.id, &profile.id)?;
    let base = state.version().or(latest);

    // Every version the key could already be in: what each environment serves, and the latest.
    let mut docs: HashMap<i64, Option<FlagsDoc>> = HashMap::new();
    for v in states.iter().filter_map(EnvState::version).chain(latest) {
        if let std::collections::hash_map::Entry::Vacant(slot) = docs.entry(v) {
            slot.insert(FlagsDoc::parse(&ac::get_hosted_version(aws, &app.id, &profile.id, v)?));
        }
    }
    let has = |v: i64| docs.get(&v).and_then(Option::as_ref).is_some_and(|d| d.has(&flag.key));
    let mut found: Vec<String> = envs
        .iter()
        .zip(&states)
        .filter_map(|(e, s)| s.version().filter(|v| has(*v)).map(|v| format!("{} (v{v})", e.name)))
        .collect();
    if let Some(v) = latest.filter(|v| has(*v) && !states.iter().any(|s| s.version() == Some(*v))) {
        found.push(format!("the latest version (v{v})"));
    }
    if !found.is_empty() {
        bail!(
            "{} already has a flag {}: in {}. Its value per environment is set in the UI (dcli, AppConfig, Enter on its cell)",
            profile.name,
            flag.key,
            found.join(", ")
        );
    }
    let doc = match base {
        Some(v) => docs.get(&v).cloned().flatten().ok_or_else(|| anyhow!("v{v} of {} is not a feature flags document", profile.name))?,
        None => FlagsDoc::empty(),
    };
    let newer = doc.with_new_flag(&flag).map_err(|e| anyhow!(e))?;

    let prod = is_production(t.env) || is_production(&target.name);
    let strategy = match (ac::list_strategies(aws), strategy) {
        (Ok(mut list), wanted) => {
            list.sort_by_key(|s| (s.deployment_duration_in_minutes.unwrap_or(0.0) as i64, s.name.clone()));
            pick_strategy(&list, wanted, prod)?
        }
        // Without permission to list them, a strategy named by id still works.
        (Err(_), Some(id)) => all_at_once(id),
        (Err(e), None) if prod => bail!("could not list deployment strategies ({}); pass --strategy", e.short()),
        (Err(_), None) => all_at_once(ALL_AT_ONCE),
    };

    let changes = flag.describe();
    let from = match (state.version(), base) {
        (Some(v), _) => format!("A new version is created from v{v} (what {} serves now).", target.name),
        (None, Some(v)) => format!("{} does not serve {} yet: a new version is created from the latest, v{v}.", target.name, profile.name),
        (None, None) => format!("The first version of {}.", profile.name),
    };
    let mut preview = vec![format!(
        "{}  {} {}",
        paint(&profile.name, "1"),
        paint(&format!("{} · AppConfig →", app.name), "90"),
        paint(&target.name, if is_production(&target.name) { "31;1" } else { "32;1" })
    )];
    preview.extend(changes.iter().map(|c| format!("  {}", paint(c, "33"))));
    preview.push(paint(&from, "90"));
    preview.push(format!("{}  {}  {}", paint("Strategy", "90"), paint(&strategy.name, "1"), paint(&strategy.summary(), "90")));
    for line in preview {
        // With --json, stdout carries only the JSON.
        if json {
            eprintln!("{line}");
        } else {
            println!("{line}");
        }
    }
    if !yes {
        if !std::io::stdin().is_terminal() {
            bail!("refusing to deploy without confirmation; pass --yes");
        }
        let agreed = if prod {
            ask(&format!("Deploy it to {}? Type '{}' to confirm:", target.name, target.name)) == target.name
        } else {
            confirm(&format!("Deploy it to {}?", target.name))
        };
        if !agreed {
            println!("Cancelled.");
            return Ok(ExitCode::FAILURE);
        }
    }

    // Built on what the environment served and the versions that existed: if either moved
    // while asking, someone else changed the profile.
    let now = EnvState::of(&ac::list_deployments(aws, &app.id, &target.id, 2)?, &profile.id, &profile.name);
    if let Some(active) = &now.active {
        bail!("{} started a deployment of {} meanwhile ({}); run it again once it ends", target.name, profile.name, active.progress());
    }
    if now.version() != state.version() || ac::latest_version(aws, &app.id, &profile.id)? != latest {
        bail!("{} changed meanwhile (another version or deployment); run it again", profile.name);
    }
    let description = ellipsize(&format!("dcli: {}", changes.join("; ")), 900);
    let version = ac::create_hosted_version(aws, &app.id, &profile.id, &newer.to_json(), &description, latest)?;
    let deployment = ac::start_deployment(aws, &app.id, &target.id, &profile.id, version, &strategy.id, &description)
        .map_err(|e| anyhow!("created v{version} but could not deploy it to {}: {e}", target.name))?;
    if json {
        print_json(&json!({
            "application": app.name,
            "profile": profile.name,
            "environment": target.name,
            "flag": flag.key,
            "version": version,
            "deployment": deployment.deployment_number,
            "strategy": strategy.name,
        }));
    } else {
        let number = deployment.deployment_number;
        println!("{} Deploying v{version} to {} · deployment #{number} ({})", paint("✓", "32"), target.name, strategy.name);
        let follow = format!("Follow it: dcli flags {} {} {}", word(t.env), word(&app.name), word(&profile.name));
        println!("{}", paint(&follow, "90"));
    }
    Ok(ExitCode::SUCCESS)
}

/// Creates a feature flag profile, after asking.
#[allow(clippy::too_many_arguments)]
fn new_profile(
    aws: &Aws,
    env: &str,
    app: &Application,
    profiles: &[Profile],
    name: &str,
    description: &str,
    yes: bool,
    json: bool,
) -> Result<ExitCode> {
    let name = name.trim();
    if let Some(p) = profiles.iter().find(|p| p.name.eq_ignore_ascii_case(name)) {
        bail!("{} already has a profile named {}", app.name, p.name);
    }
    if !confirm_write(env, &format!("Create the feature flag profile {name} in {} ({env})?", app.name), yes)? {
        println!("Cancelled.");
        return Ok(ExitCode::FAILURE);
    }
    let profile = ac::create_flags_profile(aws, &app.id, name, description)?;
    if json {
        print_json(&json!({ "application": app.name, "profile": profile.name, "id": profile.id, "type": profile.kind }));
    } else {
        println!("{} Created the feature flag profile {} ({}) in {}", paint("✓", "32"), profile.name, profile.id, app.name);
        let next = format!("Add its first flag: dcli flags {} {} {} --create KEY --in ENVIRONMENT", word(env), word(&app.name), word(&profile.name));
        println!("{}", paint(&next, "90"));
    }
    Ok(ExitCode::SUCCESS)
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

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser)]
    struct Cli {
        #[command(flatten)]
        args: Args,
    }

    fn args(argv: &[&str]) -> std::result::Result<Args, clap::Error> {
        Cli::try_parse_from(std::iter::once("flags").chain(argv.iter().copied())).map(|c| c.args)
    }

    #[test]
    fn reads_attribute_specs() {
        let a = parse_attr("rollout:number!=10").unwrap();
        assert_eq!((a.name.as_str(), a.constraints.kind.as_deref(), a.constraints.required), ("rollout", Some("number"), true));
        assert_eq!(a.value, Some(json!(10)));
        let a = parse_attr("carriers:string[]=dhl, fedex").unwrap();
        assert_eq!(a.value, Some(json!(["dhl", "fedex"])));
        assert!(!a.constraints.required);
        assert_eq!(parse_attr("query:string=a=b").unwrap().value, Some(json!("a=b")));
        assert_eq!(parse_attr("beta:boolean").unwrap().value, None);
        assert_eq!(parse_attr("ids:number[]=1,2").unwrap().value, Some(json!([1, 2])));
        assert!(parse_attr("rollout").unwrap_err().to_string().contains("NAME:TYPE"));
        assert!(parse_attr("when:date=2026-01-01").unwrap_err().to_string().contains("the type is one of"));
        assert!(parse_attr("n:number=ten").unwrap_err().to_string().contains("not a number"));
    }

    #[test]
    fn reads_ranges() {
        assert_eq!(parse_range("0..100").unwrap(), (Some(0), Some(100)));
        assert_eq!(parse_range("1..").unwrap(), (Some(1), None));
        assert_eq!(parse_range(" ..-5").unwrap(), (None, Some(-5)));
        assert!(parse_range("..").is_err());
        assert!(parse_range("5").is_err());
        assert!(parse_range("0.5..1").unwrap_err().to_string().contains("whole number"));
    }

    #[test]
    fn builds_the_flag_from_options() {
        let a = args(&[
            "dev", "checkout", "--create", "new_search", "--in", "Development", "--name", "New search", "--on",
            "--retire-by", "2026-12-31", "--attr", "rollout:number!=10", "--range", "rollout=0..100",
            "--attr", "engine:string=fast", "--allowed", "engine=fast, exact", "--attr", "code:string",
            "--pattern", "code=^[A-Z]{3}$",
        ])
        .unwrap();
        let flag = new_flag("new_search", &a).unwrap();
        assert_eq!(flag.name(), "New search");
        assert!(flag.enabled);
        assert_eq!(flag.retire_by.as_deref(), Some("2026-12-31"));
        assert_eq!((flag.attributes[0].constraints.minimum, flag.attributes[0].constraints.maximum), (Some(0), Some(100)));
        assert_eq!(flag.attributes[1].constraints.allowed, vec![json!("fast"), json!("exact")]);
        assert_eq!(flag.attributes[2].constraints.pattern.as_deref(), Some("^[A-Z]{3}$"));
        let short = args(&["checkout", "--create", "quick", "--in", "dev", "--short-term"]).unwrap();
        assert_eq!(new_flag("quick", &short).unwrap().retire_by.as_deref(), Some(""));

        let base = ["checkout", "--create", "x", "--in", "dev"];
        let fails = |extra: &[&str]| {
            let argv: Vec<&str> = base.iter().chain(extra).copied().collect();
            new_flag("x", &args(&argv).unwrap()).unwrap_err().to_string()
        };
        assert!(fails(&["--attr", "n:number!"]).contains("a value is required"));
        assert!(fails(&["--attr", "n:number=150", "--range", "n=0..100"]).contains("above the maximum"));
        assert!(fails(&["--allowed", "n=a,b"]).contains("no --attr named n"));
        assert!(fails(&["--attr", "n:number=1", "--allowed", "n=a"]).contains("for text"));
        assert!(fails(&["--attr", "s:string=c", "--allowed", "s=a,b"]).contains("not one of"));
        assert!(fails(&["--retire-by", "next week"]).contains("YYYY-MM-DD"));
        assert!(new_flag("Bad Key", &args(&base).unwrap()).unwrap_err().to_string().contains("not a valid key"));
    }

    #[test]
    fn options_go_with_their_action() {
        assert!(args(&["checkout", "--create", "x"]).is_err(), "--create needs --in");
        assert!(args(&["checkout", "--in", "dev"]).is_err(), "--in needs --create");
        assert!(args(&["checkout", "--attr", "n:number"]).is_err());
        assert!(args(&["checkout", "--create", "x", "--in", "dev", "--new-profile", "p"]).is_err());
        assert!(args(&["checkout", "--new-profile", "p", "--description", "d", "-y"]).is_ok());
    }

    fn strategy(id: &str, minutes: f64) -> Strategy {
        Strategy { id: id.into(), name: id.into(), deployment_duration_in_minutes: Some(minutes), ..Default::default() }
    }

    #[test]
    fn picks_the_strategy_like_the_ui() {
        let list = [strategy("AppConfig.AllAtOnce", 0.0), strategy("Linear50", 1.0), strategy("Canary10", 20.0)];
        assert_eq!(pick_strategy(&list, None, false).unwrap().id, "AppConfig.AllAtOnce");
        assert_eq!(pick_strategy(&list, None, true).unwrap().id, "Linear50");
        assert_eq!(pick_strategy(&list, Some("canary10"), true).unwrap().id, "Canary10");
        assert!(pick_strategy(&list, Some("Fast"), false).unwrap_err().to_string().contains("Strategies: AppConfig.AllAtOnce"));
        assert_eq!(pick_strategy(&[], None, true).unwrap().id, ALL_AT_ONCE);
        assert_eq!(pick_strategy(&[strategy("Mine", 0.0)], None, true).unwrap().id, "Mine");
    }

    #[test]
    fn quotes_command_words() {
        assert_eq!(word("checkout-flags"), "checkout-flags");
        assert_eq!(word("my flags"), "'my flags'");
        assert_eq!(word("it's"), r"'it'\''s'");
    }
}
