//! Amazon States Language: a state machine's definition read into its states and the
//! transitions between them, then arranged as a flow to draw like the console's graph:
//! sequences, Choice branches that join again, Parallel and Map states with the flows
//! inside them, and catch handlers beside the main flow. Transitions that do not fit that
//! shape (loops back, jumps to a state drawn elsewhere) become "go to" items.

use std::collections::{HashMap, HashSet};

use serde_json::Value;

use crate::aws::sfn::lambda_name;
use crate::util::fmt_duration;

/// Where `End: true` and Succeed states lead.
pub const END: &str = "\u{0}end";

#[derive(Debug, Clone, Default, PartialEq)]
pub struct State {
    pub name: String,
    /// `Task`, `Choice`, `Parallel`, `Map`, `Pass`, `Wait`, `Succeed`, `Fail`.
    pub kind: String,
    /// What it does, in a few words: `λ orders-sync`, `sqs:sendMessage`, `wait 30s`…
    pub summary: String,
    /// Where it goes: (label, state) for each Choice rule and the default, else the one
    /// `Next` with an empty label, or [`END`]. Empty for Fail states.
    pub next: Vec<(String, String)>,
    /// Catchers: the errors they catch and the state they go to.
    pub catches: Vec<(String, String)>,
    /// The flows inside: the branches of a Parallel state, the item processor of a Map.
    pub branches: Vec<Machine>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Machine {
    pub start: String,
    /// In the order of the definition.
    pub states: Vec<State>,
}

impl Machine {
    pub fn parse(definition: &str) -> Result<Machine, String> {
        let mut v: Value = serde_json::from_str(definition).map_err(|e| format!("the definition is not valid JSON: {e}"))?;
        crate::util::sanitize_json(&mut v);
        Machine::from_value(&v)
    }

    fn from_value(v: &Value) -> Result<Machine, String> {
        let start = v["StartAt"].as_str().ok_or("the definition has no StartAt")?.to_string();
        let states = v["States"].as_object().ok_or("the definition has no States")?;
        let states = states.iter().map(|(name, s)| State::from_value(name, s)).collect::<Result<Vec<_>, _>>()?;
        Ok(Machine { start, states })
    }

    pub fn get(&self, name: &str) -> Option<&State> {
        self.states.iter().find(|s| s.name == name)
    }

