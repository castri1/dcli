//! Failure report: what failed in a pipeline's most recent failed execution (stage, action,
//! error, commit, CodeBuild phase) and the part of the build log worth reading, as Markdown
//! to paste into an issue, a chat or an AI agent. Agents can also run `dcli report` directly.

use std::fmt::Write as _;
use std::ops::Range;

use crate::aws::{self, Aws, cloudformation, codebuild, codepipeline as cp, logs};
use crate::pipeline::{self, ActionView, StageView, Status};
use crate::util::{first_line, fmt_ago, fmt_duration, fmt_utc, looks_like_error, md_code};

/// Recent executions searched for the last failure.
const LOOKBACK: u32 = 10;
/// Lines kept from the end of the log, where the build stopped.
const TAIL: usize = 60;
/// Lines (error lines plus context) kept from before the tail.
const ERROR_BUDGET: usize = 200;
/// Lines kept before and after an error line; details (expected values, stack traces)
/// usually follow it.
const BEFORE: usize = 3;
const AFTER: usize = 15;
/// CloudWatch pages (1,000 events each) read at most.
const MAX_PAGES: usize = 100;

pub struct Report {
    pub markdown: String,
    /// The failed execution the report is about; `None` when nothing failed recently.
    pub execution: Option<String>,
    /// Build log lines read, and how many of them the report includes.
    pub log_lines: usize,
    pub shown_lines: usize,
}

