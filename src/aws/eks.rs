//! EKS (restJson, signing name `eks`): clusters as AWS sees them from outside — the Kubernetes
//! version and its support dates (extended support costs about six times the standard
//! cluster-hour), health, node groups, add-ons and their newer versions, Fargate profiles,
//! upgrade insights (deprecated APIs still called) and who has access. Read only. Inside the
//! cluster is [`super::k8s`]; the token both use is [`token`].

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::PathBuf;

use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use super::{Aws, AwsError, Result, Service, sts};
use crate::util::{fmt_utc, md_block, md_code, now_secs};

pub const SERVICE: Service = Service { endpoint: "eks", signing_name: "eks", target_prefix: "" };

/// Clusters described at most.
const MAX_CLUSTERS: usize = 100;
/// Seconds a day.
const DAY: f64 = 86_400.0;
/// From this close to the end of standard support, a cluster is flagged.
pub const SOON_DAYS: f64 = 90.0;

pub const TOKEN_PREFIX: &str = "k8s-aws-v1.";
/// What `aws eks get-token` gives a token: the presigned URL is valid a minute, EKS accepts
/// the token for 15, and clients reuse it until a minute before that.
const TOKEN_LIFETIME_SECS: i64 = 14 * 60;

fn get<T: DeserializeOwned>(aws: &Aws, segments: &[&str], query: &[(&str, &str)]) -> Result<T> {
    let query: Vec<(String, String)> = query.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
    aws.rest(&SERVICE, "GET", segments, &query, None)
}

/// Follows `nextToken` until the end (or `max` items).
fn paged<T>(max: usize, mut page: impl FnMut(Option<&str>) -> Result<(Vec<T>, Option<String>)>) -> Result<Vec<T>> {
    let (mut all, mut token) = (Vec::new(), None::<String>);
    loop {
        let (items, next) = page(token.as_deref())?;
        all.extend(items);
        match next.filter(|t| !t.is_empty()) {
            Some(t) if all.len() < max => token = Some(t),
            _ => return Ok(all),
        }
    }
}

/// The names of a `List…` response under `key`, and its `nextToken`.
fn names(aws: &Aws, segments: &[&str], key: &str) -> Result<Vec<String>> {
    paged(1000, |token| {
        let mut query = vec![("maxResults", "100")];
        if let Some(t) = token {
            query.push(("nextToken", t));
        }
        let v: Value = get(aws, segments, &query)?;
        let items = v.get(key).and_then(Value::as_array).map(|a| a.iter().filter_map(|s| s.as_str().map(str::to_string)).collect()).unwrap_or_default();
        Ok((items, v.get("nextToken").and_then(Value::as_str).map(str::to_string)))
    })
}

