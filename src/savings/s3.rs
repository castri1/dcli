//! S3: buckets whose objects never age (no lifecycle rule moves or expires them),
//! incomplete multipart uploads nobody aborts, and versioned buckets that keep every old
//! version. Sizes per storage class come from the daily BucketSizeBytes metric.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};

use super::{Finding, GB, Impact, Scan, billing, metric_data, par_map, query, query_id, size};
use crate::aws::s3::{self, Bucket, Lifecycle};
use crate::aws::{self, Aws, AwsError};
use crate::util::now_secs;

const MODULE: &str = "S3";
/// Buckets looked at, at most.
const MAX_BUCKETS: usize = 300;
/// Bytes in STANDARD with nothing aging them: listed from 10 GB, medium impact from
/// 500 GB, high from 5 TB.
const STANDARD_LOW: f64 = 10.0 * GB;
const STANDARD_MEDIUM: f64 = 500.0 * GB;
const STANDARD_HIGH: f64 = 5.0 * 1024.0 * GB;
/// Multipart uploads older than this many days count as abandoned.
const STALE_UPLOAD_DAYS: f64 = 7.0;
/// Abandoned uploads from which the impact is medium.
const STALE_MEDIUM: usize = 100;

/// The storage types of BucketSizeBytes read, with the storage class each stands for.
const CLASSES: [(&str, &str); 8] = [
    ("StandardStorage", "STANDARD"),
    ("IntelligentTieringFAStorage", "INTELLIGENT_TIERING (frequent)"),
    ("IntelligentTieringIAStorage", "INTELLIGENT_TIERING (infrequent)"),
    ("StandardIAStorage", "STANDARD_IA"),
    ("OneZoneIAStorage", "ONEZONE_IA"),
    ("GlacierInstantRetrievalStorage", "GLACIER_IR"),
    ("GlacierStorage", "GLACIER"),
    ("DeepArchiveStorage", "DEEP_ARCHIVE"),
];

/// What a bucket's configuration says.
#[derive(Debug, Clone, Default, PartialEq)]
struct Config {
    lifecycle: Option<Lifecycle>,
    versioning: String,
    /// Abandoned multipart uploads, and whether there were more than counted.
    stale_uploads: (usize, bool),
}

pub fn check(scan: &Scan) -> aws::Result<Vec<Finding>> {
    let mut buckets = s3::list_buckets(scan.aws)?;
    buckets.truncate(MAX_BUCKETS);
    // One client per region, shared by the buckets there.
    let clients: Mutex<HashMap<String, Arc<Aws>>> = Mutex::new(HashMap::new());
    let regional = |region: &str| clients.lock().unwrap().entry(region.to_string()).or_insert_with(|| scan.aws.in_region(region)).clone();
    let configs = par_map(&buckets, |b| config(scan.aws, b, &regional));
    let mut readable: Vec<(Bucket, String, Config)> = Vec::new();
    let mut first_error: Option<AwsError> = None;
    for (b, result) in buckets.into_iter().zip(configs) {
        match result {
            Ok((region, config)) => readable.push((b, region, config)),
            Err(e) if e.is_login_required() => return Err(e),
            // A bucket this role may not read (its policy says no): the others still count.
            Err(e) => {
                first_error.get_or_insert(e);
            }
        }
    }
    if readable.is_empty()
        && let Some(e) = first_error
    {
        return Err(e);
    }
    let sizes = sizes(&readable, &regional)?;
    Ok(readable
        .iter()
        .zip(sizes)
        .flat_map(|((b, region, config), classes)| findings(&b.name, region, config, &classes))
        .collect())
}

fn config(aws: &Aws, bucket: &Bucket, client: &dyn Fn(&str) -> Arc<Aws>) -> aws::Result<(String, Config)> {
    let region = match &bucket.region {
        Some(r) => r.clone(),
        None => s3::bucket_region(aws, &bucket.name)?,
    };
    let regional = client(&region);
    let config = Config {
        lifecycle: s3::lifecycle(&regional, &bucket.name)?,
        versioning: s3::versioning(&regional, &bucket.name)?,
        stale_uploads: s3::stale_uploads(&regional, &bucket.name, now_secs() - STALE_UPLOAD_DAYS * 86_400.0)?,
    };
    Ok((region, config))
}

/// Bytes per storage class of each bucket (the latest daily value), read region by region.
fn sizes(buckets: &[(Bucket, String, Config)], client: &dyn Fn(&str) -> Arc<Aws>) -> aws::Result<Vec<Vec<(&'static str, f64)>>> {
    let mut by_region: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
    for (i, (_, region, _)) in buckets.iter().enumerate() {
        by_region.entry(region.as_str()).or_default().push(i);
    }
    let mut out = vec![Vec::new(); buckets.len()];
    for (region, indexes) in by_region {
        let mut queries = Vec::new();
        for &i in &indexes {
            for (j, (storage, _)) in CLASSES.iter().enumerate() {
                let dims = [("BucketName", buckets[i].0.name.as_str()), ("StorageType", *storage)];
                queries.push(query(query_id("b", i * CLASSES.len() + j), "AWS/S3", "BucketSizeBytes", &dims, "Average", 86_400));
            }
        }
        // The metric is daily and can lag a day or two.
        let series = metric_data(&client(region), &queries, 3)?;
        for (&i, chunk) in indexes.iter().zip(series.chunks(CLASSES.len())) {
            out[i] = CLASSES
                .iter()
                .zip(chunk)
                .filter_map(|((_, class), s)| s.points.last().map(|p| (*class, p.1)).filter(|(_, b)| *b > 0.0))
                .collect();
        }
    }
    Ok(out)
}

