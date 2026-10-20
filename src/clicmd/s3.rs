//! `dcli s3`: list buckets and objects, presigned links, downloads.

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Result, anyhow, bail};
use serde_json::json;

use crate::aws::{logs, s3};
use crate::cli::{aws_for, current_env, env_and, paint, print_json};
use crate::config::Config;
use crate::util::{display_path, fmt_bytes, fmt_duration, fmt_utc, now_secs, program, show_invisible};

#[derive(clap::Args)]
pub struct Args {
    #[command(subcommand)]
    action: Action,
}

#[derive(clap::Subcommand)]
enum Action {
    /// List buckets, or the folders and objects of s3://bucket/prefix: ls [ENV] [URI]
    Ls {
        env: Option<String>,
        uri: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Print a presigned download link: url [ENV] s3://bucket/key
    Url {
        env: Option<String>,
        uri: Option<String>,
        /// How long the link works: 15m, 1h, 12h, 7d (at most 7 days, and never longer
        /// than your AWS session)
        #[arg(long, default_value = "1h")]
        expires: String,
    },
    /// Download an object: get [ENV] s3://bucket/key [DEST] (default: the current folder)
    Get {
        #[arg(num_args = 1..=3, value_name = "[ENV] URI [DEST]")]
        args: Vec<String>,
    },
}

pub fn run(config: &Config, args: Args) -> Result<ExitCode> {
    match args.action {
        Action::Ls { env, uri, json } => {
            let (env, uri) = env_and(config, env, uri)?;
            ls(config, &env, uri.as_deref(), json)
        }
        Action::Url { env, uri, expires } => {
            let (env, Some(uri)) = env_and(config, env, uri)? else { bail!("which object? {} s3 url [ENV] s3://bucket/key", program()) };
            url(config, &env, &uri, &expires)
        }
        Action::Get { args } => {
            let (env, rest) = match args.as_slice() {
                [first, rest @ ..] if !rest.is_empty() && config.environments.contains_key(first) => (first.clone(), rest.to_vec()),
                _ => (current_env(config)?, args),
            };
            let [uri, dest @ ..] = rest.as_slice() else { bail!("which object? {} s3 get [ENV] s3://bucket/key [DEST]", program()) };
            if dest.len() > 1 {
                bail!("too many arguments: {} s3 get [ENV] s3://bucket/key [DEST]", program());
            }
            get(config, &env, uri, dest.first().map(PathBuf::from))
        }
    }
}

fn object(uri: &str) -> Result<(String, String)> {
    let (bucket, key) = s3::parse_uri(uri).ok_or_else(|| anyhow!("not an S3 URI: {uri} (use s3://bucket/key)"))?;
    if key.is_empty() || key.ends_with('/') {
        bail!("{uri} is a bucket or folder; name an object: s3://{bucket}/path/file");
    }
    Ok((bucket, key))
}

fn ls(config: &Config, env: &str, uri: Option<&str>, json: bool) -> Result<ExitCode> {
    let aws = aws_for(config, env)?;
    let Some(uri) = uri else {
        let buckets = s3::list_buckets(&aws)?;
        if json {
            let list: Vec<_> =
                buckets.iter().map(|b| json!({ "name": b.name, "region": b.region, "created": b.created })).collect();
            print_json(&json!(list));
            return Ok(ExitCode::SUCCESS);
        }
        let width = buckets.iter().map(|b| b.name.chars().count()).max().unwrap_or(4);
        for b in &buckets {
            let created = b.created.map(fmt_utc).unwrap_or_default();
            println!("{:<width$}  {:<14}  {}", b.name, b.region.as_deref().unwrap_or("—"), paint(&created, "90"));
        }
        return Ok(ExitCode::SUCCESS);
    };
    let (bucket, given) = s3::parse_uri(uri).ok_or_else(|| anyhow!("not an S3 URI: {uri}"))?;
    let mut prefix = given.clone();
    if !prefix.is_empty() && !prefix.ends_with('/') {
        prefix.push('/');
    }
    let regional = s3::regional(&aws, &bucket)?;
    let (mut folders, mut objects) = (Vec::new(), Vec::new());
    let mut token: Option<String> = None;
    loop {
        let page = s3::list_objects(&regional, &bucket, &prefix, token.as_deref())?;
        folders.extend(page.folders);
        objects.extend(page.objects);
        match page.next {
            Some(t) => token = Some(t),
            None => break,
        }
    }
    // A key without the slash (s3://b/report.csv) names an object: show just that one.
    if folders.is_empty() && objects.is_empty() && !given.is_empty() && !given.ends_with('/') {
        let key = given;
        let info = s3::head_object(&regional, &bucket, &key).map_err(|e| {
            if s3::is_not_found(&e) { anyhow!("nothing at {uri}") } else { e.into() }
        })?;
        if json {
            print_json(&json!({ "key": key, "size": info.size, "modified": info.modified, "contentType": info.content_type }));
        } else {
            println!("{}  {:>9}  {}", info.modified.map(fmt_utc).unwrap_or_default(), fmt_bytes(info.size), show_invisible(&key));
        }
        return Ok(ExitCode::SUCCESS);
    }
    if json {
        let objects: Vec<_> = objects
            .iter()
            .map(|o| json!({ "key": o.key, "size": o.size, "modified": o.modified, "storageClass": o.storage_class }))
            .collect();
        print_json(&json!({ "bucket": bucket, "prefix": prefix, "folders": folders, "objects": objects }));
        return Ok(ExitCode::SUCCESS);
    }
    for f in &folders {
        println!("{:>16}  {:>9}  {}", "", paint("PRE", "36"), show_invisible(f.strip_prefix(&prefix).unwrap_or(f)));
    }
    for o in &objects {
        let modified = o.modified.map(fmt_utc).unwrap_or_default().replace(" UTC", "");
        println!("{modified:>16}  {:>9}  {}", fmt_bytes(o.size), show_invisible(o.key.strip_prefix(&prefix).unwrap_or(&o.key)));
    }
    Ok(ExitCode::SUCCESS)
}

fn url(config: &Config, env: &str, uri: &str, expires: &str) -> Result<ExitCode> {
    let (bucket, key) = object(uri)?;
    let seconds = logs::parse_since(expires).ok_or_else(|| anyhow!("--expires takes 15m, 1h, 12h, 7d…"))?;
    if seconds > 7 * 86_400 {
        bail!("--expires is at most 7d (an S3 limit)");
    }
    let aws = aws_for(config, env)?;
    let regional = s3::regional(&aws, &bucket)?;
    let link = s3::presigned_url(&regional, &bucket, &key, seconds as u64)?;
    println!("{link}");
    if let Some(ends) = regional.credentials_expiry()
        && ends < now_secs() + seconds as f64
    {
        eprintln!(
            "{}",
            paint(
                &format!(
                    "Note: the link works until {} (in {}), when your AWS session ends, not for the full {expires}.",
                    fmt_utc(ends),
                    fmt_duration(ends - now_secs())
                ),
                "33"
            )
        );
    }
    Ok(ExitCode::SUCCESS)
}

fn get(config: &Config, env: &str, uri: &str, dest: Option<PathBuf>) -> Result<ExitCode> {
    let (bucket, key) = object(uri)?;
    let aws = aws_for(config, env)?;
    let regional = s3::regional(&aws, &bucket)?;
    let name = s3::file_name(&key);
    let path = match dest {
        Some(d) if d.is_dir() => d.join(&name),
        Some(d) => d,
        None => PathBuf::from(&name),
    };
    let bytes = s3::download(&regional, &bucket, &key, &path)?;
    println!("{} Downloaded {} ({}) → {}", paint("✓", "32"), show_invisible(&s3::uri(&bucket, &key)), fmt_bytes(bytes as i64), display_path(&path));
    Ok(ExitCode::SUCCESS)
}
