//! EventBridge Schemas (restJson1), only for Lambda's shareable test events: the Lambda
//! console and `sam remote test-event` keep them as the examples of the OpenAPI schema
//! `_<function>-schema` in the registry `lambda-testevent-schemas`, one example per event.

use serde::Deserialize;
use serde_json::{Value, json};

use super::{Aws, AwsError, Result, Service};

pub const SERVICE: Service = Service { endpoint: "schemas", signing_name: "schemas", target_prefix: "" };

const REGISTRY: &str = "lambda-testevent-schemas";

fn schema_name(function: &str) -> String {
    format!("_{function}-schema")
}

fn not_found(e: &AwsError) -> bool {
    matches!(e, AwsError::Api { status, code, .. } if *status == 404 || code == "NotFoundException")
}

fn conflict(e: &AwsError) -> bool {
    matches!(e, AwsError::Api { status, code, .. } if *status == 409 || code == "ConflictException")
}

/// The function's schema document, when it has shared events.
fn describe(aws: &Aws, function: &str) -> Result<Option<String>> {
    #[derive(Deserialize)]
    #[serde(rename_all = "PascalCase")]
    struct Out {
        #[serde(default)]
        content: String,
    }
    let name = schema_name(function);
    match aws.rest::<Out>(&SERVICE, "GET", &segments(&name), &[], None) {
        Ok(out) => Ok(Some(out.content)),
        Err(e) if not_found(&e) => Ok(None),
        Err(e) => Err(e),
    }
}

fn segments(schema: &str) -> [&str; 7] {
    ["v1", "registries", "name", REGISTRY, "schemas", "name", schema]
}

/// The shared test events of `function`, by name.
pub fn events(aws: &Aws, function: &str) -> Result<Vec<(String, Value)>> {
    Ok(describe(aws, function)?.map(|c| examples(&c)).unwrap_or_default())
}

/// Saves `payload` as the shared event `name` (replacing one of that name); the function's
/// other events and its schema stay as they are.
pub fn save(aws: &Aws, function: &str, name: &str, payload: &Value) -> Result<()> {
    let schema = schema_name(function);
    match describe(aws, function)? {
        Some(content) => {
            let body = json!({ "Content": with_event(Some(&content), name, payload), "Type": "OpenApi3" });
            aws.rest::<Value>(&SERVICE, "PUT", &segments(&schema), &[], Some(&body))?;
        }
        None => {
            // The registry exists once anyone saved a shared event in the account.
            let registry = json!({ "Description": "Lambda shareable test events" });
            match aws.rest::<Value>(&SERVICE, "POST", &["v1", "registries", "name", REGISTRY], &[], Some(&registry)) {
                Ok(_) => {}
                Err(e) if conflict(&e) => {}
                Err(e) => return Err(e),
            }
            let body = json!({ "Content": with_event(None, name, payload), "Type": "OpenApi3" });
            aws.rest::<Value>(&SERVICE, "POST", &segments(&schema), &[], Some(&body))?;
        }
    }
    Ok(())
}

/// Removes the shared event `name`; with its last event the schema goes too. `false` when
/// there was no such event.
pub fn delete(aws: &Aws, function: &str, name: &str) -> Result<bool> {
    let schema = schema_name(function);
    let Some(content) = describe(aws, function)? else { return Ok(false) };
    match without_event(&content, name) {
        None => Ok(false),
        Some((_, 0)) => aws.rest::<Value>(&SERVICE, "DELETE", &segments(&schema), &[], None).map(|_| true),
        Some((content, _)) => {
            let body = json!({ "Content": content, "Type": "OpenApi3" });
            aws.rest::<Value>(&SERVICE, "PUT", &segments(&schema), &[], Some(&body)).map(|_| true)
        }
    }
}

/// The events in a schema document: `components.examples.<name>.value`.
fn examples(content: &str) -> Vec<(String, Value)> {
    let doc: Value = serde_json::from_str(content).unwrap_or_default();
    let Some(map) = doc.pointer("/components/examples").and_then(Value::as_object) else { return Vec::new() };
    map.iter().map(|(name, e)| (crate::util::sanitize(name), e.get("value").cloned().unwrap_or(Value::Null))).collect()
}

/// The document with the event added or replaced; a new document when there was none.
fn with_event(content: Option<&str>, name: &str, payload: &Value) -> String {
    let mut doc: Value = content.and_then(|c| serde_json::from_str(c).ok()).filter(Value::is_object).unwrap_or_else(|| {
        json!({
            "openapi": "3.0.0",
            "info": { "version": "1.0.0", "title": "Event" },
            "paths": {},
            "components": { "schemas": { "Event": { "type": "object" } }, "examples": {} },
        })
    });
    object(object(&mut doc, "components"), "examples")[name] = json!({ "value": payload });
    doc.to_string()
}

/// The document without the event and how many events are left; `None` when it has no such event.
fn without_event(content: &str, name: &str) -> Option<(String, usize)> {
    let mut doc: Value = serde_json::from_str(content).ok()?;
    let examples = doc.pointer_mut("/components/examples")?.as_object_mut()?;
    examples.remove(name)?;
    let left = examples.len();
    Some((doc.to_string(), left))
}

/// `value[key]` as an object, made one when it is missing or something else.
fn object<'a>(value: &'a mut Value, key: &str) -> &'a mut Value {
    if !value.is_object() {
        *value = json!({});
    }
    let entry = &mut value[key];
    if !entry.is_object() {
        *entry = json!({});
    }
    entry
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_are_the_examples_of_the_schema() {
        let content = with_event(None, "paid", &json!({ "id": 1 }));
        let content = with_event(Some(&content), "refund", &json!({ "id": 2, "refund": true }));
        let content = with_event(Some(&content), "paid", &json!({ "id": 3 }));
        let mut events = examples(&content);
        events.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(events, vec![("paid".to_string(), json!({ "id": 3 })), ("refund".to_string(), json!({ "id": 2, "refund": true }))]);
        let doc: Value = serde_json::from_str(&content).unwrap();
        assert_eq!(doc["openapi"], "3.0.0");
        assert_eq!(doc["components"]["schemas"]["Event"]["type"], "object");
    }

    #[test]
    fn saving_keeps_what_the_console_wrote() {
        let console = r#"{"openapi":"3.0.0","info":{"version":"1.0.0","title":"Event"},"paths":{},
            "components":{"schemas":{"Event":{"type":"object","required":["key1"],"properties":{"key1":{"type":"string"}}}},
            "examples":{"hello":{"value":{"key1":"value1"}}}}}"#;
        let doc: Value = serde_json::from_str(&with_event(Some(console), "bye", &json!({ "key1": "x" }))).unwrap();
        assert_eq!(doc["components"]["schemas"]["Event"]["required"], json!(["key1"]));
        assert_eq!(doc["components"]["examples"]["hello"]["value"]["key1"], "value1");
        assert_eq!(doc["components"]["examples"]["bye"]["value"]["key1"], "x");
    }

    #[test]
    fn deleting_counts_what_is_left() {
        let content = with_event(None, "a", &json!({}));
        let content = with_event(Some(&content), "b", &json!({}));
        assert!(without_event(&content, "nope").is_none());
        let (content, left) = without_event(&content, "a").unwrap();
        assert_eq!(left, 1);
        assert_eq!(without_event(&content, "b").unwrap().1, 0);
    }
}
