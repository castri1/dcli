//! `dcli mcp`: a Model Context Protocol server on stdin/stdout, for Claude and other AI agents.
//! It reads an environment's account (inventory, configuration, metrics, the costs last read,
//! what Savings finds) and never changes it; the one thing it writes is recommendations, to
//! the Advisor ([`crate::advisor`]), which the user reviews and acts on.
//!
//! Messages are JSON-RPC 2.0, one per line. Each request runs on its own thread (a client may
//! call several tools at once), and replies are written whole, one at a time. Nothing but
//! protocol goes to stdout; notes for a person go to stderr.
//!
//! It never signs in, opens a browser or reads Cost Explorer (which bills each request): an
//! expired session or missing costs come back as a message asking the user to act.

mod prompt;
mod tools;

use std::collections::HashMap;
use std::io::{BufRead, IsTerminal, Write};
use std::process::ExitCode;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use serde_json::{Value, json};

use crate::advisor::Store;
use crate::aws::{Aws, AwsError};
use crate::config::{Config, Environment, Loaded};
use crate::costs::Snapshot;
use crate::util::sanitize;

/// Protocol versions spoken, newest first.
const VERSIONS: &[&str] = &["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

/// JSON-RPC error codes.
const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;

#[derive(clap::Args)]
pub struct Args {
    /// Only these environments (repeat or separate with commas); default: all of them
    #[arg(long = "env", value_delimiter = ',', value_name = "ENV")]
    envs: Vec<String>,
}

pub fn serve(loaded: &Loaded, args: Args) -> Result<ExitCode> {
    for env in &args.envs {
        loaded.config.env(env)?;
    }
    if std::io::stdin().is_terminal() {
        eprintln!(
            "dcli mcp speaks the Model Context Protocol on stdin and stdout, for an AI agent. Add it to Claude Code with:\n  \
             claude mcp add dcli -- {} mcp",
            crate::util::program()
        );
    }
    let server = Server::new(loaded.config.clone(), args.envs, Store::home());
    serve_io(&server, std::io::stdin().lock(), std::io::stdout());
    Ok(ExitCode::SUCCESS)
}

/// The costs last read for an environment.
type CostsOf = Box<dyn Fn(&str) -> Option<Snapshot> + Send + Sync>;

/// What the tools work with.
pub struct Server {
    config: Config,
    /// The environments it may read (`--env`); empty: all.
    allowed: Vec<String>,
    /// Where recommendations go (`None` without a home directory).
    store: Option<Store>,
    /// Who writes recommendations: the client's name and version, from `initialize`.
    author: Mutex<String>,
    /// One AWS client per environment, so credentials are resolved once.
    clients: Mutex<HashMap<String, Arc<Aws>>>,
    /// The costs last read for an environment ([`crate::costs::cached`]; tests give their own).
    costs: CostsOf,
}

impl Server {
    pub fn new(config: Config, allowed: Vec<String>, store: Option<Store>) -> Server {
        Server {
            config,
            allowed,
            store,
            author: Mutex::new("an MCP client".into()),
            clients: Mutex::new(HashMap::new()),
            costs: Box::new(crate::costs::cached),
        }
    }

    /// The environments it may read, in the config's order.
    fn env_names(&self) -> Vec<&str> {
        self.config.environments.keys().map(String::as_str).filter(|n| self.allowed.is_empty() || self.allowed.iter().any(|a| a == n)).collect()
    }

    /// The environment `args.env` names, checked.
    fn env<'a>(&'a self, args: &Value) -> Result<(&'a str, &'a Environment), String> {
        let names = self.env_names().join(", ");
        let name = args.get("env").and_then(Value::as_str).map(str::trim).filter(|n| !n.is_empty()).ok_or_else(|| {
            format!("`env` is required: one of {names}. Use the environment the user names; if they named none, ask them.")
        })?;
        match self.config.environments.get_key_value(name) {
            Some((key, env)) if self.env_names().contains(&key.as_str()) => Ok((key.as_str(), env)),
            Some(_) => Err(format!("this server reads only {names} (dcli mcp --env)")),
            None => Err(format!("unknown environment '{}'. Environments: {names}", sanitize(name))),
        }
    }

    /// The AWS client of an environment.
    fn aws(&self, name: &str, env: &Environment) -> Arc<Aws> {
        let mut clients = self.clients.lock().unwrap_or_else(|e| e.into_inner());
        clients.entry(name.to_string()).or_insert_with(|| Aws::new(&env.profile, &env.region)).clone()
    }

    /// The AWS client of an environment whose session works.
    fn session(&self, name: &str, env: &Environment) -> Result<Arc<Aws>, String> {
        let aws = self.aws(name, env);
        aws.check_credentials().map_err(|e| aws_error(name, &e))?;
        Ok(aws)
    }

    fn store(&self) -> Result<&Store, String> {
        self.store.as_ref().ok_or_else(|| "cannot find the home directory, where dcli keeps recommendations".to_string())
    }

    fn author(&self) -> String {
        self.author.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Answers a request: its result, or a JSON-RPC error (code, message).
    fn handle(&self, method: &str, params: &Value) -> Result<Value, (i64, String)> {
        match method {
            "initialize" => Ok(self.initialize(params)),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({ "tools": tools::list() })),
            "tools/call" => {
                let name = params.get("name").and_then(Value::as_str).ok_or((INVALID_PARAMS, "tools/call needs `name`".to_string()))?;
                let args = match params.get("arguments") {
                    None | Some(Value::Null) => json!({}),
                    Some(a @ Value::Object(_)) => a.clone(),
                    Some(_) => return Err((INVALID_PARAMS, "`arguments` is an object".into())),
                };
                if !tools::exists(name) {
                    return Err((INVALID_PARAMS, format!("unknown tool: {}", sanitize(name))));
                }
                let out = tools::call(self, name, &args);
                Ok(json!({ "content": [{ "type": "text", "text": out.text }], "isError": out.error }))
            }
            "prompts/list" => Ok(prompt::list()),
            "prompts/get" => {
                let name = params.get("name").and_then(Value::as_str).ok_or((INVALID_PARAMS, "prompts/get needs `name`".to_string()))?;
                prompt::get(name, params.get("arguments").unwrap_or(&Value::Null)).map_err(|e| (INVALID_PARAMS, e))
            }
            _ => Err((METHOD_NOT_FOUND, format!("method not found: {}", sanitize(method)))),
        }
    }

    fn initialize(&self, params: &Value) -> Value {
        let asked = params.get("protocolVersion").and_then(Value::as_str).unwrap_or_default();
        let version = VERSIONS.iter().find(|v| **v == asked).unwrap_or(&VERSIONS[0]);
        let client = params.get("clientInfo");
        let part = |key: &str| client.and_then(|c| c.get(key)).and_then(Value::as_str).map(|s| sanitize(s).replace(['\n', '\t'], " "));
        if let Some(name) = part("name").filter(|n| !n.trim().is_empty()) {
            let author = format!("{} {}", name.trim(), part("version").unwrap_or_default().trim());
            *self.author.lock().unwrap_or_else(|e| e.into_inner()) = author.trim().chars().take(80).collect();
        }
        json!({
            "protocolVersion": version,
            "capabilities": { "tools": { "listChanged": false }, "prompts": { "listChanged": false } },
            "serverInfo": { "name": "dcli", "title": "dcli", "version": env!("CARGO_PKG_VERSION") },
            "instructions": prompt::INSTRUCTIONS,
        })
    }
}