    /// Every state, those inside Parallel and Map states included (state names are unique
    /// in the whole machine), with how deeply it is nested.
    pub fn all(&self) -> Vec<(&State, usize)> {
        fn walk<'a>(m: &'a Machine, depth: usize, out: &mut Vec<(&'a State, usize)>) {
            for s in &m.states {
                out.push((s, depth));
                for b in &s.branches {
                    walk(b, depth + 1, out);
                }
            }
        }
        let mut out = Vec::new();
        walk(self, 0, &mut out);
        out
    }
}

impl State {
    fn from_value(name: &str, v: &Value) -> Result<State, String> {
        let kind = v["Type"].as_str().unwrap_or("Pass").to_string();
        let mut next = Vec::new();
        match kind.as_str() {
            "Choice" => {
                for rule in v["Choices"].as_array().into_iter().flatten() {
                    if let Some(target) = rule["Next"].as_str() {
                        next.push((rule_label(rule), target.to_string()));
                    }
                }
                if let Some(d) = v["Default"].as_str() {
                    next.push(("default".into(), d.to_string()));
                }
            }
            "Fail" => {}
            "Succeed" => next.push((String::new(), END.into())),
            _ => next.push((String::new(), v["Next"].as_str().unwrap_or(END).to_string())),
        }
        let catches = v["Catch"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|c| {
                let errors: Vec<&str> = c["ErrorEquals"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
                Some((errors.join(", "), c["Next"].as_str()?.to_string()))
            })
            .collect();
        let branches = match kind.as_str() {
            "Parallel" => v["Branches"].as_array().into_iter().flatten().map(Machine::from_value).collect::<Result<_, _>>()?,
            "Map" => {
                let processor = if v["ItemProcessor"].is_object() { &v["ItemProcessor"] } else { &v["Iterator"] };
                if processor.is_object() { vec![Machine::from_value(processor)?] } else { Vec::new() }
            }
            _ => Vec::new(),
        };
        Ok(State { name: name.to_string(), summary: summary(&kind, v, branches.len()), kind, next, catches, branches })
    }
}

fn summary(kind: &str, v: &Value, branches: usize) -> String {
    match kind {
        "Task" => task_summary(v),
        "Wait" => match (&v["Seconds"], &v["SecondsPath"], &v["Timestamp"], &v["TimestampPath"]) {
            (Value::Number(n), ..) => format!("wait {}", fmt_duration(n.as_f64().unwrap_or(0.0))),
            (Value::String(s), ..) | (_, Value::String(s), ..) => format!("wait {s}"),
            (_, _, Value::String(t), _) | (.., Value::String(t)) => format!("until {t}"),
            _ => "wait".into(),
        },
        "Parallel" => format!("parallel · {branches} branches"),
        "Map" if v["ItemProcessor"]["ProcessorConfig"]["Mode"].as_str() == Some("DISTRIBUTED") => "map · distributed".into(),
        "Map" => "map · each item".into(),
        "Fail" => v["Error"].as_str().map_or_else(|| "fail".into(), |e| format!("fail · {e}")),
        other => other.to_lowercase(),
    }
}

/// `λ orders-sync`, `sqs:sendMessage`, `ecs:runTask.sync`, `activity name`…
fn task_summary(v: &Value) -> String {
    let resource = v["Resource"].as_str().unwrap_or_default();
    let params = if v["Parameters"].is_object() { &v["Parameters"] } else { &v["Arguments"] };
    if let Some(rest) = resource.strip_prefix("arn:aws:states:::") {
        if rest.starts_with("lambda:invoke") {
            return match params["FunctionName"].as_str() {
                Some(f) if !f.starts_with("{%") => format!("λ {}", lambda_name(f)),
                _ => "λ from the input".into(),
            };
        }
        return rest.strip_prefix("aws-sdk:").unwrap_or(rest).to_string();
    }
    if resource.contains(":function:") {
        return format!("λ {}", lambda_name(resource));
    }
    if let Some((_, name)) = resource.split_once(":activity:") {
        return format!("activity {name}");
    }
    resource.to_string()
}

/// A Choice rule in a few words: `status = "RUNNING"`, `amount > 100`, `a and b`.
pub fn rule_label(rule: &Value) -> String {
    if let Some(c) = rule["Condition"].as_str() {
        return c.trim().trim_start_matches("{%").trim_end_matches("%}").trim().to_string();
    }
    if let Some(all) = rule["And"].as_array() {
        return all.iter().map(rule_label).collect::<Vec<_>>().join(" and ");
    }
    if let Some(any) = rule["Or"].as_array() {
        return any.iter().map(rule_label).collect::<Vec<_>>().join(" or ");
    }
    if rule["Not"].is_object() {
        return format!("not ({})", rule_label(&rule["Not"]));
    }
    let short = |p: &str| p.strip_prefix("$.").unwrap_or(p).to_string();
    let var = rule["Variable"].as_str().map_or_else(|| "?".into(), short);
    let comparison = rule
        .as_object()
        .and_then(|o| o.iter().find(|(k, _)| !matches!(k.as_str(), "Variable" | "Next" | "Comment" | "Assign" | "Output")));
    let Some((op, value)) = comparison else { return var };
    if let Some(test) = op.strip_prefix("Is") {
        let yes = value.as_bool().unwrap_or(true);
        return match (test, yes) {
            ("Present", true) => format!("{var} exists"),
            ("Present", false) => format!("{var} missing"),
            (t, true) => format!("{var} is {}", t.to_lowercase()),
            (t, false) => format!("{var} not {}", t.to_lowercase()),
        };
    }
    let (base, path) = match op.strip_suffix("Path") {
        Some(b) => (b, true),
        None => (op.as_str(), false),
    };
    let base = ["String", "Numeric", "Boolean", "Timestamp"].iter().find_map(|t| base.strip_prefix(t)).unwrap_or(base);
    let symbol = match base {
        "Equals" => "=",
        "LessThan" => "<",
        "GreaterThan" => ">",
        "LessThanEquals" => "<=",
        "GreaterThanEquals" => ">=",
        "Matches" => "~",
        other => other,
    };
    let value = match value {
        Value::String(s) if path => short(s),
        Value::String(s) => format!("\"{s}\""),
        other => other.to_string(),
    };
    format!("{var} {symbol} {value}")
}

// ---- arranging the flow ----------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub enum Item {
    /// A state, with the flows inside it (Parallel branches, a Map's item processor).
    State { name: String, inner: Vec<Flow> },
    /// The ways out of the Choice state just before. Arms that join continue with what
    /// follows the split.
    Split(Vec<Arm>),
    /// A transition to a state drawn elsewhere; `back` when it loops to an earlier one.
    Goto { target: String, back: bool },
    /// The flow ends (End: true, Succeed).
    End,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Arm {
    pub label: String,
    /// The state the arm goes to (also when it is drawn elsewhere or right after the split).
    pub target: String,
    pub items: Vec<Item>,
    pub joins: bool,
}

/// A machine's states arranged for drawing: the main flow from StartAt, and the flows of
/// catch handlers that are not on it, each with the states that catch to it.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Flow {
    pub main: Vec<Item>,
    /// The main flow reaches the end of its branch (flows inside Parallel and Map states).
    pub joins: bool,
    pub handlers: Vec<(Vec<String>, Vec<Item>)>,
}

/// Arranges `machine`; `inner` for the flows inside Parallel and Map states, whose end
/// joins the state's own way out instead of ending the machine.
pub fn arrange(machine: &Machine, inner: bool) -> Flow {
    let mut b = Builder { machine, placed: HashSet::new(), rank: ranks(machine) };
    let (main, joins) = b.sequence("", &machine.start, inner.then_some(END));
    let mut handlers = Vec::new();
    loop {
        let target = machine
            .states
            .iter()
            .filter(|s| b.placed.contains(&s.name))
            .flat_map(|s| &s.catches)
            .map(|(_, t)| t)
            .find(|t| !b.placed.contains(*t) && machine.get(t).is_some())
            .cloned();
        let Some(target) = target else { break };
        let from: Vec<String> =
            machine.states.iter().filter(|s| s.catches.iter().any(|(_, t)| *t == target)).map(|s| s.name.clone()).collect();
        let (items, _) = b.sequence("", &target, None);
        handlers.push((from, items));
    }
    Flow { main, joins, handlers }
}

struct Builder<'a> {
    machine: &'a Machine,
    placed: HashSet<String>,
    /// Position in a topological order (loops aside): which of two states comes first.
    rank: HashMap<String, usize>,
}

