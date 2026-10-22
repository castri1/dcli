//! Redshift Data API (awsJson1_1, `X-Amz-Target: RedshiftData.<Operation>`): SQL on a cluster
//! without a driver (ExecuteStatement, DescribeStatement, GetStatementResult), used to read
//! system tables.
//!
//! Only SQL written by dcli runs here: fixed queries on system views, with nothing spliced in
//! but integers dcli checked itself (a process id to cancel). A statement runs as an
//! [`Identity`]: a database user with temporary credentials (the caller's IAM permissions,
//! `redshift:GetClusterCredentials`), or a Secrets Manager secret referenced by ARN; dcli never
//! sees a password.
//!
//! Redshift Serverless takes `WorkgroupName` where a provisioned cluster takes
//! `ClusterIdentifier` (see [`Target`]); the rest of the API is the same.

use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::{Aws, AwsError, Result, Service};
use crate::util::{sanitize, sanitize_json};

pub const SERVICE: Service = Service { endpoint: "redshift-data", signing_name: "redshift-data", target_prefix: "RedshiftData" };

/// Result pages read at most (each holds up to 100 MB; system queries return little).
const MAX_PAGES: usize = 10;
/// How long a statement may run before dcli cancels it.
pub const TIMEOUT: Duration = Duration::from_secs(30);

/// Where a statement runs. A Serverless workgroup is a `Workgroup(name)` variant away: it
/// sends `WorkgroupName` instead of `ClusterIdentifier`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    Cluster(String),
}

impl Target {
    fn params(&self) -> (&'static str, &str) {
        match self {
            Target::Cluster(id) => ("ClusterIdentifier", id),
        }
    }
}

/// Who a statement runs as, with which database. Saved per environment and cluster (see
/// `modules::redshift::identity`); holds no password.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Identity {
    pub database: String,
    /// Temporary credentials for this database user (`redshift:GetClusterCredentials`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub db_user: Option<String>,
    /// Or the user and password in this Secrets Manager secret.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret_arn: Option<String>,
}

impl Identity {
    /// `dev as admin (temporary credentials)`, `dev with the secret prod/analytics/redshift`.
    pub fn describe(&self) -> String {
        match (&self.db_user, &self.secret_arn) {
            (_, Some(arn)) => format!("{} with the secret {}", self.database, secret_name(arn)),
            (Some(user), None) => format!("{} as {user} (temporary credentials)", self.database),
            (None, None) => format!("{} (no user)", self.database),
        }
    }

    pub fn is_complete(&self) -> bool {
        !self.database.trim().is_empty() && (self.db_user.as_deref().is_some_and(|u| !u.trim().is_empty()) || self.secret_arn.is_some())
    }
}

// ---- saved identities --------------------------------------------------------------------

/// `~/.deployment-cli/redshift.json`: the identity used for each environment's clusters,
/// `{"<env>/<cluster>": {"database": …, "dbUser": …} | {"database": …, "secretArn": …}}`.
/// Kept out of config.yaml, which the Node version rewrites. Holds no password.
pub fn identities_path() -> Option<std::path::PathBuf> {
    Some(crate::util::home_dir()?.join(".deployment-cli").join("redshift.json"))
}

fn read_identities() -> std::collections::BTreeMap<String, Identity> {
    identities_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

/// The identity saved for `cluster` in `env`.
pub fn saved_identity(env: &str, cluster: &str) -> Option<Identity> {
    read_identities().remove(&format!("{env}/{cluster}")).filter(Identity::is_complete)
}

pub fn save_identity(env: &str, cluster: &str, identity: &Identity) -> std::io::Result<()> {
    let path = identities_path().ok_or_else(|| std::io::Error::other("cannot find the home directory"))?;
    let mut all = read_identities();
    all.insert(format!("{env}/{cluster}"), identity.clone());
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&path, serde_json::to_string_pretty(&all).unwrap_or_default() + "\n")
}

/// `prod/analytics/redshift` from its ARN (`…:secret:prod/analytics/redshift-AbCdEf`).
pub fn secret_name(arn: &str) -> &str {
    let name = arn.split(":secret:").nth(1).unwrap_or(arn);
    // Secrets Manager appends a dash and six characters to the name.
    match name.rsplit_once('-') {
        Some((base, suffix)) if suffix.len() == 6 && suffix.chars().all(|c| c.is_ascii_alphanumeric()) => base,
        _ => name,
    }
}

/// A value of a result row.
#[derive(Debug, Clone, PartialEq)]
pub enum Field {
    Null,
    Bool(bool),
    Long(i64),
    Double(f64),
    Text(String),
}