/// An AWS error as a tool says it: an expired session asks the user to sign in.
fn aws_error(env: &str, e: &AwsError) -> String {
    if e.is_login_required() {
        format!("The AWS session for '{env}' expired: ask the user to run `dcli login {env}` (this server never signs in by itself).")
    } else {
        sanitize(&e.short())
    }
}

fn reply(id: Value, result: Result<Value, (i64, String)>) -> Value {
    match result {
        Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
        Err((code, message)) => json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } }),
    }
}

/// Serves `input` until it ends, answering on `output`; returns once every request is answered.
pub fn serve_io(server: &Server, input: impl BufRead, output: impl Write + Send) {
    let output = Mutex::new(output);
    let send = |message: &Value| {
        let mut out = output.lock().unwrap_or_else(|e| e.into_inner());
        // serde_json escapes newlines inside strings, so a message is one line.
        let _ = writeln!(out, "{message}");
        let _ = out.flush();
    };
    std::thread::scope(|s| {
        for line in input.lines() {
            let Ok(line) = line else { break };
            if line.trim().is_empty() {
                continue;
            }
            let message: Value = match serde_json::from_str(&line) {
                Ok(m) => m,
                Err(e) => {
                    send(&reply(Value::Null, Err((PARSE_ERROR, format!("not JSON: {e}")))));
                    continue;
                }
            };
            let Some(object) = message.as_object() else {
                send(&reply(Value::Null, Err((INVALID_REQUEST, "a message is a JSON object".into()))));
                continue;
            };
            // Answers to requests of ours (there are none) and notifications need no reply.
            let Some(method) = object.get("method").and_then(Value::as_str).map(str::to_string) else { continue };
            let id = match object.get("id") {
                None | Some(Value::Null) => continue,
                Some(id @ (Value::String(_) | Value::Number(_))) => id.clone(),
                Some(_) => {
                    send(&reply(Value::Null, Err((INVALID_REQUEST, "`id` is a string or a number".into()))));
                    continue;
                }
            };
            let params = object.get("params").cloned().unwrap_or_else(|| json!({}));
            let send = &send;
            s.spawn(move || send(&reply(id, server.handle(&method, &params))));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Environment;

    pub(super) fn server(store: Option<Store>, costs: Option<Snapshot>) -> Server {
        let mut config = Config::default();
        for name in ["dev", "prod"] {
            config.environments.insert(name.into(), Environment { profile: format!("{name}-acct"), region: "us-east-1".into(), pipelines: Vec::new() });
        }
        let mut server = Server::new(config, Vec::new(), store);
        server.costs = Box::new(move |env| costs.clone().filter(|_| env == "dev"));
        server
    }

    /// The replies to `lines`, in order of id (requests run in parallel).
    pub(super) fn session(server: &Server, lines: &[Value]) -> Vec<Value> {
        let input: String = lines.iter().map(|l| format!("{l}\n")).collect();
        let mut out = Vec::new();
        serve_io(server, input.as_bytes(), &mut out);
        let mut replies: Vec<Value> = String::from_utf8(out).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        replies.sort_by_key(|r| r["id"].as_i64().unwrap_or(-1));
        replies
    }

    fn request(id: i64, method: &str, params: Value) -> Value {
        json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })
    }

    #[test]
    fn initializes_and_lists() {
        let s = server(None, None);
        let replies = session(
            &s,
            &[
                request(1, "initialize", json!({ "protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": { "name": "claude-code", "version": "2.1.0" } })),
                json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
                request(2, "tools/list", json!({})),
                request(3, "prompts/list", json!({})),
                request(4, "ping", json!({})),
            ],
        );
        assert_eq!(replies.len(), 4, "the notification has no reply: {replies:?}");
        let init = &replies[0]["result"];
        assert_eq!(init["protocolVersion"], "2025-06-18");
        assert_eq!(init["serverInfo"]["name"], "dcli");
        assert!(init["instructions"].as_str().unwrap().contains("never changes them"));
        assert_eq!(s.author(), "claude-code 2.1.0");
        let tools = replies[1]["result"]["tools"].as_array().unwrap();
        assert!(tools.iter().any(|t| t["name"] == "recommend"));
        for t in tools {
            assert_eq!(t["inputSchema"]["type"], "object", "{}", t["name"]);
            assert!(t["annotations"]["readOnlyHint"].is_boolean(), "{}", t["name"]);
        }
        assert_eq!(replies[2]["result"]["prompts"][0]["name"], "audit");
        assert_eq!(replies[3]["result"], json!({}));
    }

    #[test]
    fn newest_version_for_an_unknown_one() {
        let s = server(None, None);
        let r = session(&s, &[request(1, "initialize", json!({ "protocolVersion": "2099-01-01" }))]);
        assert_eq!(r[0]["result"]["protocolVersion"], VERSIONS[0]);
    }

    #[test]
    fn protocol_errors() {
        let s = server(None, None);
        let input = "{not json\n[1,2]\n{\"jsonrpc\":\"2.0\",\"id\":true,\"method\":\"ping\"}\n";
        let mut out = Vec::new();
        serve_io(&s, input.as_bytes(), &mut out);
        let replies: Vec<Value> = String::from_utf8(out).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        let codes: Vec<i64> = replies.iter().map(|r| r["error"]["code"].as_i64().unwrap()).collect();
        assert_eq!(codes, [PARSE_ERROR, INVALID_REQUEST, INVALID_REQUEST]);
        assert!(replies.iter().all(|r| r["id"].is_null()));
        let r = session(
            &s,
            &[
                request(1, "resources/list", json!({})),
                request(2, "tools/call", json!({ "name": "rm_rf", "arguments": {} })),
                request(3, "tools/call", json!({ "arguments": {} })),
                request(4, "prompts/get", json!({ "name": "audit", "arguments": {} })),
                request(5, "tools/call", json!({ "name": "costs", "arguments": [] })),
            ],
        );
        let codes: Vec<i64> = r.iter().map(|r| r["error"]["code"].as_i64().unwrap()).collect();
        assert_eq!(codes, [METHOD_NOT_FOUND, INVALID_PARAMS, INVALID_PARAMS, INVALID_PARAMS, INVALID_PARAMS]);
    }

    #[test]
    fn environments_are_checked() {
        let mut s = server(None, None);
        s.allowed = vec!["dev".into()];
        assert!(s.env(&json!({})).unwrap_err().contains("`env` is required: one of dev"));
        assert!(s.env(&json!({ "env": "prod" })).unwrap_err().contains("reads only dev"));
        assert!(s.env(&json!({ "env": "qa\u{1b}[2J" })).unwrap_err().contains("unknown environment 'qa'"));
        assert_eq!(s.env(&json!({ "env": " dev " })).unwrap().0, "dev");
    }
}
