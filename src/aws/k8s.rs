//! The Kubernetes API of an EKS cluster, read lightly to show what is broken: deployments,
//! pods that are not well, an object's warning events and a container's logs. Never writes,
//! and never reads pod specs' environment values. Requests carry an EKS token
//! ([`super::eks::token`]) and trust only the cluster's own certificate authority.

use std::fmt::Write as _;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use ureq::Agent;
use ureq::tls::{PemItem, RootCerts, TlsConfig, parse_pem};

use super::eks::{self, Cluster};
use super::{Aws, AwsError, Result};
use crate::util::{fmt_ago, md_block, md_code, now_secs, parse_rfc3339, sanitize_json};

/// Items listed at most per kind (pods, deployments).
const MAX_ITEMS: usize = 10_000;
const PAGE: &str = "500";
/// Tokens are made again after this (they are accepted for 15 minutes).
const TOKEN_REUSE: Duration = Duration::from_secs(10 * 60);
/// Waiting reasons that mean a container will not start by itself.
const BAD_WAITING: &[&str] = &[
    "CrashLoopBackOff",
    "ImagePullBackOff",
    "ErrImagePull",
    "ErrImageNeverPull",
    "InvalidImageName",
    "CreateContainerConfigError",
    "CreateContainerError",
    "RunContainerError",
    "PreCreateHookError",
    "PostStartHookError",
];
/// A pod still pending, or running without being ready, for this long is reported.
const SLOW_SECS: f64 = 300.0;

pub struct Client {
    base: String,
    cluster: String,
    aws: Arc<Aws>,
    agent: Agent,
    token: Mutex<Option<(String, Instant)>>,
}

impl Client {
    /// A client for `cluster`'s API endpoint. Plain `http` is accepted only with
    /// `AWS_ENDPOINT_URL` set (a local test server).
    pub fn new(aws: Arc<Aws>, cluster: &Cluster) -> Result<Client> {
        let endpoint = cluster.endpoint.as_deref().filter(|e| !e.is_empty()).ok_or_else(|| AwsError::Config(format!("{} has no API endpoint yet ({})", cluster.name, cluster.status)))?;
        let testing = std::env::var("AWS_ENDPOINT_URL").is_ok_and(|u| !u.is_empty());
        let mut config = Agent::config_builder()
            .http_status_as_error(false)
            .max_redirects(0)
            .timeout_global(Some(Duration::from_secs(30)))
            .timeout_connect(Some(Duration::from_secs(8)))
            .user_agent(concat!("dcli/", env!("CARGO_PKG_VERSION")));
        if endpoint.starts_with("https://") {
            let pem = cluster.ca_pem().ok_or_else(|| AwsError::Config(format!("{} has no certificate authority", cluster.name)))?;
            let certs: Vec<_> = parse_pem(&pem)
                .filter_map(|item| match item {
                    Ok(PemItem::Certificate(c)) => Some(c),
                    _ => None,
                })
                .collect();
            if certs.is_empty() {
                return Err(AwsError::Config(format!("{}'s certificate authority is not a PEM certificate", cluster.name)));
            }
            config = config.tls_config(TlsConfig::builder().root_certs(RootCerts::new_with_certs(&certs)).build());
        } else if !(testing && endpoint.starts_with("http://")) {
            return Err(AwsError::Config(format!("{}'s endpoint is not https: {endpoint}", cluster.name)));
        }
        Ok(Client {
            base: endpoint.trim_end_matches('/').to_string(),
            cluster: cluster.name.clone(),
            aws,
            agent: Agent::new_with_config(config.build()),
            token: Mutex::new(None),
        })
    }

    fn token(&self) -> Result<String> {
        let mut slot = self.token.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((t, _)) = slot.as_ref().filter(|(_, at)| at.elapsed() < TOKEN_REUSE) {
            return Ok(t.clone());
        }
        let t = eks::token(&self.aws, &self.cluster)?;
        *slot = Some((t.clone(), Instant::now()));
        Ok(t)
    }

