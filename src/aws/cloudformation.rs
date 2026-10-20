//! CloudFormation stack events, to explain a failed deploy action. CloudFormation speaks
//! the query protocol (form-encoded request, XML response).

use super::{Aws, Result, Service};
use crate::util::{parse_rfc3339, xml_blocks, xml_text};

pub const SERVICE: Service = Service { endpoint: "cloudformation", signing_name: "cloudformation", target_prefix: "" };

const VERSION: &str = "2010-05-15";

#[derive(Debug, Clone, Default)]
pub struct StackEvent {
    pub logical_id: String,
    pub resource_type: String,
    pub status: String,
    pub reason: Option<String>,
    /// Unix seconds.
    pub timestamp: Option<f64>,
}

impl StackEvent {
    pub fn failed(&self) -> bool {
        self.status.ends_with("_FAILED")
    }
}

/// Events of `stack`, newest first, until `since` (Unix seconds) or `max_pages` pages.
pub fn stack_events(aws: &Aws, stack: &str, since: f64, max_pages: usize) -> Result<Vec<StackEvent>> {
    let mut events = Vec::new();
    let mut token: Option<String> = None;
    for _ in 0..max_pages {
        let mut params = vec![("StackName", stack)];
        if let Some(t) = &token {
            params.push(("NextToken", t));
        }
        let xml = aws.query(&SERVICE, "DescribeStackEvents", VERSION, &params)?;
        let list = xml_blocks(&xml, "StackEvents").into_iter().next().unwrap_or("");
        let mut older = false;
        for m in xml_blocks(list, "member") {
            let event = StackEvent {
                logical_id: xml_text(m, "LogicalResourceId").unwrap_or_default(),
                resource_type: xml_text(m, "ResourceType").unwrap_or_default(),
                status: xml_text(m, "ResourceStatus").unwrap_or_default(),
                reason: xml_text(m, "ResourceStatusReason").filter(|r| !r.is_empty()),
                timestamp: xml_text(m, "Timestamp").and_then(|t| parse_rfc3339(&t)),
            };
            older |= event.timestamp.is_some_and(|t| t < since);
            if !older {
                events.push(event);
            }
        }
        token = xml_text(&xml, "NextToken").filter(|t| !t.is_empty());
        if older || token.is_none() {
            break;
        }
    }
    Ok(events)
}