impl Field {
    fn from_json(v: &Value) -> Field {
        if v.get("isNull").and_then(Value::as_bool) == Some(true) {
            return Field::Null;
        }
        if let Some(b) = v.get("booleanValue").and_then(Value::as_bool) {
            return Field::Bool(b);
        }
        if let Some(n) = v.get("longValue").and_then(Value::as_i64) {
            return Field::Long(n);
        }
        if let Some(n) = v.get("doubleValue").and_then(Value::as_f64) {
            return Field::Double(n);
        }
        match v.get("stringValue").and_then(Value::as_str) {
            Some(s) => Field::Text(sanitize(s)),
            // Blobs and anything newer: not used by dcli's queries.
            None => Field::Null,
        }
    }

    pub fn text(&self) -> String {
        match self {
            Field::Null => String::new(),
            Field::Bool(b) => b.to_string(),
            Field::Long(n) => n.to_string(),
            Field::Double(n) => n.to_string(),
            Field::Text(s) => s.trim().to_string(),
        }
    }

    pub fn num(&self) -> Option<f64> {
        match self {
            Field::Long(n) => Some(*n as f64),
            Field::Double(n) => Some(*n),
            Field::Text(s) => s.trim().parse().ok(),
            Field::Bool(b) => Some(f64::from(u8::from(*b))),
            Field::Null => None,
        }
    }

    pub fn int(&self) -> Option<i64> {
        match self {
            Field::Long(n) => Some(*n),
            Field::Double(n) if n.fract() == 0.0 => Some(*n as i64),
            Field::Text(s) => s.trim().parse().ok(),
            _ => None,
        }
    }
}

/// A result: column names (lower case) and rows.
#[derive(Debug, Clone, Default)]
pub struct Rows {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Field>>,
}

impl Rows {
    fn column(&self, name: &str) -> Option<usize> {
        self.columns.iter().position(|c| c == name)
    }

    /// The field `name` of row `row` (`Null` when there is no such column).
    pub fn get(&self, row: usize, name: &str) -> &Field {
        static NULL: Field = Field::Null;
        self.column(name).and_then(|c| self.rows.get(row)?.get(c)).unwrap_or(&NULL)
    }

    /// Decodes a GetStatementResult page and appends it.
    fn extend_from(&mut self, page: &Value) {
        if self.columns.is_empty()
            && let Some(meta) = page.get("ColumnMetadata").and_then(Value::as_array)
        {
            self.columns = meta.iter().map(|c| c.get("name").and_then(Value::as_str).unwrap_or("").to_lowercase()).collect();
        }
        if let Some(records) = page.get("Records").and_then(Value::as_array) {
            for r in records {
                let fields = r.as_array().map(|f| f.iter().map(Field::from_json).collect()).unwrap_or_default();
                self.rows.push(fields);
            }
        }
    }
}

/// Where a statement is.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct Statement {
    pub id: String,
    /// SUBMITTED, PICKED, STARTED, FINISHED, ABORTED, FAILED.
    pub status: String,
    pub error: Option<String>,
    pub has_result_set: bool,
}

impl Statement {
    fn done(&self) -> bool {
        matches!(self.status.as_str(), "FINISHED" | "FAILED" | "ABORTED")
    }
}

/// A JSON call that is never sent twice after it may have run (a statement, once
/// submitted, runs): sent again only when AWS throttled it.
fn call_once(aws: &Aws, operation: &str, input: &Value) -> Result<Value> {
    let headers = vec![
        ("content-type".to_string(), "application/x-amz-json-1.1".to_string()),
        ("x-amz-target".to_string(), format!("{}.{operation}", SERVICE.target_prefix)),
    ];
    let resp = aws.raw_once(&SERVICE, "POST", &[], headers, serde_json::to_vec(input).unwrap_or_default())?;
    let mut value: Value = serde_json::from_slice(&resp.body).map_err(|e| AwsError::Network(format!("unexpected response from AWS: {e}")))?;
    sanitize_json(&mut value);
    Ok(value)
}

/// Starts `sql` on `target` as `identity`; returns the statement id.
pub fn execute(aws: &Aws, target: &Target, identity: &Identity, sql: &str) -> Result<String> {
    if !identity.is_complete() {
        return Err(AwsError::Config("no database identity: set a database and a user or a secret".into()));
    }
    let (key, value) = target.params();
    let mut input = json!({ key: value, "Database": identity.database.trim(), "Sql": sql, "StatementName": "dcli" });
    match (&identity.secret_arn, &identity.db_user) {
        (Some(arn), _) => input["SecretArn"] = json!(arn),
        (None, Some(user)) => input["DbUser"] = json!(user.trim()),
        (None, None) => {}
    }
    // The token makes a repeat of the same request (a lost answer) the same statement.
    input["ClientToken"] = json!(client_token());
    let out = call_once(aws, "ExecuteStatement", &input)?;
    out.get("Id").and_then(Value::as_str).map(str::to_string).ok_or_else(|| AwsError::Network("ExecuteStatement returned no statement id".into()))
}

fn client_token() -> String {
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos());
    format!("dcli-{nanos:x}-{:x}", std::process::id())
}

pub fn describe(aws: &Aws, id: &str) -> Result<Statement> {
    aws.call(&SERVICE, "DescribeStatement", &json!({ "Id": id }))
}