    /// GET `path` with `query`; the body of a 2xx answer.
    fn get(&self, path: &str, query: &[(&str, &str)]) -> Result<Vec<u8>> {
        let query: Vec<(String, String)> = query.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        let qs = super::sigv4::canonical_query(&query);
        let url = if qs.is_empty() { format!("{}{path}", self.base) } else { format!("{}{path}?{qs}", self.base) };
        let request = ureq::http::Request::builder()
            .method("GET")
            .uri(&url)
            .header("authorization", format!("Bearer {}", self.token()?))
            .header("accept", "application/json, */*")
            .body(())
            .map_err(|e| AwsError::Network(e.to_string()))?;
        let mut response = self.agent.run(request).map_err(|e| AwsError::Network(e.to_string()))?;
        let status = response.status().as_u16();
        let body = response.body_mut().with_config().limit(256 * 1024 * 1024).read_to_vec().map_err(|e| AwsError::Network(e.to_string()))?;
        if (200..300).contains(&status) {
            return Ok(body);
        }
        // A Kubernetes Status: {"reason": "Forbidden", "message": "pods is forbidden: …"}.
        let v: Value = serde_json::from_slice(&body).unwrap_or_default();
        let field = |k: &str| v.get(k).and_then(Value::as_str).map(crate::util::sanitize);
        let code = field("reason").filter(|r| !r.is_empty()).unwrap_or_else(|| format!("HTTP{status}"));
        let message = field("message").unwrap_or_else(|| crate::util::sanitize(String::from_utf8_lossy(&body).trim()));
        Err(AwsError::Api { status, code, message })
    }

    fn json<T: DeserializeOwned>(&self, path: &str, query: &[(&str, &str)]) -> Result<T> {
        let bytes = self.get(path, query)?;
        let mut v: Value = serde_json::from_slice(&bytes).map_err(|e| AwsError::Network(format!("unexpected answer from the cluster: {e}")))?;
        sanitize_json(&mut v);
        T::deserialize(v).map_err(|e| AwsError::Network(format!("unexpected answer from the cluster: {e}")))
    }

    /// Every item of a list, following `continue`.
    fn list<T: DeserializeOwned>(&self, path: &str, extra: &[(&str, &str)]) -> Result<(Vec<T>, bool)> {
        #[derive(Deserialize)]
        #[serde(bound = "T: DeserializeOwned")]
        struct List<T> {
            #[serde(default)]
            items: Vec<T>,
            #[serde(default)]
            metadata: ListMeta,
        }
        #[derive(Deserialize, Default)]
        #[serde(default)]
        struct ListMeta {
            #[serde(rename = "continue")]
            next: Option<String>,
        }
        let (mut all, mut next) = (Vec::new(), None::<String>);
        loop {
            let mut query: Vec<(&str, &str)> = vec![("limit", PAGE)];
            query.extend_from_slice(extra);
            if let Some(n) = next.as_deref() {
                query.push(("continue", n));
            }
            let page: List<T> = self.json(path, &query)?;
            all.extend(page.items);
            match page.metadata.next.filter(|n| !n.is_empty()) {
                Some(n) if all.len() < MAX_ITEMS => next = Some(n),
                Some(_) => return Ok((all, true)),
                None => return Ok((all, false)),
            }
        }
    }

    pub fn namespaces(&self) -> Result<Vec<String>> {
        let (items, _) = self.list::<Object>("/api/v1/namespaces", &[])?;
        Ok(items.into_iter().map(|o| o.metadata.name).collect())
    }

    pub fn deployments(&self) -> Result<(Vec<Deployment>, bool)> {
        let (items, more) = self.list::<RawDeployment>("/apis/apps/v1/deployments", &[])?;
        Ok((items.into_iter().map(Deployment::from).collect(), more))
    }

    pub fn pods(&self) -> Result<(Vec<Pod>, bool)> {
        let (items, more) = self.list::<RawPod>("/api/v1/pods", &[])?;
        Ok((items.into_iter().map(Pod::from).collect(), more))
    }

    /// The warning events of object `name` in `namespace`, newest first.
    pub fn events(&self, namespace: &str, name: &str) -> Result<Vec<Event>> {
        let selector = format!("involvedObject.name={name},type=Warning");
        let path = format!("/api/v1/namespaces/{}/events", crate::util::uri_encode(namespace));
        let (items, _) = self.list::<RawEvent>(&path, &[("fieldSelector", &selector)])?;
        let mut events: Vec<Event> = items.into_iter().map(Event::from).collect();
        events.sort_by(|a, b| b.last.unwrap_or(0.0).total_cmp(&a.last.unwrap_or(0.0)));
        Ok(events)
    }