/// Report on the most recent failed execution of `pipeline` among the last few.
/// `full_log` includes the whole build log instead of an excerpt.
pub fn failure_report(aws: &Aws, env_name: &str, pipeline: &str, full_log: bool) -> aws::Result<Report> {
    let executions = cp::list_executions(aws, pipeline, LOOKBACK)?;
    let Some((index, exec)) = executions.iter().enumerate().find(|(_, e)| Status::parse(&e.status) == Status::Failed)
    else {
        let latest = executions
            .first()
            .map_or("no executions".to_string(), |l| format!("latest: {}", Status::parse(&l.status).label()));
        return Ok(Report {
            markdown: format!("No failed execution of {pipeline} ({env_name}) among the last {LOOKBACK} ({latest}).\n"),
            execution: None,
            log_lines: 0,
            shown_lines: 0,
        });
    };
    let id = &exec.pipeline_execution_id;
    let state = cp::get_pipeline_state(aws, pipeline)?;
    let details = cp::list_action_executions(aws, pipeline, Some(id), 100)?;
    let stages = pipeline::stages(&state, Some(exec), &details);
    let revision = pipeline::revision(exec, &details);

    let mut md = String::new();
    let mut log_lines = 0;
    let mut shown_lines = 0;
    let _ = writeln!(md, "## Pipeline failure: {pipeline} ({env_name})\n");
    let _ = writeln!(md, "- **Pipeline:** `{pipeline}` · environment `{env_name}` · region `{}`", aws.region);
    let started = exec.start_time.map(|t| format!(" · started {} ({})", fmt_utc(t), fmt_ago(t))).unwrap_or_default();
    let ran = exec
        .start_time
        .zip(exec.last_update_time)
        .map(|(a, b)| format!(" · ran {}", fmt_duration(b - a)))
        .unwrap_or_default();
    let _ = writeln!(md, "- **Execution:** `{id}` · Failed · trigger {}{started}{ran}", pipeline::trigger_label(exec));
    if index > 0 {
        let latest = &executions[0];
        let _ = writeln!(
            md,
            "- **Note:** not the latest execution; the latest (`{}`) is {}.",
            short(&latest.pipeline_execution_id),
            Status::parse(&latest.status).label()
        );
    }
    if let Some(summary) = exec.status_summary.as_deref().filter(|s| !s.is_empty()) {
        let _ = writeln!(md, "- **Status summary:** {}", md_code(&one_line(summary)));
    }
    if revision.commit.is_some() || revision.message.is_some() {
        // Commit messages and branch names are anyone's text: quoted, never loose Markdown.
        let mut text = revision.commit.as_deref().map(md_code).unwrap_or_default();
        if let Some(m) = &revision.message {
            let _ = write!(text, " {}", md_code(first_line(m)));
        }
        if let Some(b) = &revision.branch {
            let _ = write!(text, " · branch {}", md_code(b));
        }
        if let Some(r) = &revision.repo {
            let _ = write!(text, " · repo {}", md_code(r));
        }
        let _ = writeln!(md, "- **Commit:** {}", text.trim());
    }
    match pipeline::last_success(&executions, index) {
        Some(ok_index) => {
            let ok = &executions[ok_index];
            let ok_rev = pipeline::revision(ok, &[]);
            let when = ok.last_update_time.map(|t| format!(" · {}", fmt_ago(t))).unwrap_or_default();
            let commit = ok_rev.commit.as_deref().map(|c| format!("`{c}` ")).unwrap_or_default();
            let _ = writeln!(md, "- **Last success:** {commit}(execution `{}`{when})", short(&ok.pipeline_execution_id));
            if ok_index > index + 1 {
                let runs: Vec<String> = executions[index + 1..ok_index]
                    .iter()
                    .map(|e| {
                        let rev = pipeline::revision(e, &[]);
                        let commit = rev.commit.map(|c| format!(" `{c}`")).unwrap_or_default();
                        format!("`{}` {}{commit}", short(&e.pipeline_execution_id), Status::parse(&e.status).label())
                    })
                    .collect();
                let _ = writeln!(md, "- **Runs between:** {}", runs.join(" · "));
            }
            if let Some(url) = pipeline::compare_url(ok, exec, revision.repo.as_deref()) {
                let _ = writeln!(md, "- **Changes since the last success:** {url}");
            }
        }
        None => {
            let _ = writeln!(md, "- **Last success:** none in the last {} runs", executions.len());
        }
    }

    let failed: Vec<(&StageView, &ActionView)> = stages
        .iter()
        .flat_map(|s| s.actions.iter().filter(|a| a.status == Status::Failed).map(move |a| (s, a)))
        .collect();
    if failed.is_empty() {
        let names: Vec<&str> = stages.iter().filter(|s| s.status == Status::Failed).map(|s| s.name.as_str()).collect();
        let _ = writeln!(md, "- **Failed stage:** {}", if names.is_empty() { "unknown".into() } else { names.join(", ") });
    }

    for (stage, action) in failed {
        let provider = action.provider.as_deref().map(|p| format!(" ({p})")).unwrap_or_default();
        let _ = writeln!(md, "\n### Failed: stage `{}`, action `{}`{provider}\n", stage.name, action.name);
        if let Some(e) = &action.error {
            let _ = writeln!(md, "- **Error:** {}", md_code(&one_line(e)));
        }
        if let Some(u) = &action.url {
            let _ = writeln!(md, "- **Console:** {u}");
        }
        let detail = details.iter().find(|d| d.stage_name == stage.name && d.action_name == action.name);
        let input = detail.and_then(|d| d.input.as_ref());
        if let Some(stack) = input.and_then(|i| i.configuration.get("StackName")) {
            let regional = input.and_then(|i| i.region.as_deref()).filter(|r| *r != aws.region).map(|r| aws.in_region(r));
            let since = detail.and_then(|d| d.start_time).or(exec.start_time).unwrap_or(0.0) - 60.0;
            match cloudformation::stack_events(regional.as_deref().unwrap_or(aws), stack, since, 5) {
                Ok(events) => write_stack_events(&mut md, stack, &events),
                Err(e) if e.is_login_required() => return Err(e),
                Err(e) => {
                    let _ = writeln!(md, "- Could not read the events of stack `{stack}`: {}", e.short());
                }
            }
        }
        let Some(build_id) = &action.build_id else { continue };
        let build = match codebuild::get_build(aws, build_id) {
            Ok(b) => b,
            Err(e) if e.is_login_required() => return Err(e),
            Err(e) => {
                let _ = writeln!(md, "- **CodeBuild:** `{build_id}` (could not read it: {})", e.short());
                continue;
            }
        };
        let _ = writeln!(md, "- **CodeBuild:** `{build_id}`");
        let Some(build) = build else { continue };
        for phase in build.phases.iter().filter(|p| matches!(p.phase_status.as_deref(), Some(s) if s != "SUCCEEDED")) {
            let contexts: Vec<String> = phase
                .contexts
                .iter()
                .filter_map(|c| match (&c.status_code, &c.message) {
                    (Some(code), Some(m)) if !m.is_empty() => Some(format!("{code}: {}", one_line(m))),
                    (_, Some(m)) if !m.is_empty() => Some(one_line(m)),
                    (Some(code), _) => Some(code.clone()),
                    _ => None,
                })
                .collect();
            let status = phase.phase_status.as_deref().unwrap_or("?");
            let context = if contexts.is_empty() { String::new() } else { md_code(&contexts.join("; ")) };
            let _ = writeln!(md, "- **Phase `{}`:** {status}{}", phase.phase_type, prefixed(" — ", &context));
        }
        let Some(location) = &build.logs else { continue };
        if let Some(link) = &location.deep_link {
            let _ = writeln!(md, "- **Log in the console:** {link}");
        }
        let (Some(group), Some(stream)) = (&location.group_name, &location.stream_name) else {
            let _ = writeln!(md, "- The build has no CloudWatch log.");
            continue;
        };
        let (lines, truncated) = match logs::read_all(aws, group, stream, MAX_PAGES) {
            Ok(read) => read,
            Err(e) if e.is_login_required() => return Err(e),
            Err(e) => {
                let _ = writeln!(md, "- Could not read the build log: {}", e.short());
                continue;
            }
        };
        log_lines += lines.len();
        let whole = 0..lines.len();
        let ranges = if full_log { vec![whole] } else { excerpt(&lines, TAIL, ERROR_BUDGET) };
        let shown: usize = ranges.iter().map(|r| r.len()).sum();
        shown_lines += shown;
        let what = if shown == lines.len() {
            format!("all {} lines", lines.len())
        } else {
            format!("error lines with context and the last {TAIL}; {shown} of {} lines", lines.len())
        };
        let more = if truncated { " — the log is longer; only its beginning was read" } else { "" };
        let _ = writeln!(md, "\n**Build log** ({what}){more}:\n");
        write_log(&mut md, &lines, &ranges);
    }

    let _ = writeln!(
        md,
        "\n_Report by dcli {}. {}_",
        env!("CARGO_PKG_VERSION"),
        if full_log {
            "Full build log included.".to_string()
        } else {
            format!("Full log: `dcli report {env_name} {pipeline} --full`.")
        }
    );
    Ok(Report { markdown: md, execution: Some(id.clone()), log_lines, shown_lines })
}

