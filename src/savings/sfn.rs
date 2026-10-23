//! Step Functions: busy Standard workflows of short executions, billed per state
//! transition, that could run as Express workflows.

use serde_json::Value;

use super::{Finding, Impact, Scan, billing, metric_data, par_map, peak, per_month, qty, query, query_id, total};
use crate::aws::sfn::{self, StateMachine};
use crate::aws::{self, metrics::Series};
use crate::tui::chart::fmt_ms;

const MODULE: &str = "Step Functions";
/// Express workflows last five minutes at most.
const EXPRESS_LIMIT_MS: f64 = 300_000.0;
/// State transitions a month: listed from 1 M, medium impact from 10 M, high from 100 M.
const TRANSITIONS_LOW: f64 = 1e6;
const TRANSITIONS_MEDIUM: f64 = 1e7;
const TRANSITIONS_HIGH: f64 = 1e8;

pub fn check(scan: &Scan) -> aws::Result<Vec<Finding>> {
    let standard: Vec<StateMachine> = sfn::all_state_machines(scan.aws)?.into_iter().filter(|m| !m.express()).collect();
    if standard.is_empty() {
        return Ok(Vec::new());
    }
    let mut queries = Vec::new();
    for (i, m) in standard.iter().enumerate() {
        let dims = [("StateMachineArn", m.state_machine_arn.as_str())];
        queries.push(query(query_id("st", i), "AWS/States", "ExecutionsStarted", &dims, "Sum", 86_400));
        queries.push(query(query_id("avg", i), "AWS/States", "ExecutionTime", &dims, "Average", 86_400));
        queries.push(query(query_id("max", i), "AWS/States", "ExecutionTime", &dims, "Maximum", 86_400));
    }
    let series = metric_data(scan.aws, &queries, scan.days)?;
    let runs: Vec<(&StateMachine, Runs)> =
        standard.iter().zip(series.chunks(3)).map(|(m, s)| (m, Runs::of(&s[0], &s[1], &s[2]))).collect();
    // Only busy machines short enough for Express are worth reading the definition of.
    let busy: Vec<&(&StateMachine, Runs)> = runs
        .iter()
        .filter(|(_, r)| r.max_ms < EXPRESS_LIMIT_MS && r.executions * per_month(scan.days) * 2.0 >= TRANSITIONS_LOW)
        .collect();
    let definitions = par_map(&busy, |(m, _)| sfn::describe_state_machine(scan.aws, &m.state_machine_arn));
    let mut found = Vec::new();
    for ((m, r), detail) in busy.into_iter().zip(definitions) {
        let definition = detail?.definition.unwrap_or_default();
        found.extend(express_candidate(m, r, &definition, scan.days));
    }
    Ok(found)
}

/// A machine's executions over the window.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
struct Runs {
    executions: f64,
    /// Average execution time, weighted by each day's executions (ms).
    mean_ms: f64,
    /// The longest execution (ms).
    max_ms: f64,
}

impl Runs {
    fn of(started: &Series, average: &Series, maximum: &Series) -> Runs {
        let (mut sum, mut weight) = (0.0, 0.0);
        for (t, avg) in &average.points {
            let w = started.points.iter().find(|p| p.0 == *t).map_or(0.0, |p| p.1);
            sum += avg * w;
            weight += w;
        }
        Runs {
            executions: total(started),
            mean_ms: if weight > 0.0 { sum / weight } else { 0.0 },
            max_ms: peak(maximum).unwrap_or(0.0),
        }
    }
}

/// States of a definition, those inside Map and Parallel states included.
pub(crate) fn count_states(v: &Value) -> usize {
    match v {
        Value::Object(map) => {
            let own = map.get("States").and_then(Value::as_object).map_or(0, |s| s.len());
            own + map.values().map(count_states).sum::<usize>()
        }
        Value::Array(items) => items.iter().map(count_states).sum(),
        _ => 0,
    }
}

/// Integration patterns Express workflows do not run: waiting for a job (`.sync`) or for a
/// callback (`.waitForTaskToken`).
fn waits(definition: &str) -> bool {
    definition.contains(".sync") || definition.contains(".waitForTaskToken")
}

