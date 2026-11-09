//! Saved test events for Lambda functions: the payloads to invoke them with again. They live
//! in two places: on this machine (`~/.deployment-cli/test-events/<function>.json`, private)
//! and in AWS, as the console's shareable test events ([`crate::aws::schemas`]), which
//! everyone with access to the account sees.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::aws::{self, Aws};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Place {
    Local,
    Shared,
}

impl Place {
    pub fn label(self) -> &'static str {
        match self {
            Place::Local => "this machine",
            Place::Shared => "shared in AWS",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Event {
    pub name: String,
    pub place: Place,
    pub payload: Value,
}

impl Event {
    /// The payload on one line.
    pub fn compact(&self) -> String {
        self.payload.to_string()
    }
}

#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
struct File {
    events: BTreeMap<String, Value>,
}

/// Names work in both places: letters, digits, `.`, `-` and `_`, up to 64.
pub fn check_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("Name the event to save it".into());
    }
    if name.len() > 64 || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b)) {
        return Err("An event's name takes letters, digits, '.', '-' and '_' (64 at most)".into());
    }
    Ok(())
}

fn path(function: &str) -> Option<PathBuf> {
    let name = format!("{}.json", crate::costs::file_name(function));
    Some(crate::util::home_dir()?.join(".deployment-cli").join("test-events").join(name))
}

fn read(function: &str) -> File {
    path(function).and_then(|p| std::fs::read_to_string(p).ok()).and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
}

fn write(function: &str, file: &File) -> std::io::Result<()> {
    let path = path(function).ok_or_else(|| std::io::Error::other("no home folder"))?;
    if file.events.is_empty() {
        return match std::fs::remove_file(&path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        };
    }
    crate::platform::write_private(&path, &serde_json::to_vec_pretty(file).map_err(std::io::Error::other)?)
}

/// The events saved on this machine for `function`, by name.
pub fn local(function: &str) -> Vec<Event> {
    read(function).events.into_iter().map(|(name, payload)| Event { name, place: Place::Local, payload }).collect()
}

pub fn save_local(function: &str, name: &str, payload: &Value) -> std::io::Result<()> {
    let mut file = read(function);
    file.events.insert(name.to_string(), payload.clone());
    write(function, &file)
}

/// `false` when there was no such event.
pub fn delete_local(function: &str, name: &str) -> std::io::Result<bool> {
    let mut file = read(function);
    if file.events.remove(name).is_none() {
        return Ok(false);
    }
    write(function, &file).map(|_| true)
}

/// The function's shareable test events, by name.
pub fn shared(aws: &Aws, function: &str) -> aws::Result<Vec<Event>> {
    let mut events: Vec<Event> =
        aws::schemas::events(aws, function)?.into_iter().map(|(name, payload)| Event { name, place: Place::Shared, payload }).collect();
    events.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(events)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_fit_both_places() {
        assert!(check_name("order.paid-v2_big").is_ok());
        assert!(check_name("").is_err());
        assert!(check_name("with space").is_err());
        assert!(check_name("a/b").is_err());
        assert!(check_name(&"x".repeat(65)).is_err());
    }
}