fn findings(name: &str, region: &str, config: &Config, classes: &[(&'static str, f64)]) -> Vec<Finding> {
    let target = format!("s3://{name}/");
    let standard = classes.iter().find(|(c, _)| *c == "STANDARD").map_or(0.0, |(_, b)| *b);
    let all: f64 = classes.iter().map(|(_, b)| b).sum();
    let per_class: Vec<String> = classes.iter().map(|(class, bytes)| format!("{class}: {}", size(*bytes))).collect();
    let rules = match &config.lifecycle {
        None => "no lifecycle configuration".to_string(),
        Some(l) => format!("{} enabled lifecycle rule{}", l.rules, if l.rules == 1 { "" } else { "s" }),
    };
    let ages = config.lifecycle.as_ref().is_some_and(|l| l.ages_objects);
    let mut found = Vec::new();
    if !ages && standard >= STANDARD_LOW {
        let mut f = Finding::new(
            MODULE,
            &target,
            name,
            format!("{} in STANDARD and no lifecycle rule ages it", size(standard)),
            "Add a lifecycle rule: move objects to cheaper classes as they age (or use Intelligent-Tiering), expire what is no longer needed",
            format!("{} in STANDARD with nothing aging it", size(standard)),
            Impact::of(standard, STANDARD_MEDIUM, STANDARD_HIGH),
        )
        .kind("s3-lifecycle")
        .score(standard / STANDARD_HIGH)
        // As if it all moved to Standard-IA: lifecycle rules move what ages, so at most that.
        .billed_instead(billing::S3_STANDARD, billing::S3_STANDARD_IA, standard / GB)
        .at_most()
        .evidence(format!("{rules} · region {region}"))
        .caveat("Infrequent-access classes bill retrievals and keep objects a minimum time: they suit data read rarely.");
        for line in &per_class {
            f = f.evidence(line.clone());
        }
        found.push(f);
    }
    let (stale, more) = config.stale_uploads;
    if stale > 0 && !config.lifecycle.as_ref().is_some_and(|l| l.aborts_uploads) {
        let count = format!("{stale}{}", if more { "+" } else { "" });
        found.push(
            Finding::new(
                MODULE,
                &target,
                name,
                format!("{count} incomplete multipart uploads older than {STALE_UPLOAD_DAYS} days"),
                "Add a lifecycle rule that aborts incomplete multipart uploads after 7 days",
                format!("the parts of {count} uploads, stored but in no listing"),
                if stale >= STALE_MEDIUM { Impact::Medium } else { Impact::Low },
            )
            .kind("s3-multipart")
            .score(stale as f64 / 1000.0)
            .evidence(format!("ListMultipartUploads: {count} started more than {STALE_UPLOAD_DAYS} days ago"))
            .evidence(rules.clone())
            .caveat("A tool may still finish an old upload; few do after a week."),
        );
    }
    if config.versioning == "Enabled" && !config.lifecycle.as_ref().is_some_and(|l| l.ages_versions) {
        let mut f = Finding::new(
            MODULE,
            &target,
            name,
            "versioned, and every old version is kept forever",
            "Add a lifecycle rule that expires noncurrent versions after some days (keep a few if you need undo)",
            format!("the noncurrent versions within {} stored", size(all)),
            if all >= STANDARD_MEDIUM { Impact::Medium } else { Impact::Low },
        )
        .kind("s3-versions")
        .score(all / STANDARD_HIGH)
        .evidence(format!("versioning Enabled · {rules}"))
        .caveat("CloudWatch does not split current from noncurrent bytes: S3 Storage Lens or an inventory tells how much is old versions.");
        for line in &per_class {
            f = f.evidence(line.clone());
        }
        found.push(f);
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(lifecycle: Option<Lifecycle>, versioning: &str, stale: usize) -> Config {
        Config { lifecycle, versioning: versioning.into(), stale_uploads: (stale, false) }
    }

    #[test]
    fn buckets_without_rules() {
        let classes = [("STANDARD", 620.0 * GB), ("STANDARD_IA", 3.0 * GB)];
        let found = findings("assets", "us-east-1", &config(None, "", 0), &classes);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].target, "s3://assets/");
        assert_eq!(found[0].reduction, "620 GB in STANDARD with nothing aging it");
        assert_eq!(found[0].impact, Impact::Medium);
        assert!(found[0].evidence.contains(&"STANDARD_IA: 3.0 GB".to_string()));
        // A rule that ages objects, or a small bucket: nothing to say.
        let aging = Lifecycle { rules: 1, ages_objects: true, ..Default::default() };
        assert!(findings("assets", "us-east-1", &config(Some(aging), "", 0), &classes).is_empty());
        assert!(findings("small", "us-east-1", &config(None, "", 0), &[("STANDARD", 2.0 * GB)]).is_empty());
    }

    #[test]
    fn abandoned_uploads_and_old_versions() {
        let found = findings("logs", "us-west-2", &config(None, "Enabled", 12), &[("GLACIER_IR", 40.0 * GB)]);
        let summaries: Vec<&str> = found.iter().map(|f| f.summary.as_str()).collect();
        assert_eq!(summaries, ["12 incomplete multipart uploads older than 7 days", "versioned, and every old version is kept forever"]);
        assert_eq!(found[1].reduction, "the noncurrent versions within 40.0 GB stored");
        let covered = Lifecycle { rules: 2, ages_objects: true, aborts_uploads: true, ages_versions: true };
        assert!(findings("logs", "us-west-2", &config(Some(covered), "Enabled", 12), &[("GLACIER_IR", 40.0 * GB)]).is_empty());
        // Suspended versioning keeps what it has but adds nothing new.
        assert!(findings("logs", "us-west-2", &config(None, "Suspended", 0), &[]).is_empty());
    }
}