/// The failed resources of a stack operation, oldest first: usually the first one is the
/// cause and the rest are rollbacks or cancellations.
fn write_stack_events(md: &mut String, stack: &str, events: &[cloudformation::StackEvent]) {
    let failed: Vec<&cloudformation::StackEvent> = events.iter().rev().filter(|e| e.failed()).take(15).collect();
    if failed.is_empty() {
        let _ = writeln!(md, "- **Stack `{stack}`:** no failed resource events during this deploy");
        return;
    }
    let _ = writeln!(md, "- **Stack `{stack}` — failed resources** (oldest first):");
    for e in failed {
        let reason = e.reason.as_deref().map(|r| format!(": {}", md_code(&one_line(r)))).unwrap_or_default();
        let _ = writeln!(md, "  - `{}` ({}) {}{reason}", e.logical_id, e.resource_type, e.status);
    }
}

fn short(id: &str) -> &str {
    &id[..8.min(id.len())]
}

fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn prefixed(prefix: &str, text: &str) -> String {
    if text.is_empty() { String::new() } else { format!("{prefix}{text}") }
}

/// The lines worth reading in a long log: error-looking lines with a little context, earliest
/// first (the first error is usually the cause) up to `budget` lines, then the last `tail`
/// lines, where the build stopped. Ranges are in order and do not overlap.
pub fn excerpt(lines: &[String], tail: usize, budget: usize) -> Vec<Range<usize>> {
    let n = lines.len();
    let tail_start = n.saturating_sub(tail);
    let mut ranges: Vec<Range<usize>> = Vec::new();
    let mut used = 0;
    for (i, line) in lines[..tail_start].iter().enumerate() {
        if used >= budget {
            break;
        }
        if !looks_like_error(line) {
            continue;
        }
        let start = i.saturating_sub(BEFORE);
        let end = (i + AFTER + 1).min(tail_start);
        match ranges.last_mut() {
            Some(last) if start <= last.end => {
                used += end.saturating_sub(last.end);
                last.end = last.end.max(end);
            }
            _ => {
                used += end - start;
                ranges.push(start..end);
            }
        }
    }
    match ranges.last_mut() {
        Some(last) if last.end >= tail_start => last.end = n,
        _ if tail_start < n => ranges.push(tail_start..n),
        _ => {}
    }
    ranges
}