impl Builder<'_> {
    fn rank(&self, name: &str) -> usize {
        self.rank.get(name).copied().unwrap_or(usize::MAX)
    }

    /// The items from `start` until `stop` (true when it got there), a state already drawn
    /// (a go to), or the end. `from` is the state that leads to `start`.
    fn sequence(&mut self, from: &str, start: &str, stop: Option<&str>) -> (Vec<Item>, bool) {
        let mut items = Vec::new();
        let (mut prev, mut cur) = (from.to_string(), start.to_string());
        loop {
            if stop == Some(cur.as_str()) {
                return (items, true);
            }
            if cur == END {
                items.push(Item::End);
                return (items, false);
            }
            let machine = self.machine;
            let state = match machine.get(&cur) {
                Some(s) if !self.placed.contains(&cur) => s,
                _ => {
                    let back = self.rank(&cur) <= self.rank(&prev);
                    items.push(Item::Goto { target: cur, back });
                    return (items, false);
                }
            };
            self.placed.insert(cur.clone());
            items.push(Item::State { name: cur.clone(), inner: state.branches.iter().map(|b| arrange(b, true)).collect() });
            // Rules that go to the same state share an arm.
            let mut arms: Vec<(String, String)> = Vec::new();
            for (label, target) in &state.next {
                match arms.iter_mut().find(|(_, t)| t == target) {
                    Some((l, _)) => *l = format!("{l} | {label}"),
                    None => arms.push((label.clone(), target.clone())),
                }
            }
            match arms.len() {
                0 => return (items, false),
                1 if state.kind != "Choice" => {
                    prev = std::mem::replace(&mut cur, arms.remove(0).1);
                }
                _ => {
                    let targets: Vec<String> = arms.iter().map(|(_, t)| t.clone()).collect();
                    let merge = self.merge(&targets, stop);
                    let split = arms
                        .into_iter()
                        .map(|(label, target)| {
                            let (items, joins) = self.sequence(&cur, &target, merge.as_deref());
                            Arm { label, target, items, joins }
                        })
                        .collect();
                    items.push(Item::Split(split));
                    match merge {
                        Some(m) => prev = std::mem::replace(&mut cur, m),
                        None => return (items, false),
                    }
                }
            }
        }
    }

    /// Where the arms of a split meet again: the first state (or the end) that more than
    /// one of them reaches. When they never meet, the arm that leads furthest continues
    /// below the split and the others end on their own.
    fn merge(&self, targets: &[String], stop: Option<&str>) -> Option<String> {
        let reach: Vec<HashSet<String>> = targets.iter().map(|t| self.reach(t, stop)).collect();
        let mut count: HashMap<&str, usize> = HashMap::new();
        for name in reach.iter().flatten() {
            *count.entry(name).or_default() += 1;
        }
        let met = count.into_iter().filter(|(_, c)| *c >= 2).map(|(n, _)| n).min_by_key(|n| (self.rank(n), *n));
        if let Some(m) = met {
            return Some(m.to_string());
        }
        let mut furthest: Option<(usize, &String)> = None;
        for (r, t) in reach.iter().zip(targets) {
            if !r.is_empty() && furthest.is_none_or(|(n, _)| r.len() > n) {
                furthest = Some((r.len(), t));
            }
        }
        furthest.map(|(_, t)| t.clone())
    }

    /// States not drawn yet that `from` leads to (itself included), up to `stop`.
    fn reach(&self, from: &str, stop: Option<&str>) -> HashSet<String> {
        let mut seen = HashSet::new();
        let mut todo = vec![from.to_string()];
        while let Some(n) = todo.pop() {
            if self.placed.contains(&n) || !seen.insert(n.clone()) || stop == Some(n.as_str()) || n == END {
                continue;
            }
            if let Some(s) = self.machine.get(&n) {
                todo.extend(s.next.iter().map(|(_, t)| t.clone()));
            }
        }
        seen
    }
}

