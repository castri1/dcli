//! What an environment has, by name, for the `:` palette: pipelines, Lambda functions, log
//! groups, parameters, secrets, CloudFormation stacks, buckets, state machines, queues,
//! tables, API Gateway APIs, Aurora clusters, event buses, AppConfig applications and
//! alarms, each with the module that opens it. Services are listed in parallel and capped; a service the user cannot read is left out.

use crate::aws::{self, Aws, alarms, appconfig, cloudformation, dynamodb, events, lambda, logs, s3, secrets, sfn, sqs, ssm};
use crate::util::show_invisible;

/// Resources listed at most per service (some accounts have thousands of log groups).
const MAX: usize = 2000;

#[derive(Debug, Clone)]
pub struct Resource {
    /// The title of the module that opens it.
    pub module: &'static str,
    pub name: String,
    /// What that module's `open` takes.
    pub target: String,
    pub detail: String,
}

impl Resource {
    fn new(module: &'static str, name: &str, target: impl Into<String>, what: &str) -> Resource {
        let detail = if what.is_empty() { module.to_string() } else { format!("{module} · {what}") };
        Resource { module, name: name.to_string(), target: target.into(), detail }
    }
}

type Lister = fn(&Aws) -> aws::Result<Vec<Resource>>;

const LISTERS: &[Lister] = &[
    functions,
    apis,
    log_groups,
    parameters,
    secret_list,
    stacks,
    buckets,
    state_machines,
    queues,
    tables,
    db_clusters,
    buses,
    applications,
    alarm_list,
];

/// The resources of the environment `aws` points at, `pipelines` being those in its config,
/// and how many services could be listed (none: the session has probably expired).
pub fn list(aws: &Aws, pipelines: &[String]) -> (Vec<Resource>, usize) {
    let mut all: Vec<Resource> = pipelines.iter().map(|p| Resource::new("Pipelines", p, p.clone(), "pipeline")).collect();
    let found: Vec<Option<Vec<Resource>>> = std::thread::scope(|s| {
        let jobs: Vec<_> = LISTERS.iter().map(|list| s.spawn(move || list(aws).ok())).collect();
        jobs.into_iter().map(|j| j.join().ok().flatten()).collect()
    });
    let listed = found.iter().filter(|f| f.is_some()).count();
    all.extend(found.into_iter().flatten().flatten());
    (all, listed)
}

/// Follows a paginated listing until its end or [`MAX`] items.
fn pages<T>(mut page: impl FnMut(Option<&str>) -> aws::Result<(Vec<T>, Option<String>)>) -> aws::Result<Vec<T>> {
    let (mut all, mut token) = (Vec::new(), None::<String>);
    loop {
        let (items, next) = page(token.as_deref())?;
        all.extend(items);
        match next {
            Some(t) if all.len() < MAX => token = Some(t),
            _ => return Ok(all),
        }
    }
}

fn functions(aws: &Aws) -> aws::Result<Vec<Resource>> {
    let list = pages(|t| lambda::list_functions(aws, t))?;
    Ok(list.iter().map(|f| Resource::new("Lambda", &f.function_name, f.function_name.clone(), f.runtime.as_deref().unwrap_or("function"))).collect())
}

/// API Gateway APIs (REST, HTTP and WebSocket); the target is the API id.
fn apis(_aws: &Aws) -> aws::Result<Vec<Resource>> {
    Ok(Vec::new())
}

fn log_groups(aws: &Aws) -> aws::Result<Vec<Resource>> {
    let list = pages(|t| logs::describe_log_groups(aws, None, t))?;
    Ok(list.iter().map(|g| Resource::new("Logs", &g.log_group_name, g.log_group_name.clone(), "log group")).collect())
}

fn parameters(aws: &Aws) -> aws::Result<Vec<Resource>> {
    let list = pages(|t| ssm::describe_parameters(aws, t))?;
    Ok(list.iter().map(|p| Resource::new("Parameters", &p.name, p.name.clone(), &p.kind)).collect())
}

fn secret_list(aws: &Aws) -> aws::Result<Vec<Resource>> {
    let list = pages(|t| secrets::list_secrets(aws, t))?;
    Ok(list.iter().map(|s| Resource::new("Secrets", &s.name, s.name.clone(), "secret")).collect())
}

fn stacks(aws: &Aws) -> aws::Result<Vec<Resource>> {
    let list = cloudformation::describe_stacks(aws, None)?;
    Ok(list.iter().map(|s| Resource::new("CloudFormation", &s.name, s.name.clone(), &s.status.to_lowercase())).collect())
}

fn buckets(aws: &Aws) -> aws::Result<Vec<Resource>> {
    Ok(s3::list_buckets(aws)?.iter().map(|b| Resource::new("S3", &b.name, format!("s3://{}/", b.name), "bucket")).collect())
}

fn state_machines(aws: &Aws) -> aws::Result<Vec<Resource>> {
    let list = sfn::all_state_machines(aws)?;
    Ok(list.iter().map(|m| Resource::new("Step Functions", &m.name, m.state_machine_arn.clone(), &m.kind.to_lowercase())).collect())
}

fn queues(aws: &Aws) -> aws::Result<Vec<Resource>> {
    let urls = sqs::list_queue_urls(aws)?;
    Ok(urls.iter().map(|u| Resource::new("SQS", sqs::queue_name(u), u.clone(), "queue")).collect())
}

fn tables(aws: &Aws) -> aws::Result<Vec<Resource>> {
    Ok(pages(|t| dynamodb::list_tables(aws, t))?.iter().map(|t| Resource::new("DynamoDB", t, t.clone(), "table")).collect())
}

/// Aurora clusters; the target is the cluster identifier.
fn db_clusters(aws: &Aws) -> aws::Result<Vec<Resource>> {
    let list = aws::rds::describe_clusters(aws, None)?;
    Ok(list.iter().map(|c| Resource::new("Aurora", &c.id, c.id.clone(), &c.engine_label())).collect())
}

fn buses(aws: &Aws) -> aws::Result<Vec<Resource>> {
    Ok(events::list_event_buses(aws)?.iter().map(|b| Resource::new("EventBridge", &b.name, b.name.clone(), "event bus")).collect())
}

fn alarm_list(aws: &Aws) -> aws::Result<Vec<Resource>> {
    // Alarm names are kept byte-exact for the API: shown with their invisible characters made visible.
    let list = alarms::describe_alarms(aws)?;
    Ok(list.iter().map(|a| Resource::new("Alarms", &show_invisible(&a.name), a.name.clone(), "alarm")).collect())
}

fn applications(aws: &Aws) -> aws::Result<Vec<Resource>> {
    let list = appconfig::list_applications(aws)?;
    Ok(list.iter().map(|a| Resource::new("AppConfig", &a.name, a.id.clone(), "application")).collect())
}