    /// The last `tail` lines a container wrote; `previous`: those of the run before (the one
    /// that crashed).
    pub fn logs(&self, namespace: &str, pod: &str, container: &str, previous: bool, tail: usize) -> Result<String> {
        let path = format!("/api/v1/namespaces/{}/pods/{}/log", crate::util::uri_encode(namespace), crate::util::uri_encode(pod));
        let tail = tail.to_string();
        let mut query = vec![("container", container), ("tailLines", tail.as_str())];
        if previous {
            query.push(("previous", "true"));
        }
        let bytes = self.get(&path, &query)?;
        Ok(crate::util::sanitize(&String::from_utf8_lossy(&bytes)))
    }
}

// ---- objects ----------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct Meta {
    name: String,
    namespace: String,
    creation_timestamp: Option<String>,
    owner_references: Vec<Owner>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
struct Owner {
    kind: String,
    name: String,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Object {
    metadata: Meta,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Condition {
    #[serde(rename = "type")]
    pub kind: String,
    pub status: String,
    pub reason: Option<String>,
    pub message: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct RawDeployment {
    metadata: Meta,
    spec: DeploymentSpec,
    status: DeploymentStatus,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct DeploymentSpec {
    replicas: Option<i64>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
struct DeploymentStatus {
    ready_replicas: i64,
    updated_replicas: i64,
    conditions: Vec<Condition>,
}

#[derive(Debug, Clone, Default)]
pub struct Deployment {
    pub namespace: String,
    pub name: String,
    pub desired: i64,
    pub ready: i64,
    pub updated: i64,
    pub conditions: Vec<Condition>,
}

impl From<RawDeployment> for Deployment {
    fn from(d: RawDeployment) -> Deployment {
        Deployment {
            namespace: d.metadata.namespace,
            name: d.metadata.name,
            desired: d.spec.replicas.unwrap_or(1),
            ready: d.status.ready_replicas,
            updated: d.status.updated_replicas,
            conditions: d.status.conditions,
        }
    }
}

impl Deployment {
    /// What is wrong with it, if anything: a rollout stuck, replicas missing.
    pub fn problem(&self) -> Option<String> {
        // A rollout that timed out says more than the missing availability it causes.
        let failed = |kind: &str| self.conditions.iter().find(|c| c.kind == kind && c.status == "False");
        let stuck = failed("Progressing").or_else(|| failed("Available"));
        if let Some(c) = stuck {
            let reason = c.reason.clone().unwrap_or_else(|| format!("{} false", c.kind));
            return Some(match c.message.as_deref().filter(|m| !m.is_empty()) {
                Some(m) => format!("{reason}: {m}"),
                None => reason,
            });
        }
        (self.ready < self.desired).then(|| format!("{} of {} replicas ready", self.ready, self.desired))
    }

    /// Its pods: owned by one of its ReplicaSets (`<deployment>-<hash>`).
    pub fn owns(&self, pod: &Pod) -> bool {
        pod.namespace == self.namespace
            && pod.owner.as_ref().is_some_and(|(kind, name)| {
                kind == "ReplicaSet" && name.strip_prefix(&self.name).and_then(|r| r.strip_prefix('-')).is_some_and(|h| !h.contains('-'))
            })
    }
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct RawPod {
    metadata: Meta,
    spec: PodSpec,
    status: PodStatus,
}

/// Only what dcli shows: never the containers' environment.
#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
struct PodSpec {
    node_name: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
struct PodStatus {
    phase: String,
    reason: Option<String>,
    message: Option<String>,
    conditions: Vec<Condition>,
    container_statuses: Vec<RawContainer>,
    init_container_statuses: Vec<RawContainer>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
struct RawContainer {
    name: String,
    ready: bool,
    restart_count: i64,
    image: String,
    state: RawState,
    last_state: RawState,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
struct RawState {
    waiting: Option<Why>,
    running: Option<Value>,
    terminated: Option<Ended>,
}

#[derive(Deserialize, Default, Clone, Debug)]
#[serde(default)]
pub struct Why {
    pub reason: String,
    pub message: Option<String>,
}

#[derive(Deserialize, Default, Clone, Debug)]
#[serde(rename_all = "camelCase", default)]
pub struct Ended {
    pub reason: Option<String>,
    pub exit_code: i64,
    pub message: Option<String>,
    pub finished_at: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct Container {
    pub name: String,
    pub init: bool,
    pub ready: bool,
    pub running: bool,
    pub restarts: i64,
    pub image: String,
    pub waiting: Option<Why>,
    pub terminated: Option<Ended>,
    pub last_terminated: Option<Ended>,
}

#[derive(Debug, Clone, Default)]
pub struct Pod {
    pub namespace: String,
    pub name: String,
    pub phase: String,
    pub reason: Option<String>,
    pub message: Option<String>,
    pub node: Option<String>,
    /// (kind, name) of its controller.
    pub owner: Option<(String, String)>,
    pub created: Option<f64>,
    pub conditions: Vec<Condition>,
    pub containers: Vec<Container>,
}

fn container(c: RawContainer, init: bool) -> Container {
    Container {
        name: c.name,
        init,
        ready: c.ready,
        running: c.state.running.is_some(),
        restarts: c.restart_count,
        image: c.image,
        waiting: c.state.waiting,
        terminated: c.state.terminated,
        last_terminated: c.last_state.terminated,
    }
}

impl From<RawPod> for Pod {
    fn from(p: RawPod) -> Pod {
        let mut containers: Vec<Container> = p.status.init_container_statuses.into_iter().map(|c| container(c, true)).collect();
        containers.extend(p.status.container_statuses.into_iter().map(|c| container(c, false)));
        Pod {
            owner: p.metadata.owner_references.into_iter().next().map(|o| (o.kind, o.name)),
            created: p.metadata.creation_timestamp.as_deref().and_then(parse_rfc3339),
            namespace: p.metadata.namespace,
            name: p.metadata.name,
            phase: p.status.phase,
            reason: p.status.reason,
            message: p.status.message,
            node: p.spec.node_name,
            conditions: p.status.conditions,
            containers,
        }
    }
}

/// What is wrong with a pod, in a few words, with the container to read logs from.
#[derive(Debug, Clone, PartialEq)]
pub struct Problem {
    /// `CrashLoopBackOff`, `OOMKilled`, `Unschedulable`…
    pub reason: String,
    pub detail: String,
    pub container: Option<String>,
    /// Its logs worth reading are those of the run that ended (it crashed and restarts).
    pub previous: bool,
}

fn ended(e: &Ended) -> String {
    let reason = e.reason.clone().unwrap_or_else(|| "Terminated".into());
    match e.message.as_deref().map(str::trim).filter(|m| !m.is_empty()) {
        Some(m) => format!("{reason} (exit {}): {m}", e.exit_code),
        None => format!("{reason} (exit {})", e.exit_code),
    }
}

impl Pod {
    pub fn restarts(&self) -> i64 {
        self.containers.iter().map(|c| c.restarts).sum()
    }

    fn age(&self, now: f64) -> f64 {
        self.created.map_or(0.0, |c| now - c)
    }

    /// What is wrong with it at `now`, if anything. Completed pods are fine; pods pending or
    /// not ready for a few minutes are not.
    pub fn problem(&self, now: f64) -> Option<Problem> {
        if self.phase == "Succeeded" {
            return None;
        }
        let first_app = self.containers.iter().find(|c| !c.init).map(|c| c.name.clone());
        // A container that will not start by itself.
        if let Some(c) = self.containers.iter().find(|c| c.waiting.as_ref().is_some_and(|w| BAD_WAITING.contains(&w.reason.as_str()))) {
            let w = c.waiting.as_ref().map(|w| w.reason.clone()).unwrap_or_default();
            let mut detail = c.waiting.as_ref().and_then(|w| w.message.clone()).unwrap_or_default();
            if let Some(last) = &c.last_terminated {
                detail = if detail.is_empty() { format!("last run: {}", ended(last)) } else { format!("last run: {} · {detail}", ended(last)) };
            }
            // An OOM kill says more than the back-off that follows it.
            let reason = match &c.last_terminated {
                Some(l) if w == "CrashLoopBackOff" && l.reason.as_deref() == Some("OOMKilled") => "OOMKilled".to_string(),
                _ => w,
            };
            let previous = c.restarts > 0 && c.last_terminated.is_some();
            // A container that never ran (its image not pulled, its config wrong) has no logs.
            let ran = previous || c.waiting.as_ref().is_some_and(|w| w.reason == "CrashLoopBackOff");
            return Some(Problem { reason, detail, container: ran.then(|| c.name.clone()), previous });
        }
        if self.phase == "Failed" {
            let c = self.containers.iter().find(|c| c.terminated.as_ref().is_some_and(|t| t.exit_code != 0));
            let reason = self.reason.clone().or_else(|| c.and_then(|c| c.terminated.as_ref()?.reason.clone())).unwrap_or_else(|| "Failed".into());
            let detail = self.message.clone().or_else(|| c.and_then(|c| c.terminated.as_ref().map(ended))).unwrap_or_default();
            return Some(Problem { reason, detail, container: c.map(|c| c.name.clone()).or(first_app), previous: false });
        }
        if self.phase == "Pending" {
            if let Some(cond) = self.conditions.iter().find(|c| c.kind == "PodScheduled" && c.status == "False") {
                return Some(Problem {
                    reason: cond.reason.clone().unwrap_or_else(|| "Unschedulable".into()),
                    detail: cond.message.clone().unwrap_or_default(),
                    container: None,
                    previous: false,
                });
            }
            if self.age(now) >= SLOW_SECS {
                let w = self.containers.iter().find_map(|c| c.waiting.clone());
                return Some(Problem {
                    reason: w.as_ref().map(|w| w.reason.clone()).unwrap_or_else(|| "Pending".into()),
                    detail: w.and_then(|w| w.message).unwrap_or_else(|| format!("pending for {}", fmt_ago(self.created.unwrap_or(now)))),
                    container: first_app,
                    previous: false,
                });
            }
            return None;
        }
        // Running, restarted after an out-of-memory kill.
        if let Some(c) = self.containers.iter().find(|c| c.restarts > 0 && c.last_terminated.as_ref().is_some_and(|t| t.reason.as_deref() == Some("OOMKilled"))) {
            return Some(Problem {
                reason: "OOMKilled".into(),
                detail: format!("restarted {} time{} after running out of memory", c.restarts, if c.restarts == 1 { "" } else { "s" }),
                container: Some(c.name.clone()),
                previous: true,
            });
        }
        if self.phase == "Running" && self.age(now) >= SLOW_SECS
            && let Some(c) = self.containers.iter().find(|c| !c.init && !c.ready)
        {
            let (reason, detail) = match &c.waiting {
                Some(w) if !c.running => (w.reason.clone(), w.message.clone().unwrap_or_else(|| format!("container {} is not running", c.name))),
                _ => ("NotReady".into(), format!("container {} is running but not ready (readiness probe)", c.name)),
            };
            return Some(Problem { reason, detail, container: Some(c.name.clone()), previous: false });
        }
        if !matches!(self.phase.as_str(), "Running" | "Pending") {
            return Some(Problem { reason: self.phase.clone(), detail: self.message.clone().unwrap_or_default(), container: first_app, previous: false });
        }
        None
    }
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
struct RawEvent {
    reason: String,
    message: String,
    count: Option<i64>,
    last_timestamp: Option<String>,
    event_time: Option<String>,
    first_timestamp: Option<String>,
    series: Option<Series>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
struct Series {
    count: i64,
    last_observed_time: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct Event {
    pub reason: String,
    pub message: String,
    pub count: i64,
    pub last: Option<f64>,
}

impl From<RawEvent> for Event {
    fn from(e: RawEvent) -> Event {
        let when = e
            .series
            .as_ref()
            .and_then(|s| s.last_observed_time.clone())
            .or(e.last_timestamp)
            .or(e.event_time)
            .or(e.first_timestamp);
        Event {
            reason: e.reason,
            message: e.message,
            count: e.series.as_ref().map(|s| s.count).or(e.count).unwrap_or(1).max(1),
            last: when.as_deref().and_then(parse_rfc3339),
        }
    }
}

impl Event {
    /// `BackOff ×12 (3m ago): Back-off restarting failed container…`
    pub fn line(&self) -> String {
        let times = if self.count > 1 { format!(" ×{}", self.count) } else { String::new() };
        let when = self.last.map(|t| format!(" ({})", fmt_ago(t))).unwrap_or_default();
        format!("{}{times}{when}: {}", self.reason, self.message.trim())
    }
}

// ---- what is broken -------------------------------------------------------------------------

/// The cluster's workloads, as far as dcli looks: every deployment, and the pods with a
/// problem (all of them are counted).
#[derive(Debug, Clone, Default)]
pub struct Workloads {
    pub namespaces: usize,
    pub deployments: Vec<Deployment>,
    pub pods: usize,
    /// Pods with a problem, with it.
    pub failing: Vec<(Pod, Problem)>,
    /// More items than dcli lists.
    pub truncated: bool,
}

impl Workloads {
    pub fn unhealthy(&self) -> Vec<&Deployment> {
        self.deployments.iter().filter(|d| d.problem().is_some()).collect()
    }
}

pub fn workloads(client: &Client) -> Result<Workloads> {
    let (namespaces, deployments, pods) = std::thread::scope(|s| {
        let n = s.spawn(|| client.namespaces());
        let d = s.spawn(|| client.deployments());
        let p = s.spawn(|| client.pods());
        let lost = || AwsError::Network("thread failed".into());
        (n.join().unwrap_or_else(|_| Err(lost())), d.join().unwrap_or_else(|_| Err(lost())), p.join().unwrap_or_else(|_| Err(lost())))
    });
    let (mut deployments, more_d) = deployments?;
    let (pods, more_p) = pods?;
    let now = now_secs();
    let total = pods.len();
    let mut failing: Vec<(Pod, Problem)> = pods.into_iter().filter_map(|p| p.problem(now).map(|why| (p, why))).collect();
    failing.sort_by(|a, b| (&a.0.namespace, &a.0.name).cmp(&(&b.0.namespace, &b.0.name)));
    deployments.sort_by(|a, b| (a.problem().is_none(), &a.namespace, &a.name).cmp(&(b.problem().is_none(), &b.namespace, &b.name)));
    // Namespaces are a count; without permission to list them, the pods' own say enough.
    let namespaces = namespaces.map(|n| n.len()).unwrap_or_else(|_| {
        let mut seen: Vec<&str> = failing.iter().map(|(p, _)| p.namespace.as_str()).collect();
        seen.sort_unstable();
        seen.dedup();
        seen.len()
    });
    Ok(Workloads { namespaces, deployments, pods: total, failing, truncated: more_d || more_p })
}

/// A failing pod with its warning events and the logs worth reading.
#[derive(Debug, Clone)]
pub struct PodReport {
    pub pod: Pod,
    pub problem: Problem,
    pub events: std::result::Result<Vec<Event>, String>,
    /// (container, previous run, last lines), or why they could not be read.
    pub logs: Option<std::result::Result<(String, bool, String), String>>,
}

pub fn pod_report(client: &Client, pod: &Pod, problem: &Problem, tail: usize) -> PodReport {
    let events = client.events(&pod.namespace, &pod.name).map_err(|e| e.short());
    let logs = problem.container.as_ref().map(|c| {
        client.logs(&pod.namespace, &pod.name, c, problem.previous, tail).map(|text| (c.clone(), problem.previous, text)).map_err(|e| e.short())
    });
    PodReport { pod: pod.clone(), problem: problem.clone(), events, logs }
}

/// What the report says about the inside of a cluster.
#[derive(Debug, Clone)]
pub struct Findings {
    pub workloads: Workloads,
    pub pods: Vec<PodReport>,
}

/// The workloads and, for the first `max_pods` failing pods, their events and last `tail`
/// log lines.
pub fn findings(client: &Client, max_pods: usize, tail: usize) -> Result<Findings> {
    let workloads = workloads(client)?;
    let chosen: Vec<&(Pod, Problem)> = workloads.failing.iter().take(max_pods).collect();
    let pods = std::thread::scope(|s| {
        let jobs: Vec<_> = chosen.iter().map(|(p, why)| s.spawn(move || pod_report(client, p, why, tail))).collect();
        jobs.into_iter().filter_map(|j| j.join().ok()).collect()
    });
    Ok(Findings { workloads, pods })
}

impl Findings {
    pub fn markdown(&self, md: &mut String) {
        let w = &self.workloads;
        let unhealthy = w.unhealthy();
        let _ = writeln!(
            md,
            "{} namespaces · {} deployments ({} not healthy) · {} pods ({} with a problem){}\n",
            w.namespaces,
            w.deployments.len(),
            unhealthy.len(),
            w.pods,
            w.failing.len(),
            if w.truncated { " · more than dcli lists" } else { "" }
        );
        if !unhealthy.is_empty() {
            let _ = writeln!(md, "### Deployments not healthy\n");
            for d in unhealthy {
                let _ = writeln!(
                    md,
                    "- {} — {}/{} ready: {}",
                    md_code(&format!("{}/{}", d.namespace, d.name)),
                    d.ready,
                    d.desired,
                    md_code(&d.problem().unwrap_or_default())
                );
            }
            let _ = writeln!(md);
        }
        if !w.failing.is_empty() {
            let _ = writeln!(md, "### Pods with a problem\n");
            for (p, why) in &w.failing {
                let _ = writeln!(md, "- {} — {} · {} restarts: {}", md_code(&format!("{}/{}", p.namespace, p.name)), md_code(&why.reason), p.restarts(), md_code(&why.detail));
            }
            let _ = writeln!(md);
        }
        for r in &self.pods {
            let _ = writeln!(md, "### {} ({})\n", md_code(&format!("{}/{}", r.pod.namespace, r.pod.name)), md_code(&r.problem.reason));
            if let Some(node) = &r.pod.node {
                let _ = writeln!(md, "Node: {}\n", md_code(node));
            }
            for c in r.pod.containers.iter().filter(|c| !c.image.is_empty()) {
                let _ = writeln!(md, "- container {} image {}", md_code(&c.name), md_code(&c.image));
            }
            match &r.events {
                Ok(e) if e.is_empty() => {}
                Ok(e) => {
                    let _ = writeln!(md, "\nWarning events:\n");
                    let text: Vec<String> = e.iter().take(15).map(Event::line).collect();
                    md_block(md, "text", &text.join("\n"));
                }
                Err(e) => {
                    let _ = writeln!(md, "\nEvents could not be read: {}", md_code(e));
                }
            }
            match &r.logs {
                Some(Ok((c, previous, text))) => {
                    let which = if *previous { " (the run that ended)" } else { "" };
                    let _ = writeln!(md, "\nLast lines of {}{which}:\n", md_code(c));
                    md_block(md, "text", if text.trim().is_empty() { "(empty)" } else { text });
                }
                Some(Err(e)) => {
                    let _ = writeln!(md, "\nLogs could not be read: {}", md_code(e));
                }
                None => {}
            }
            let _ = writeln!(md);
        }
    }
}

/// Why the inside of `cluster` could not be read, and what to do: `principal` is who dcli
/// signs in as (see [`super::sts::caller_principal`]).
pub fn explain(e: &AwsError, cluster: &Cluster, principal: Option<&str>) -> String {
    let who = principal.unwrap_or("this profile's IAM identity");
    let mode = cluster.auth_mode();
    match e {
        AwsError::Api { status: 401, .. } => {
            let entry = format!(
                "give it an access entry: aws eks create-access-entry --cluster-name {c} --principal-arn {who}, then aws eks associate-access-policy --cluster-name {c} --principal-arn {who} --policy-arn arn:aws:eks::aws:cluster-access-policy/AmazonEKSViewPolicy --access-scope type=cluster",
                c = cluster.name
            );
            let config_map = format!("map {who} in the aws-auth ConfigMap (kube-system) to a group that can read");
            let how = match mode {
                "CONFIG_MAP" => format!("The cluster uses only the aws-auth ConfigMap: {config_map}, or switch its authentication mode to API_AND_CONFIG_MAP and {entry}."),
                "API" => format!("{}{}.", entry[..1].to_uppercase(), &entry[1..]),
                _ => format!("{}{}; or {config_map}.", entry[..1].to_uppercase(), &entry[1..]),
            };
            format!("The cluster does not know {who} (401 Unauthorized). {how}")
        }
        AwsError::Api { status: 403, message, .. } => format!(
            "{who} signs in to the cluster but may not read this ({message}). Associate a policy that can read, like AmazonEKSViewPolicy with cluster scope, with its access entry, or bind its Kubernetes groups to a role that can list pods, deployments and events."
        ),
        AwsError::Network(detail) => {
            let v = &cluster.resources_vpc_config;
            if !v.endpoint_public_access {
                format!("The API endpoint of {} is private: it answers only inside its VPC (a VPN, a bastion or a peered network). ({detail})", cluster.name)
            } else if !v.public_access_cidrs.is_empty() && !v.public_access_cidrs.iter().any(|c| c == "0.0.0.0/0") {
                format!("The API endpoint of {} answers only from {}: is this machine's address one of them? ({detail})", cluster.name, v.public_access_cidrs.join(", "))
            } else {
                format!("Could not reach the API endpoint of {}: {detail}", cluster.name)
            }
        }
        other => other.short(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pod(json: Value) -> Pod {
        Pod::from(serde_json::from_value::<RawPod>(json).unwrap())
    }

    #[test]
    fn crash_loops_and_oom_kills_are_problems_with_the_previous_logs() {
        let now = 1_800_000_000.0;
        let p = pod(serde_json::json!({
            "metadata": {"name": "cart-7d9f8b6c5d-x2x9q", "namespace": "shop", "creationTimestamp": "2027-01-15T08:00:00Z",
                         "ownerReferences": [{"kind": "ReplicaSet", "name": "cart-7d9f8b6c5d"}]},
            "spec": {"nodeName": "ip-10-0-1-5", "containers": [{"name": "app", "env": [{"name": "DB_PASSWORD", "value": "hunter2"}]}]},
            "status": {"phase": "Running", "containerStatuses": [{"name": "app", "ready": false, "restartCount": 14, "image": "cart:2.4",
                "state": {"waiting": {"reason": "CrashLoopBackOff", "message": "back-off 5m0s"}},
                "lastState": {"terminated": {"reason": "Error", "exitCode": 1}}}]}
        }));
        let why = p.problem(now).unwrap();
        assert_eq!(why.reason, "CrashLoopBackOff");
        assert!(why.previous);
        assert!(why.detail.contains("Error (exit 1)"));
        assert!(!format!("{p:?}").contains("hunter2"));
        let d = Deployment { namespace: "shop".into(), name: "cart".into(), desired: 3, ready: 1, ..Default::default() };
        assert!(d.owns(&p));
        assert!(!Deployment { name: "car".into(), ..d.clone() }.owns(&p));
        assert_eq!(d.problem().as_deref(), Some("1 of 3 replicas ready"));

        let oom = pod(serde_json::json!({
            "metadata": {"name": "report", "namespace": "batch"},
            "status": {"phase": "Running", "containerStatuses": [{"name": "job", "ready": true, "restartCount": 3,
                "state": {"running": {}}, "lastState": {"terminated": {"reason": "OOMKilled", "exitCode": 137}}}]}
        }));
        assert_eq!(oom.problem(now).unwrap().reason, "OOMKilled");
    }

    #[test]
    fn pending_pods_say_why_and_completed_ones_are_fine() {
        let now = 1_800_000_000.0;
        let pending = pod(serde_json::json!({
            "metadata": {"name": "cart-z9", "namespace": "shop"},
            "status": {"phase": "Pending", "conditions": [{"type": "PodScheduled", "status": "False", "reason": "Unschedulable",
                "message": "0/6 nodes are available: 6 Insufficient memory."}]}
        }));
        let why = pending.problem(now).unwrap();
        assert_eq!(why.reason, "Unschedulable");
        assert!(why.container.is_none());
        let done = pod(serde_json::json!({"metadata": {"name": "job"}, "status": {"phase": "Succeeded"}}));
        assert!(done.problem(now).is_none());
    }

    #[test]
    fn access_problems_say_what_to_do() {
        let c = Cluster { name: "shop".into(), access_config: eks::AccessConfig { authentication_mode: Some("API".into()) }, ..Default::default() };
        let e = AwsError::Api { status: 401, code: "Unauthorized".into(), message: String::new() };
        let text = explain(&e, &c, Some("arn:aws:iam::1:role/Dev"));
        assert!(text.contains("does not know arn:aws:iam::1:role/Dev"));
        assert!(text.contains("create-access-entry --cluster-name shop"));
        let private = Cluster { name: "vpn".into(), ..Default::default() };
        assert!(explain(&AwsError::Network("timed out".into()), &private, None).contains("private"));
    }
}