// ---- clusters -----------------------------------------------------------------------

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Cluster {
    pub name: String,
    pub arn: String,
    pub created_at: Option<f64>,
    pub version: String,
    pub endpoint: Option<String>,
    pub status: String,
    pub platform_version: String,
    pub certificate_authority: Option<Authority>,
    pub resources_vpc_config: VpcConfig,
    pub logging: Logging,
    pub health: Health,
    pub access_config: AccessConfig,
    pub upgrade_policy: Option<UpgradePolicy>,
    pub compute_config: Option<ComputeConfig>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Authority {
    pub data: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct VpcConfig {
    pub vpc_id: Option<String>,
    pub endpoint_public_access: bool,
    pub endpoint_private_access: bool,
    pub public_access_cidrs: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Logging {
    pub cluster_logging: Vec<LogSetup>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct LogSetup {
    pub types: Vec<String>,
    pub enabled: bool,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Health {
    pub issues: Vec<Issue>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Issue {
    pub code: String,
    pub message: String,
    pub resource_ids: Vec<String>,
}

impl Issue {
    pub fn text(&self) -> String {
        let ids = if self.resource_ids.is_empty() { String::new() } else { format!(" ({})", self.resource_ids.join(", ")) };
        if self.message.is_empty() { format!("{}{ids}", self.code) } else { format!("{}: {}{ids}", self.code, self.message) }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AccessConfig {
    /// `API`, `API_AND_CONFIG_MAP` or `CONFIG_MAP`.
    pub authentication_mode: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct UpgradePolicy {
    /// `STANDARD` (upgraded by AWS when standard support ends) or `EXTENDED`.
    pub support_type: String,
}

/// EKS Auto Mode.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ComputeConfig {
    pub enabled: bool,
    pub node_pools: Vec<String>,
}

impl Cluster {
    /// The cluster's certificate authority, PEM.
    pub fn ca_pem(&self) -> Option<Vec<u8>> {
        crate::util::base64_decode(&self.certificate_authority.as_ref()?.data).filter(|p| !p.is_empty())
    }

    pub fn auth_mode(&self) -> &str {
        self.access_config.authentication_mode.as_deref().unwrap_or("CONFIG_MAP")
    }

    /// What the cluster does when standard support ends: `EXTENDED` unless set to `STANDARD`.
    pub fn support_type(&self) -> &str {
        self.upgrade_policy.as_ref().map(|u| u.support_type.as_str()).filter(|s| !s.is_empty()).unwrap_or("EXTENDED")
    }

    pub fn auto_mode(&self) -> bool {
        self.compute_config.as_ref().is_some_and(|c| c.enabled)
    }

    /// The control plane log types sent to CloudWatch Logs.
    pub fn logs_enabled(&self) -> Vec<String> {
        self.logging.cluster_logging.iter().filter(|l| l.enabled).flat_map(|l| l.types.clone()).collect()
    }

    /// Where the API endpoint answers.
    pub fn endpoint_access(&self) -> String {
        let v = &self.resources_vpc_config;
        let open = v.public_access_cidrs.is_empty() || v.public_access_cidrs.iter().any(|c| c == "0.0.0.0/0");
        let public = if open { "public".to_string() } else { format!("public from {}", v.public_access_cidrs.join(", ")) };
        match (v.endpoint_public_access, v.endpoint_private_access) {
            (true, true) => format!("{public} and private"),
            (true, false) => public,
            (false, _) => "private only (from the cluster's VPC)".to_string(),
        }
    }

    pub fn log_group(&self) -> String {
        format!("/aws/eks/{}/cluster", self.name)
    }
}

pub fn list_clusters(aws: &Aws) -> Result<Vec<String>> {
    names(aws, &["clusters"], "clusters")
}

pub fn describe_cluster(aws: &Aws, name: &str) -> Result<Cluster> {
    #[derive(Deserialize)]
    struct Out {
        cluster: Cluster,
    }
    Ok(get::<Out>(aws, &["clusters", name], &[])?.cluster)
}

/// The Kubernetes versions EKS has, with their support dates.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ClusterVersion {
    pub cluster_version: String,
    pub release_date: Option<f64>,
    pub end_of_standard_support_date: Option<f64>,
    pub end_of_extended_support_date: Option<f64>,
    /// `STANDARD_SUPPORT`, `EXTENDED_SUPPORT` or `UNSUPPORTED`.
    pub version_status: String,
    pub default_version: bool,
}

pub fn cluster_versions(aws: &Aws) -> Result<Vec<ClusterVersion>> {
    paged(500, |token| {
        let mut query = vec![("includeAll", "true"), ("maxResults", "100")];
        if let Some(t) = token {
            query.push(("nextToken", t));
        }
        #[derive(Deserialize, Default)]
        #[serde(rename_all = "camelCase", default)]
        struct Out {
            cluster_versions: Vec<ClusterVersion>,
            next_token: Option<String>,
        }
        let out: Out = get(aws, &["cluster-versions"], &query)?;
        Ok((out.cluster_versions, out.next_token))
    })
}

/// Where a cluster's Kubernetes version stands.
#[derive(Debug, Clone, PartialEq)]
pub enum Support {
    /// Standard support until `ends` (Unix seconds).
    Standard { ends: Option<f64> },
    /// Past standard support: billed at the extended rate until `ends`, then upgraded by AWS.
    Extended { since: Option<f64>, ends: Option<f64> },
    Unsupported,
    /// The version is not in EKS's list (no permission to read it, say).
    Unknown,
}

impl Support {
    /// For a version, from EKS's list (`versions`) at `now`.
    pub fn of(version: &str, versions: &[ClusterVersion], now: f64) -> Support {
        let Some(v) = versions.iter().find(|v| v.cluster_version == version) else { return Support::Unknown };
        let standard_ended = v.end_of_standard_support_date.is_some_and(|d| d <= now);
        let extended_ended = v.end_of_extended_support_date.is_some_and(|d| d <= now);
        match v.version_status.as_str() {
            "UNSUPPORTED" => Support::Unsupported,
            "EXTENDED_SUPPORT" => Support::Extended { since: v.end_of_standard_support_date, ends: v.end_of_extended_support_date },
            "STANDARD_SUPPORT" => Support::Standard { ends: v.end_of_standard_support_date },
            _ if extended_ended => Support::Unsupported,
            _ if standard_ended => Support::Extended { since: v.end_of_standard_support_date, ends: v.end_of_extended_support_date },
            _ => Support::Standard { ends: v.end_of_standard_support_date },
        }
    }

    pub fn extended(&self) -> bool {
        matches!(self, Support::Extended { .. })
    }

    /// Days until standard support ends, while it lasts.
    pub fn standard_days_left(&self, now: f64) -> Option<f64> {
        match self {
            Support::Standard { ends: Some(e) } => Some(((e - now) / DAY).max(0.0)),
            _ => None,
        }
    }

    /// One line: "standard support until 2026-12-20 (45 days)".
    pub fn describe(&self, cluster: &Cluster, now: f64) -> String {
        let days = |d: f64| format!("{:.0} day{}", d, if d.round() == 1.0 { "" } else { "s" });
        let date = |d: f64| fmt_utc(d).split(' ').next().unwrap_or_default().to_string();
        match self {
            Support::Standard { ends: Some(e) } => {
                let left = ((e - now) / DAY).max(0.0);
                let after = if cluster.support_type() == "STANDARD" {
                    "then AWS upgrades it".to_string()
                } else {
                    "then extended support, ~6× the cluster-hour".to_string()
                };
                format!("standard support until {} ({}), {after}", date(*e), days(left))
            }
            Support::Standard { ends: None } => "standard support".to_string(),
            Support::Extended { ends, .. } => {
                let until = ends.map(|e| format!(" until {} ({})", date(e), days(((e - now) / DAY).max(0.0)))).unwrap_or_default();
                format!("EXTENDED support{until}: billed ~6× the standard cluster-hour")
            }
            Support::Unsupported => "no longer supported: AWS upgrades it".to_string(),
            Support::Unknown => "support dates unknown".to_string(),
        }
    }

    /// A few words for a list: "standard · 45 days left", "EXTENDED ~6× · until 2026-11-25".
    pub fn short(&self, now: f64) -> String {
        let date = |d: f64| fmt_utc(d).split(' ').next().unwrap_or_default().to_string();
        match self {
            Support::Standard { ends: Some(e) } => format!("standard · {:.0} days left", ((e - now) / DAY).max(0.0)),
            Support::Standard { ends: None } => "standard".to_string(),
            Support::Extended { ends: Some(e), .. } => format!("EXTENDED ~6× · until {}", date(*e)),
            Support::Extended { ends: None, .. } => "EXTENDED ~6×".to_string(),
            Support::Unsupported => "unsupported".to_string(),
            Support::Unknown => "?".to_string(),
        }
    }

    /// Needs attention: in extended support, unsupported, or standard support ending soon.
    pub fn attention(&self, now: f64) -> bool {
        match self {
            Support::Extended { .. } | Support::Unsupported => true,
            s => s.standard_days_left(now).is_some_and(|d| d <= SOON_DAYS),
        }
    }
}

/// The clusters of the region, described, and EKS's versions (empty when they cannot be read).
pub struct Overview {
    pub clusters: Vec<Cluster>,
    pub versions: Vec<ClusterVersion>,
}

pub fn overview(aws: &Aws) -> Result<Overview> {
    let mut names = list_clusters(aws)?;
    names.truncate(MAX_CLUSTERS);
    let (described, versions) = std::thread::scope(|s| {
        let versions = s.spawn(|| cluster_versions(aws).unwrap_or_default());
        let jobs: Vec<_> = names.iter().map(|n| s.spawn(move || describe_cluster(aws, n))).collect();
        let described: Vec<Result<Cluster>> = jobs.into_iter().map(|j| j.join().unwrap_or_else(|_| Err(AwsError::Network("thread failed".into())))).collect();
        (described, versions.join().unwrap_or_default())
    });
    let mut clusters = Vec::new();
    for (name, c) in names.iter().zip(described) {
        match c {
            Ok(c) => clusters.push(c),
            // Deleted between the list and the describe.
            Err(AwsError::Api { status: 404, .. }) => {}
            Err(e) if clusters.is_empty() && name == &names[0] => return Err(e),
            Err(_) => clusters.push(Cluster { name: name.clone(), status: "UNKNOWN".into(), ..Cluster::default() }),
        }
    }
    clusters.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(Overview { clusters, versions })
}

// ---- node groups, add-ons, Fargate ------------------------------------------------------

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Nodegroup {
    pub nodegroup_name: String,
    pub status: String,
    /// `ON_DEMAND`, `SPOT` or `CAPACITY_BLOCK`.
    pub capacity_type: Option<String>,
    pub scaling_config: Scaling,
    pub instance_types: Vec<String>,
    pub ami_type: Option<String>,
    pub release_version: Option<String>,
    pub version: Option<String>,
    pub health: Health,
    pub modified_at: Option<f64>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Scaling {
    pub min_size: i64,
    pub max_size: i64,
    pub desired_size: i64,
}

impl Nodegroup {
    pub fn ok(&self) -> bool {
        self.status == "ACTIVE" && self.health.issues.is_empty()
    }
}

pub fn nodegroups(aws: &Aws, cluster: &str) -> Result<Vec<Nodegroup>> {
    #[derive(Deserialize)]
    struct Out {
        nodegroup: Nodegroup,
    }
    let names = names(aws, &["clusters", cluster, "node-groups"], "nodegroups")?;
    each(&names, |n| Ok(get::<Out>(aws, &["clusters", cluster, "node-groups", n], &[])?.nodegroup))
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Addon {
    pub addon_name: String,
    pub addon_version: String,
    pub status: String,
    pub health: Health,
    pub service_account_role_arn: Option<String>,
    pub modified_at: Option<f64>,
    /// The newest version compatible with the cluster's Kubernetes version, when newer than
    /// this one (from DescribeAddonVersions).
    #[serde(skip)]
    pub newer: Option<String>,
}

impl Addon {
    pub fn ok(&self) -> bool {
        self.status == "ACTIVE" && self.health.issues.is_empty()
    }
}

/// `v1.18.3-eksbuild.2` → [1, 18, 3, 2], to compare versions.
pub fn version_key(v: &str) -> Vec<u64> {
    v.split(|c: char| !c.is_ascii_digit()).filter(|p| !p.is_empty()).filter_map(|p| p.parse().ok()).collect()
}

pub fn addons(aws: &Aws, cluster: &str, k8s_version: &str) -> Result<Vec<Addon>> {
    #[derive(Deserialize)]
    struct Out {
        addon: Addon,
    }
    let names = names(aws, &["clusters", cluster, "addons"], "addons")?;
    each(&names, |n| {
        let mut addon = get::<Out>(aws, &["clusters", cluster, "addons", n], &[])?.addon;
        // A newer version is a hint; without permission to read versions, none is shown.
        if let Ok(Some(latest)) = latest_addon_version(aws, n, k8s_version)
            && version_key(&latest) > version_key(&addon.addon_version)
        {
            addon.newer = Some(latest);
        }
        Ok(addon)
    })
}

/// The newest version of add-on `name` that runs on Kubernetes `k8s_version`.
fn latest_addon_version(aws: &Aws, name: &str, k8s_version: &str) -> Result<Option<String>> {
    let v: Value = get(aws, &["addons", "supported-versions"], &[("addonName", name), ("kubernetesVersion", k8s_version)])?;
    let versions = v.pointer("/addons/0/addonVersions").and_then(Value::as_array).cloned().unwrap_or_default();
    let compatible = versions.iter().filter(|av| {
        av.get("compatibilities")
            .and_then(Value::as_array)
            .is_none_or(|c| c.is_empty() || c.iter().any(|x| x.get("clusterVersion").and_then(Value::as_str) == Some(k8s_version)))
    });
    Ok(compatible.filter_map(|av| av.get("addonVersion").and_then(Value::as_str)).max_by_key(|s| version_key(s)).map(str::to_string))
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct FargateProfile {
    pub fargate_profile_name: String,
    pub status: String,
    pub selectors: Vec<Selector>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Selector {
    pub namespace: String,
    pub labels: BTreeMap<String, String>,
}

impl FargateProfile {
    /// `batch, jobs (app=report)`.
    pub fn selects(&self) -> String {
        self.selectors
            .iter()
            .map(|s| {
                if s.labels.is_empty() {
                    s.namespace.clone()
                } else {
                    let labels: Vec<String> = s.labels.iter().map(|(k, v)| format!("{k}={v}")).collect();
                    format!("{} ({})", s.namespace, labels.join(", "))
                }
            })
            .collect::<Vec<_>>()
            .join(", ")
    }
}

pub fn fargate_profiles(aws: &Aws, cluster: &str) -> Result<Vec<FargateProfile>> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Out {
        fargate_profile: FargateProfile,
    }
    let names = names(aws, &["clusters", cluster, "fargate-profiles"], "fargateProfileNames")?;
    each(&names, |n| Ok(get::<Out>(aws, &["clusters", cluster, "fargate-profiles", n], &[])?.fargate_profile))
}

/// Runs `job` on every name in parallel (a few threads at a time), in order.
fn each<T: Send>(names: &[String], job: impl Fn(&str) -> Result<T> + Sync) -> Result<Vec<T>> {
    let mut out = Vec::with_capacity(names.len());
    for chunk in names.chunks(8) {
        let results: Vec<Result<T>> = std::thread::scope(|s| {
            let jobs: Vec<_> = chunk.iter().map(|n| s.spawn(|| job(n))).collect();
            jobs.into_iter().map(|j| j.join().unwrap_or_else(|_| Err(AwsError::Network("thread failed".into())))).collect()
        });
        for r in results {
            out.push(r?);
        }
    }
    Ok(out)
}

// ---- insights ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Insight {
    pub id: String,
    pub name: String,
    /// `UPGRADE_READINESS` or `MISCONFIGURATION`.
    pub category: String,
    pub kubernetes_version: Option<String>,
    pub description: String,
    pub insight_status: InsightStatus,
    pub recommendation: Option<String>,
    pub resources: Vec<InsightResource>,
    pub category_specific_summary: Option<CategorySummary>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct InsightStatus {
    /// `PASSING`, `WARNING`, `ERROR` or `UNKNOWN`.
    pub status: String,
    pub reason: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct InsightResource {
    pub kubernetes_resource_uri: Option<String>,
    pub arn: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct CategorySummary {
    pub deprecation_details: Vec<Deprecation>,
    pub addon_compatibility_details: Vec<AddonCompatibility>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Deprecation {
    /// The API still called: `/apis/flowcontrol.apiserver.k8s.io/v1beta3/…`.
    pub usage: String,
    pub replaced_with: Option<String>,
    pub stop_serving_version: Option<String>,
    pub client_stats: Vec<ClientStat>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ClientStat {
    pub user_agent: String,
    pub number_of_requests_last30_days: i64,
    pub last_request_time: Option<f64>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AddonCompatibility {
    pub name: String,
    pub compatible_versions: Vec<String>,
}

impl Insight {
    pub fn passing(&self) -> bool {
        self.insight_status.status == "PASSING"
    }
}

/// The cluster's insights, those not passing described in full (which APIs, which clients).
pub fn insights(aws: &Aws, cluster: &str) -> Result<Vec<Insight>> {
    #[derive(Deserialize, Default)]
    #[serde(rename_all = "camelCase", default)]
    struct List {
        insights: Vec<Insight>,
        next_token: Option<String>,
    }
    #[derive(Deserialize)]
    struct One {
        insight: Insight,
    }
    let list = paged(500, |token| {
        let mut body = json!({ "maxResults": 100 });
        if let Some(t) = token {
            body["nextToken"] = json!(t);
        }
        let out: List = aws.rest(&SERVICE, "POST", &["clusters", cluster, "insights"], &[], Some(&body))?;
        Ok((out.insights, out.next_token))
    })?;
    let (passing, failing): (Vec<Insight>, Vec<Insight>) = list.into_iter().partition(Insight::passing);
    let ids: Vec<String> = failing.iter().map(|i| i.id.clone()).collect();
    let mut described = each(&ids, |id| Ok(get::<One>(aws, &["clusters", cluster, "insights", id], &[])?.insight))?;
    described.sort_by_key(|i| if i.insight_status.status == "ERROR" { 0 } else { 1 });
    described.extend(passing);
    Ok(described)
}

// ---- access -------------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AccessEntry {
    pub principal_arn: String,
    pub kubernetes_groups: Vec<String>,
    pub username: Option<String>,
    /// `STANDARD`, `EC2_LINUX`, `FARGATE_LINUX`…
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(skip)]
    pub policies: Vec<AccessPolicy>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AccessPolicy {
    pub policy_arn: String,
    pub access_scope: AccessScope,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct AccessScope {
    #[serde(rename = "type")]
    pub kind: String,
    pub namespaces: Vec<String>,
}

impl AccessPolicy {
    /// `AmazonEKSViewPolicy (shop, web)`.
    pub fn label(&self) -> String {
        let name = self.policy_arn.rsplit('/').next().unwrap_or(&self.policy_arn);
        if self.access_scope.kind == "namespace" {
            format!("{name} ({})", self.access_scope.namespaces.join(", "))
        } else {
            name.to_string()
        }
    }
}

/// Who may use the cluster through access entries; `None` when it uses only the aws-auth
/// ConfigMap (read inside the cluster, not here).
pub fn access_entries(aws: &Aws, cluster: &Cluster) -> Result<Option<Vec<AccessEntry>>> {
    if cluster.auth_mode() == "CONFIG_MAP" {
        return Ok(None);
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct One {
        access_entry: AccessEntry,
    }
    #[derive(Deserialize, Default)]
    #[serde(rename_all = "camelCase", default)]
    struct Policies {
        associated_access_policies: Vec<AccessPolicy>,
    }
    let name = cluster.name.as_str();
    let arns = names(aws, &["clusters", name, "access-entries"], "accessEntries")?;
    let entries = each(&arns, |arn| {
        let mut entry = get::<One>(aws, &["clusters", name, "access-entries", arn], &[])?.access_entry;
        entry.policies = get::<Policies>(aws, &["clusters", name, "access-entries", arn, "access-policies"], &[])?.associated_access_policies;
        Ok(entry)
    })?;
    Ok(Some(entries))
}

// ---- a cluster in full --------------------------------------------------------------------

/// What the cluster screen and the report show. Each part fails on its own (a missing
/// permission hides one part, not the cluster).
#[derive(Clone)]
pub struct Detail {
    pub cluster: Cluster,
    pub support: Support,
    pub nodegroups: Result<Vec<Nodegroup>>,
    pub addons: Result<Vec<Addon>>,
    pub fargate: Result<Vec<FargateProfile>>,
    pub insights: Result<Vec<Insight>>,
    pub access: Result<Option<Vec<AccessEntry>>>,
}

impl Detail {
    /// Things that need attention, one line each, worst first.
    pub fn problems(&self, now: f64) -> Vec<String> {
        let c = &self.cluster;
        let mut out = Vec::new();
        if c.status != "ACTIVE" {
            out.push(format!("Cluster {}", c.status));
        }
        out.extend(c.health.issues.iter().map(|i| format!("Cluster: {}", i.text())));
        if self.support.attention(now) {
            out.push(format!("Kubernetes {}: {}", c.version, self.support.describe(c, now)));
        }
        for n in self.nodegroups.iter().flatten().filter(|n| !n.ok()) {
            out.push(format!("Node group {} {}", n.nodegroup_name, n.status));
            out.extend(n.health.issues.iter().map(|i| format!("Node group {}: {}", n.nodegroup_name, i.text())));
        }
        for n in self.nodegroups.iter().flatten() {
            if let Some(v) = n.version.as_deref().filter(|v| !v.is_empty() && *v != c.version) {
                out.push(format!("Node group {} runs Kubernetes {v}, the control plane {}", n.nodegroup_name, c.version));
            }
        }
        for a in self.addons.iter().flatten().filter(|a| !a.ok()) {
            out.push(format!("Add-on {} {}", a.addon_name, a.status));
            out.extend(a.health.issues.iter().map(|i| format!("Add-on {}: {}", a.addon_name, i.text())));
        }
        for i in self.insights.iter().flatten().filter(|i| !i.passing()) {
            out.push(format!("Insight {}: {}", i.insight_status.status, i.name));
        }
        out
    }
}

pub fn detail(aws: &Aws, name: &str, versions: &[ClusterVersion]) -> Result<Detail> {
    let cluster = describe_cluster(aws, name)?;
    let support = Support::of(&cluster.version, versions, now_secs());
    let c = &cluster;
    let (nodegroups, addons, fargate, insights, access) = std::thread::scope(|s| {
        let n = s.spawn(|| nodegroups(aws, name));
        let a = s.spawn(|| addons(aws, name, &c.version));
        let f = s.spawn(|| fargate_profiles(aws, name));
        let i = s.spawn(|| insights(aws, name));
        let e = s.spawn(|| access_entries(aws, c));
        let lost = || AwsError::Network("thread failed".into());
        (
            n.join().unwrap_or_else(|_| Err(lost())),
            a.join().unwrap_or_else(|_| Err(lost())),
            f.join().unwrap_or_else(|_| Err(lost())),
            i.join().unwrap_or_else(|_| Err(lost())),
            e.join().unwrap_or_else(|_| Err(lost())),
        )
    });
    Ok(Detail { cluster, support, nodegroups, addons, fargate, insights, access })
}

pub fn console_url(region: &str, cluster: &str) -> String {
    super::console_url(region, &format!("eks/home?region={region}#/clusters/{}", crate::util::uri_encode(cluster)))
}

// ---- tokens and kubeconfig ------------------------------------------------------------------

fn base64url(bytes: &[u8]) -> String {
    crate::util::base64(bytes).trim_end_matches('=').replace('+', "-").replace('/', "_")
}

/// A token for the Kubernetes API of `cluster`, as `aws eks get-token` makes one: a presigned
/// STS GetCallerIdentity (of the cluster's region) signed with the cluster's name in
/// `x-k8s-aws-id`, which EKS sends to STS to learn who is calling.
pub fn token(aws: &Aws, cluster: &str) -> Result<String> {
    let params = [("Action".to_string(), "GetCallerIdentity".to_string()), ("Version".to_string(), sts::VERSION.to_string())];
    let headers = [("x-k8s-aws-id".to_string(), cluster.to_string())];
    let url = aws.presign_api(&sts::SERVICE, &params, &headers, 60)?;
    Ok(format!("{TOKEN_PREFIX}{}", base64url(url.as_bytes())))
}

/// The ExecCredential a kubeconfig's exec plugin prints for kubectl and k9s.
pub fn exec_credential(token: &str, now: f64) -> Value {
    json!({
        "kind": "ExecCredential",
        "apiVersion": "client.authentication.k8s.io/v1beta1",
        "spec": {},
        "status": {
            "expirationTimestamp": crate::util::rfc3339(now as i64 + TOKEN_LIFETIME_SECS),
            "token": token,
        },
    })
}

/// `~/.deployment-cli/kube/<env>-<cluster>.yaml`: dcli's own kubeconfig for a cluster (it never
/// touches `~/.kube/config`).
pub fn kubeconfig_path(env: &str, cluster: &str) -> Option<PathBuf> {
    let name = format!("{}-{}.yaml", crate::costs::file_name(env), crate::costs::file_name(cluster));
    Some(crate::util::home_dir()?.join(".deployment-cli").join("kube").join(name))
}

/// A kubeconfig for `cluster` whose user runs `command args…` for tokens (dcli itself, see
/// [`token_command`]). Scalars are JSON-quoted, which YAML reads as they are.
pub fn kubeconfig(cluster: &Cluster, context: &str, command: &str, args: &[String]) -> String {
    let q = |s: &str| serde_json::to_string(s).unwrap_or_default();
    let server = cluster.endpoint.as_deref().unwrap_or_default();
    let ca = cluster.certificate_authority.as_ref().map(|a| a.data.trim()).filter(|d| !d.is_empty());
    let id = if cluster.arn.is_empty() { cluster.name.as_str() } else { cluster.arn.as_str() };
    let mut out = String::new();
    let _ = writeln!(out, "# Written by dcli for {} — the token comes from dcli itself.", cluster.name);
    let _ = writeln!(out, "apiVersion: v1\nkind: Config\nclusters:\n- name: {}\n  cluster:\n    server: {}", q(id), q(server));
    if let Some(ca) = ca {
        let _ = writeln!(out, "    certificate-authority-data: {}", q(ca));
    }
    let _ = writeln!(out, "contexts:\n- name: {}\n  context:\n    cluster: {}\n    user: {}", q(context), q(id), q(context));
    let _ = writeln!(out, "current-context: {}\nusers:\n- name: {}\n  user:\n    exec:", q(context), q(context));
    let _ = writeln!(out, "      apiVersion: client.authentication.k8s.io/v1beta1\n      command: {}", q(command));
    let _ = writeln!(out, "      args: [{}]", args.iter().map(|a| q(a)).collect::<Vec<_>>().join(", "));
    let _ = writeln!(out, "      interactiveMode: Never\n      provideClusterInfo: false");
    out
}

/// The program and arguments a kubeconfig runs for a token: this dcli binary, `--config` when
/// it was given a config file, then `eks ENV CLUSTER --token`.
pub fn token_command(config_file: Option<&std::path::Path>, env: &str, cluster: &str) -> (String, Vec<String>) {
    let exe = std::env::current_exe().ok();
    #[cfg(unix)]
    let exe = exe.map(|p| p.canonicalize().unwrap_or(p));
    let command = exe.map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(crate::util::program);
    let mut args = Vec::new();
    if let Some(path) = config_file {
        let path = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
        args.extend(["--config".to_string(), path.to_string_lossy().into_owned()]);
    }
    args.extend(["eks".to_string(), env.to_string(), cluster.to_string(), "--token".to_string()]);
    (command, args)
}

/// Writes dcli's kubeconfig for `cluster` (readable only by the user) and returns its path.
pub fn write_kubeconfig(cluster: &Cluster, env: &str, config_file: Option<&std::path::Path>) -> std::result::Result<PathBuf, String> {
    let path = kubeconfig_path(env, &cluster.name).ok_or("no home folder")?;
    let (command, args) = token_command(config_file, env, &cluster.name);
    let text = kubeconfig(cluster, &format!("{env}-{}", cluster.name), &command, &args);
    crate::platform::write_private(&path, text.as_bytes()).map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    Ok(path)
}

// ---- report ---------------------------------------------------------------------------------

/// Markdown for an AI agent: what is wrong with a cluster, from AWS and (when it could be
/// read) from inside it. `inside`: failing workloads, or why they could not be read.
pub fn report(env: &str, region: &str, d: &Detail, inside: Option<std::result::Result<&super::k8s::Findings, String>>, now: f64) -> String {
    let c = &d.cluster;
    let mut md = String::new();
    let _ = writeln!(md, "# EKS cluster {} ({env}, {region})\n", md_code(&c.name));
    let _ = writeln!(md, "- **Status:** {} · Kubernetes {} (platform {})", c.status, c.version, c.platform_version);
    let _ = writeln!(md, "- **Support:** {}", d.support.describe(c, now));
    let _ = writeln!(md, "- **When standard support ends:** {}", if c.support_type() == "STANDARD" { "AWS upgrades it" } else { "it goes to extended support" });
    let _ = writeln!(md, "- **Endpoint:** {} · authentication mode {}", c.endpoint_access(), c.auth_mode());
    if c.auto_mode() {
        let pools = c.compute_config.as_ref().map(|cc| cc.node_pools.join(", ")).unwrap_or_default();
        let _ = writeln!(md, "- **Auto Mode:** on (node pools: {pools})");
    }
    let logs = c.logs_enabled();
    let _ = writeln!(md, "- **Control plane logs:** {}", if logs.is_empty() { "off".to_string() } else { format!("{} in {}", logs.join(", "), md_code(&c.log_group())) });
    let problems = d.problems(now);
    let _ = writeln!(md, "\n## What needs attention\n");
    if problems.is_empty() {
        let _ = writeln!(md, "Nothing, as AWS sees the cluster.");
    }
    for p in &problems {
        let _ = writeln!(md, "- {}", md_code(p));
    }
    match &d.nodegroups {
        Ok(list) if !list.is_empty() => {
            let _ = writeln!(md, "\n## Node groups\n");
            for n in list {
                let s = &n.scaling_config;
                let _ = writeln!(
                    md,
                    "- {} — {} · {} · {} nodes (min {}, max {}) · {} · Kubernetes {} · AMI {}",
                    md_code(&n.nodegroup_name),
                    n.status,
                    n.capacity_type.as_deref().unwrap_or("ON_DEMAND"),
                    s.desired_size,
                    s.min_size,
                    s.max_size,
                    n.instance_types.join(", "),
                    n.version.as_deref().unwrap_or("?"),
                    n.release_version.as_deref().unwrap_or("?"),
                );
                for i in &n.health.issues {
                    let _ = writeln!(md, "  - issue: {}", md_code(&i.text()));
                }
            }
        }
        Ok(_) => {}
        Err(e) => {
            let _ = writeln!(md, "\n## Node groups\n\nCould not be read: {}", md_code(&e.short()));
        }
    }
    if let Ok(list) = &d.addons {
        let notable: Vec<&Addon> = list.iter().filter(|a| !a.ok() || a.newer.is_some()).collect();
        if !notable.is_empty() {
            let _ = writeln!(md, "\n## Add-ons\n");
            for a in notable {
                let newer = a.newer.as_deref().map(|v| format!(" · {} is available", md_code(v))).unwrap_or_default();
                let _ = writeln!(md, "- {} {} — {}{newer}", md_code(&a.addon_name), md_code(&a.addon_version), a.status);
                for i in &a.health.issues {
                    let _ = writeln!(md, "  - issue: {}", md_code(&i.text()));
                }
            }
        }
    }
    if let Ok(list) = &d.insights {
        let failing: Vec<&Insight> = list.iter().filter(|i| !i.passing()).collect();
        if !failing.is_empty() {
            let _ = writeln!(md, "\n## Upgrade insights\n");
            for i in failing {
                let _ = writeln!(md, "### {} — {}\n", md_code(&i.name), i.insight_status.status);
                if !i.insight_status.reason.is_empty() {
                    let _ = writeln!(md, "{}\n", md_code(&i.insight_status.reason));
                }
                if let Some(r) = &i.recommendation {
                    let _ = writeln!(md, "Recommendation: {}\n", md_code(r));
                }
                for dep in i.category_specific_summary.iter().flat_map(|s| &s.deprecation_details) {
                    let replaced = dep.replaced_with.as_deref().map(|r| format!(" → {}", md_code(r))).unwrap_or_default();
                    let stop = dep.stop_serving_version.as_deref().map(|v| format!(" (removed in {v})")).unwrap_or_default();
                    let _ = writeln!(md, "- {}{replaced}{stop}", md_code(&dep.usage));
                    for c in &dep.client_stats {
                        let _ = writeln!(md, "  - called by {} ({} requests in 30 days)", md_code(&c.user_agent), c.number_of_requests_last30_days);
                    }
                }
                for r in i.resources.iter().filter_map(|r| r.kubernetes_resource_uri.as_deref().or(r.arn.as_deref())) {
                    let _ = writeln!(md, "- resource {}", md_code(r));
                }
                let _ = writeln!(md);
            }
        }
    }
    let _ = writeln!(md, "\n## Inside the cluster\n");
    match inside {
        None => {
            let _ = writeln!(md, "Not read.");
        }
        Some(Err(why)) => {
            let _ = writeln!(md, "Could not be read:\n");
            md_block(&mut md, "text", &why);
        }
        Some(Ok(f)) => f.markdown(&mut md),
    }
    md
}

#[cfg(test)]
mod tests {
    use super::*;

    fn versions(now: f64) -> Vec<ClusterVersion> {
        vec![
            ClusterVersion {
                cluster_version: "1.31".into(),
                end_of_standard_support_date: Some(now - 100.0 * DAY),
                end_of_extended_support_date: Some(now + 20.0 * DAY),
                version_status: "EXTENDED_SUPPORT".into(),
                ..Default::default()
            },
            ClusterVersion {
                cluster_version: "1.33".into(),
                end_of_standard_support_date: Some(now + 45.0 * DAY),
                end_of_extended_support_date: Some(now + 410.0 * DAY),
                ..Default::default()
            },
        ]
    }

    #[test]
    fn support_from_status_or_dates() {
        let now = 1_800_000_000.0;
        let v = versions(now);
        assert!(Support::of("1.31", &v, now).extended());
        let s = Support::of("1.33", &v, now);
        assert_eq!(s.standard_days_left(now).map(f64::round), Some(45.0));
        assert!(s.attention(now));
        assert!(!Support::of("1.33", &v, now - 100.0 * DAY).attention(now - 100.0 * DAY));
        assert_eq!(Support::of("1.20", &v, now), Support::Unknown);
        let c = Cluster { upgrade_policy: Some(UpgradePolicy { support_type: "STANDARD".into() }), ..Default::default() };
        assert!(s.describe(&c, now).contains("then AWS upgrades it"));
        assert!(s.describe(&Cluster::default(), now).contains("extended support"));
    }

    #[test]
    fn versions_compare_by_their_numbers() {
        assert!(version_key("v1.19.2-eksbuild.1") > version_key("v1.18.3-eksbuild.2"));
        assert!(version_key("v1.18.10-eksbuild.1") > version_key("v1.18.9-eksbuild.3"));
    }

    #[test]
    fn kubeconfig_runs_dcli_for_tokens() {
        let c = Cluster {
            name: "shop".into(),
            arn: "arn:aws:eks:us-east-1:111122223333:cluster/shop".into(),
            endpoint: Some("https://ABC.gr7.us-east-1.eks.amazonaws.com".into()),
            certificate_authority: Some(Authority { data: "LS0tQ0E=".into() }),
            ..Default::default()
        };
        let args = vec!["eks".to_string(), "prod".into(), "shop".into(), "--token".into()];
        let text = kubeconfig(&c, "prod-shop", "/usr/local/bin/dcli", &args);
        assert!(text.contains("server: \"https://ABC.gr7.us-east-1.eks.amazonaws.com\""));
        assert!(text.contains("certificate-authority-data: \"LS0tQ0E=\""));
        assert!(text.contains("args: [\"eks\", \"prod\", \"shop\", \"--token\"]"));
        assert!(text.contains("current-context: \"prod-shop\""));
        // The config file goes in absolute: on Windows that adds the drive.
        let config = std::path::absolute("/tmp/c.yaml").unwrap().to_string_lossy().into_owned();
        let (_, args) = token_command(Some(std::path::Path::new("/tmp/c.yaml")), "prod", "shop");
        assert_eq!(args, ["--config", config.as_str(), "eks", "prod", "shop", "--token"]);
    }

    #[test]
    fn exec_credential_expires_in_fourteen_minutes() {
        let v = exec_credential("k8s-aws-v1.abc", 1_700_000_000.0);
        assert_eq!(v["status"]["token"], "k8s-aws-v1.abc");
        assert_eq!(v["status"]["expirationTimestamp"], crate::util::rfc3339(1_700_000_000 + 840));
        assert_eq!(base64url(b"\xfb\xff"), "-_8");
    }
}