fn express_candidate(m: &StateMachine, r: &Runs, definition: &str, days: u32) -> Option<Finding> {
    if r.executions <= 0.0 || r.max_ms >= EXPRESS_LIMIT_MS || waits(definition) {
        return None;
    }
    let states = serde_json::from_str::<Value>(definition).map(|v| count_states(&v)).unwrap_or(0).max(1);
    let monthly = r.executions * per_month(days);
    let transitions = monthly * states as f64;
    if transitions < TRANSITIONS_LOW {
        return None;
    }
    Some(
        Finding::new(
            MODULE,
            &m.state_machine_arn,
            &m.name,
            format!("Standard: {} executions a month of {} on average (at most {})", qty(monthly), fmt_ms(r.mean_ms), fmt_ms(r.max_ms)),
            "Consider an Express workflow",
            format!("≈ {} state transitions a month billed in Standard", qty(transitions)),
            Impact::of(transitions, TRANSITIONS_MEDIUM, TRANSITIONS_HIGH),
        )
        .score(transitions / TRANSITIONS_HIGH)
        .billed_at_most(billing::SFN_TRANSITIONS, transitions)
        .evidence(format!("{} executions started in {days} days (ExecutionsStarted)", qty(r.executions)))
        .evidence(format!("{states} states in the definition; no .sync or callback tasks"))
        .caveat("Express runs at least once (asynchronous) or at most once (synchronous), not exactly once: tasks must tolerate it.")
        .caveat("Express executions last 5 minutes at most and keep their history only in CloudWatch Logs.")
        .caveat("The transition count assumes each state runs once per execution (choices skip some, loops repeat some)."),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEFINITION: &str = r#"{"StartAt":"A","States":{"A":{"Type":"Task","Next":"B"},"B":{"Type":"Map","ItemProcessor":{"StartAt":"C","States":{"C":{"Type":"Pass","End":true}}},"Next":"D"},"D":{"Type":"Succeed"}}}"#;

    fn machine() -> StateMachine {
        StateMachine { state_machine_arn: "arn:aws:states:us-east-1:1:stateMachine:orders".into(), name: "orders".into(), kind: "STANDARD".into(), creation_date: None }
    }

    #[test]
    fn counts_states_inside_maps_and_parallels() {
        assert_eq!(count_states(&serde_json::from_str(DEFINITION).unwrap()), 4);
        assert!(waits(r#"{"Resource":"arn:aws:states:::glue:startJobRun.sync"}"#));
        assert!(!waits(DEFINITION));
    }

    #[test]
    fn busy_short_standard_workflows() {
        let runs = Runs { executions: 2_000_000.0, mean_ms: 1800.0, max_ms: 9000.0 };
        let f = express_candidate(&machine(), &runs, DEFINITION, 14).unwrap();
        // 4.35 M executions a month × 4 states.
        assert_eq!(f.reduction, "≈ 17.4 M state transitions a month billed in Standard");
        assert_eq!(f.impact, Impact::Medium);
        assert_eq!(f.target, "arn:aws:states:us-east-1:1:stateMachine:orders");
        // Too long for Express, waiting for jobs, or too few executions: nothing.
        assert!(express_candidate(&machine(), &Runs { max_ms: 400_000.0, ..runs }, DEFINITION, 14).is_none());
        assert!(express_candidate(&machine(), &runs, r#"{"States":{"A":{"Resource":"arn:aws:states:::ecs:runTask.sync"}}}"#, 14).is_none());
        assert!(express_candidate(&machine(), &Runs { executions: 1000.0, ..runs }, DEFINITION, 14).is_none());
    }

    #[test]
    fn weights_execution_times_by_executions() {
        let s = |points: &[(f64, f64)]| Series { points: points.to_vec(), ..Default::default() };
        let r = Runs::of(&s(&[(0.0, 10.0), (1.0, 30.0)]), &s(&[(0.0, 100.0), (1.0, 500.0)]), &s(&[(0.0, 900.0), (1.0, 2000.0)]));
        assert_eq!(r, Runs { executions: 40.0, mean_ms: 400.0, max_ms: 2000.0 });
    }
}