/// Stops a statement dcli started (one that ran out of time).
pub fn cancel(aws: &Aws, id: &str) -> Result<()> {
    let _: Value = aws.call(&SERVICE, "CancelStatement", &json!({ "Id": id }))?;
    Ok(())
}

/// Every page of a finished statement's result.
pub fn results(aws: &Aws, id: &str) -> Result<Rows> {
    let mut rows = Rows::default();
    let mut token: Option<String> = None;
    for _ in 0..MAX_PAGES {
        let mut input = json!({ "Id": id });
        if let Some(t) = &token {
            input["NextToken"] = json!(t);
        }
        let page: Value = aws.call(&SERVICE, "GetStatementResult", &input)?;
        rows.extend_from(&page);
        token = page.get("NextToken").and_then(Value::as_str).filter(|t| !t.is_empty()).map(str::to_string);
        if token.is_none() {
            break;
        }
    }
    Ok(rows)
}

/// Runs `sql` and waits for its rows, at most [`TIMEOUT`]; a statement still running then is
/// cancelled. A failed statement is an error with the database's message.
pub fn run(aws: &Aws, target: &Target, identity: &Identity, sql: &str) -> Result<Rows> {
    let id = execute(aws, target, identity, sql)?;
    let started = Instant::now();
    let mut wait = Duration::from_millis(150);
    loop {
        let s = describe(aws, &id)?;
        if s.done() {
            return match s.status.as_str() {
                "FINISHED" if s.has_result_set => results(aws, &id),
                "FINISHED" => Ok(Rows::default()),
                _ => Err(AwsError::Config(statement_error(&s))),
            };
        }
        if started.elapsed() >= TIMEOUT {
            let _ = cancel(aws, &id);
            return Err(AwsError::Config(format!("the query ran for more than {} s and was cancelled", TIMEOUT.as_secs())));
        }
        std::thread::sleep(wait);
        wait = (wait * 2).min(Duration::from_millis(1000));
    }
}

/// What went wrong, with a hint when it is the identity.
fn statement_error(s: &Statement) -> String {
    let error = s.error.clone().unwrap_or_else(|| s.status.to_lowercase());
    let lower = error.to_lowercase();
    if lower.contains("permission denied") || lower.contains("not authorized") || lower.contains("authentication") || lower.contains("password") {
        format!("{error} (check the database identity dcli uses for this cluster)")
    } else {
        error
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_typed_fields_and_cleans_text() {
        let page = json!({
            "ColumnMetadata": [{"name": "PID"}, {"name": "query_text"}, {"name": "elapsed"}, {"name": "flag"}, {"name": "gone"}],
            "Records": [[
                {"longValue": 1073}, {"stringValue": "select 1 \u{1b}[2J-- x"}, {"doubleValue": 1.5}, {"booleanValue": true}, {"isNull": true}
            ]],
            "TotalNumRows": 1
        });
        let mut rows = Rows::default();
        rows.extend_from(&page);
        assert_eq!(rows.columns, ["pid", "query_text", "elapsed", "flag", "gone"]);
        assert_eq!(rows.get(0, "pid").int(), Some(1073));
        assert_eq!(rows.get(0, "query_text").text(), "select 1 -- x");
        assert_eq!(rows.get(0, "elapsed").num(), Some(1.5));
        assert_eq!(rows.get(0, "flag"), &Field::Bool(true));
        assert_eq!(rows.get(0, "gone"), &Field::Null);
        assert_eq!(rows.get(0, "missing"), &Field::Null);
        assert_eq!(rows.get(3, "pid"), &Field::Null);
    }

    #[test]
    fn identities_describe_themselves_without_secrets() {
        let temp = Identity { database: "dev".into(), db_user: Some("admin".into()), secret_arn: None };
        assert!(temp.is_complete());
        assert_eq!(temp.describe(), "dev as admin (temporary credentials)");
        let secret = Identity { database: "dev".into(), db_user: None, secret_arn: Some("arn:aws:secretsmanager:us-east-1:1:secret:prod/analytics/redshift-AbCdEf".into()) };
        assert_eq!(secret.describe(), "dev with the secret prod/analytics/redshift");
        assert!(!Identity { database: "dev".into(), ..Default::default() }.is_complete());
        assert!(!Identity { database: " ".into(), db_user: Some("a".into()), secret_arn: None }.is_complete());
        assert_eq!(secret_name("plain"), "plain");
        assert_eq!(serde_json::to_string(&temp).unwrap(), r#"{"database":"dev","dbUser":"admin"}"#);
    }

    #[test]
    fn identity_errors_point_at_the_identity() {
        let s = Statement { status: "FAILED".into(), error: Some("ERROR: permission denied for relation stv_recents".into()), ..Default::default() };
        assert!(statement_error(&s).ends_with("(check the database identity dcli uses for this cluster)"));
        let s = Statement { status: "ABORTED".into(), ..Default::default() };
        assert_eq!(statement_error(&s), "aborted");
    }
}