/// Reverse postorder of a depth-first walk from StartAt (catches after the transitions):
/// a topological order once the edges that loop back are left out. [`END`] comes last.
fn ranks(machine: &Machine) -> HashMap<String, usize> {
    let succ = |name: &str| -> Vec<String> {
        machine.get(name).map_or_else(Vec::new, |s| s.next.iter().chain(&s.catches).map(|(_, t)| t.clone()).collect())
    };
    let mut seen: HashSet<String> = HashSet::from([machine.start.clone()]);
    let mut stack: Vec<(String, Vec<String>, usize)> = vec![(machine.start.clone(), succ(&machine.start), 0)];
    let mut post = Vec::new();
    while let Some((name, next, i)) = stack.last_mut() {
        if let Some(s) = next.get(*i).cloned() {
            *i += 1;
            if seen.insert(s.clone()) {
                let n = succ(&s);
                stack.push((s, n, 0));
            }
        } else {
            post.push(std::mem::take(name));
            stack.pop();
        }
    }
    post.into_iter().rev().enumerate().map(|(i, n)| (n, i)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(name: &str) -> Item {
        Item::State { name: name.into(), inner: Vec::new() }
    }

    fn main(definition: &str) -> Vec<Item> {
        arrange(&Machine::parse(definition).unwrap(), false).main
    }

    #[test]
    fn reads_states() {
        let m = Machine::parse(
            r#"{"StartAt": "Charge", "States": {
                "Charge": {"Type": "Task", "Resource": "arn:aws:states:::lambda:invoke",
                           "Parameters": {"FunctionName": "arn:aws:lambda:us-east-1:1:function:pay:live"},
                           "Catch": [{"ErrorEquals": ["Declined", "Fraud"], "Next": "Rejected"}], "Next": "Wait"},
                "Wait": {"Type": "Wait", "Seconds": 90, "Next": "Notify"},
                "Notify": {"Type": "Task", "Resource": "arn:aws:states:::aws-sdk:sns:publish", "End": true},
                "Rejected": {"Type": "Fail", "Error": "PaymentRejected"}}}"#,
        )
        .unwrap();
        let summaries: Vec<&str> = m.states.iter().map(|s| s.summary.as_str()).collect();
        assert_eq!(summaries, ["λ pay", "wait 1m 30s", "sns:publish", "fail · PaymentRejected"]);
        assert_eq!(m.states[0].catches, [("Declined, Fraud".to_string(), "Rejected".to_string())]);
        assert_eq!(m.states[2].next, [(String::new(), END.to_string())]);
        assert!(m.states[3].next.is_empty());
    }

    #[test]
    fn labels_choice_rules() {
        let label = |json: &str| rule_label(&serde_json::from_str(json).unwrap());
        assert_eq!(label(r#"{"Variable": "$.status", "StringEquals": "RUNNING"}"#), r#"status = "RUNNING""#);
        assert_eq!(label(r#"{"Variable": "$.amount", "NumericGreaterThanEquals": 100}"#), "amount >= 100");
        assert_eq!(label(r#"{"Variable": "$.a", "NumericLessThanPath": "$.b"}"#), "a < b");
        assert_eq!(label(r#"{"Variable": "$.coupon", "IsPresent": false}"#), "coupon missing");
        assert_eq!(
            label(r#"{"And": [{"Variable": "$.vip", "BooleanEquals": true}, {"Not": {"Variable": "$.x", "IsNull": true}}]}"#),
            "vip = true and not (x is null)"
        );
        assert_eq!(label(r#"{"Condition": "{% $states.input.n > 3 %}"}"#), "$states.input.n > 3");
    }

    #[test]
    fn arranges_a_sequence() {
        let items = main(r#"{"StartAt": "A", "States": {"A": {"Type": "Pass", "Next": "B"}, "B": {"Type": "Pass", "End": true}}}"#);
        assert_eq!(items, [state("A"), state("B"), Item::End]);
    }

    #[test]
    fn joins_choice_arms_where_they_meet() {
        let items = main(
            r#"{"StartAt": "Check", "States": {
                "Check": {"Type": "Choice", "Choices": [{"Variable": "$.x", "NumericGreaterThan": 1, "Next": "A"}], "Default": "B"},
                "A": {"Type": "Pass", "Next": "A2"}, "A2": {"Type": "Pass", "Next": "M"},
                "B": {"Type": "Pass", "Next": "M"},
                "M": {"Type": "Pass", "End": true}}}"#,
        );
        let arm = |label: &str, target: &str, items: Vec<Item>| Arm { label: label.into(), target: target.into(), items, joins: true };
        assert_eq!(
            items,
            [
                state("Check"),
                Item::Split(vec![arm("x > 1", "A", vec![state("A"), state("A2")]), arm("default", "B", vec![state("B")])]),
                state("M"),
                Item::End
            ]
        );
    }

    #[test]
    fn keeps_the_main_flow_straight_past_failing_arms_and_loops() {
        let items = main(
            r#"{"StartAt": "Poll", "States": {
                "Poll": {"Type": "Task", "Resource": "arn:aws:states:::aws-sdk:athena:getQueryExecution", "Next": "Done?"},
                "Done?": {"Type": "Choice", "Choices": [
                    {"Variable": "$.state", "StringEquals": "RUNNING", "Next": "Poll"},
                    {"Variable": "$.state", "StringEquals": "FAILED", "Next": "Failed"}], "Default": "Finish"},
                "Failed": {"Type": "Fail"},
                "Finish": {"Type": "Succeed"}}}"#,
        );
        assert_eq!(
            items,
            [
                state("Poll"),
                state("Done?"),
                Item::Split(vec![
                    Arm { label: r#"state = "RUNNING""#.into(), target: "Poll".into(), items: vec![Item::Goto { target: "Poll".into(), back: true }], joins: false },
                    Arm { label: r#"state = "FAILED""#.into(), target: "Failed".into(), items: vec![state("Failed")], joins: false },
                    Arm { label: "default".into(), target: "Finish".into(), items: vec![], joins: true },
                ]),
                state("Finish"),
                Item::End
            ]
        );
    }

    #[test]
    fn nests_parallel_branches_and_puts_handlers_aside() {
        let m = Machine::parse(
            r#"{"StartAt": "Both", "States": {
                "Both": {"Type": "Parallel", "Next": "Done", "Catch": [{"ErrorEquals": ["States.ALL"], "Next": "Alert"}],
                         "Branches": [{"StartAt": "X", "States": {"X": {"Type": "Pass", "End": true}}},
                                      {"StartAt": "Y", "States": {"Y": {"Type": "Pass", "Next": "Z"}, "Z": {"Type": "Pass", "End": true}}}]},
                "Done": {"Type": "Succeed"},
                "Alert": {"Type": "Task", "Resource": "arn:aws:states:::sns:publish", "Next": "Done"}}}"#,
        )
        .unwrap();
        let flow = arrange(&m, false);
        let branch = |items: Vec<Item>| Flow { main: items, joins: true, handlers: Vec::new() };
        assert_eq!(
            flow.main,
            [Item::State { name: "Both".into(), inner: vec![branch(vec![state("X")]), branch(vec![state("Y"), state("Z")])] }, state("Done"), Item::End]
        );
        assert_eq!(flow.handlers, [(vec!["Both".to_string()], vec![state("Alert"), Item::Goto { target: "Done".into(), back: false }])]);
        assert_eq!(m.all().iter().map(|(s, d)| (s.name.as_str(), *d)).collect::<Vec<_>>(), [
            ("Both", 0),
            ("X", 1),
            ("Y", 1),
            ("Z", 1),
            ("Done", 0),
            ("Alert", 0)
        ]);
    }
}