/// The selected lines in a code block, with a marker where lines were skipped. The fence is
/// longer than any run of backticks in the log, so the log cannot close it.
fn write_log(md: &mut String, lines: &[String], ranges: &[Range<usize>]) {
    let longest_ticks = lines
        .iter()
        .flat_map(|l| l.split(|c| c != '`').map(str::len))
        .max()
        .unwrap_or(0);
    let fence = "`".repeat(longest_ticks.max(2) + 1);
    let _ = writeln!(md, "{fence}text");
    let mut next = 0;
    for r in ranges {
        if r.start > next {
            let _ = writeln!(md, "[… {} lines omitted …]", r.start - next);
        }
        for line in &lines[r.clone()] {
            let _ = writeln!(md, "{line}");
        }
        next = r.end;
    }
    if next < lines.len() {
        let _ = writeln!(md, "[… {} lines omitted …]", lines.len() - next);
    }
    let _ = writeln!(md, "{fence}");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn log(n: usize, errors: &[usize]) -> Vec<String> {
        (0..n).map(|i| if errors.contains(&i) { format!("Error: boom {i}") } else { format!("line {i}") }).collect()
    }

    #[test]
    fn excerpt_keeps_errors_with_context_and_the_tail() {
        let lines = log(1000, &[100, 102, 500]);
        let ranges = excerpt(&lines, 50, 150);
        assert_eq!(ranges, vec![97..118, 497..516, 950..1000]);

        // Short logs are kept whole.
        assert_eq!(excerpt(&log(30, &[3]), 50, 150), vec![0..30]);
        // An error just before the tail joins it.
        assert_eq!(excerpt(&log(200, &[148]), 50, 150), vec![145..200]);
        // The budget bounds the error lines.
        let many: Vec<usize> = (0..900).step_by(20).collect();
        let ranges = excerpt(&log(1000, &many), 50, 30);
        assert!(ranges.iter().map(|r| r.len()).sum::<usize>() <= 30 + 19 + 50);
    }

    #[test]
    fn benign_lines_are_not_errors() {
        assert!(looks_like_error("npm ERR! code ELIFECYCLE"));
        assert!(looks_like_error("FAIL src/queue.test.ts"));
        assert!(!looks_like_error("Tests: 0 failed, 57 passed"));
        assert!(!looks_like_error("Compiled with 0 errors"));
    }

    #[test]
    fn fence_outlasts_backticks_in_the_log() {
        let lines = vec!["```".to_string(), "a ````` b".to_string()];
        let mut md = String::new();
        let all = 0..lines.len();
        write_log(&mut md, &lines, &[all]);
        assert!(md.starts_with("``````text\n"), "{md}");
        assert!(md.ends_with("\n``````\n"));
    }
}
