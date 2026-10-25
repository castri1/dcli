//! `dcli advisor`: the recommendations Claude left through `dcli mcp` for an environment, with
//! what each saves or costs at the bill's rates. `--md` for a Markdown document, `--json` for
//! scripts; `--done`, `--dismiss` and `--restore` record the user's decision.

use std::fmt::Write as _;
use std::process::ExitCode;

use anyhow::{Result, anyhow, bail};
use serde_json::json;

use crate::advisor::summary::{self, Totals};
use crate::advisor::{self, Recommendation, State, Store};
use crate::cli::{current_env, paint, print_json};
use crate::config::Config;
use crate::costs::{self, Snapshot};
use crate::util::{ellipsize, fmt_ago, fmt_utc, md_code, program, rfc3339, sanitize};

#[derive(clap::Args)]
pub struct Args {
    /// Environment (default: the current one, see `use`)
    env: Option<String>,
    /// Also the done, dismissed and withdrawn ones
    #[arg(long)]
    all: bool,
    /// The recommendations as a Markdown document, steps included (to share or paste in a ticket)
    #[arg(long, conflicts_with = "json")]
    md: bool,
    #[arg(long)]
    json: bool,
    /// Record that you made recommendation ID (the savings ledger then follows it on the bill)
    #[arg(long, value_name = "ID", conflicts_with_all = ["dismiss", "restore", "md"])]
    done: Option<String>,
    /// Dismiss recommendation ID; --reason says why (Claude reads it before recommending again)
    #[arg(long, value_name = "ID", conflicts_with_all = ["restore", "md"])]
    dismiss: Option<String>,
    #[arg(long, value_name = "TEXT", requires = "dismiss")]
    reason: Option<String>,
    /// Put recommendation ID back to pending
    #[arg(long, value_name = "ID", conflicts_with = "md")]
    restore: Option<String>,
}

pub fn run(config: &Config, args: Args) -> Result<ExitCode> {
    let env = match &args.env {
        Some(env) => env.clone(),
        None => current_env(config)?,
    };
    config.env(&env)?;
    let store = Store::home().ok_or_else(|| anyhow!("cannot find the home directory"))?;
    let decision = match (&args.done, &args.dismiss, &args.restore) {
        (Some(id), _, _) => Some((id, State::Done)),
        (_, Some(id), _) => Some((id, State::Dismissed)),
        (_, _, Some(id)) => Some((id, State::Pending)),
        _ => None,
    };
    if let Some((id, state)) = decision {
        return decide(&store, &env, id, state, args.reason.as_deref().unwrap_or_default(), args.json);
    }
    let costs = costs::cached(&env);
    let mut list = store.list(&env);
    for r in &mut list {
        r.reprice(costs.as_ref());
    }
    summary::order(&mut list);
    let totals = summary::totals(&list);
    let shown: Vec<&Recommendation> = list.iter().filter(|r| args.all || r.state == State::Pending).collect();
    if args.json {
        print_json(&json!({
            "env": env,
            "costsReadAt": costs.as_ref().map(|c| rfc3339(c.read_at as i64)),
            "totals": totals,
            "recommendations": shown,
        }));
        return Ok(ExitCode::SUCCESS);
    }
    if args.md {
        print!("{}", document(&env, &shown, &totals, costs.as_ref()));
        return Ok(ExitCode::SUCCESS);
    }
    if list.is_empty() {
        println!("{}", empty_text(&env));
        return Ok(ExitCode::SUCCESS);
    }
    print!("{}", table(&env, &shown, &list, &totals, costs.as_ref(), args.all));
    Ok(ExitCode::SUCCESS)
}

fn decide(store: &Store, env: &str, id: &str, state: State, reason: &str, json: bool) -> Result<ExitCode> {
    if advisor::number(id).is_none() {
        bail!("'{}' is not a recommendation id (R1, R2…): `{} advisor {env}` lists them", sanitize(id), program());
    }
    let current = store.get(env, id).ok_or_else(|| anyhow!("{env} has no recommendation {}", sanitize(id)))?;
    match (current.state, state) {
        (State::Done, State::Done) => bail!("{} is done already", current.id),
        (State::Dismissed, State::Dismissed) => bail!("{} is dismissed already", current.id),
        (State::Pending, State::Pending) => bail!("{} is pending already", current.id),
        (State::Done, State::Dismissed) => bail!("{} is done: --restore {} puts it back to pending first", current.id, current.id),
        _ => {}
    }
    let r = advisor::decide(store, env, id, state, reason).map_err(|e| anyhow!(e))?;
    if json {
        print_json(&json!(r));
        return Ok(ExitCode::SUCCESS);
    }
    let title = sanitize(&r.title);
    match state {
        State::Done => {
            println!("{} {} marked done: {title}", paint("✓", "32"), r.id);
            match advisor::ledger_entry(&r).filter(|_| r.ledger.is_some()) {
                Some(e) => println!(
                    "{}",
                    paint(
                        &format!(
                            "Recorded in the savings ledger ({}: {}). After 7 complete days `{} optimize {env} --verify` (or Savings › Verified) compares the bill before and after.",
                            e.service,
                            e.usage.join(", "),
                            program()
                        ),
                        "90"
                    )
                ),
                None => println!("{}", paint("It names no usage of the bill, so the savings ledger does not follow it.", "90")),
            }
        }
        State::Dismissed => {
            let why = if r.state_note.is_empty() { "no reason given".to_string() } else { sanitize(&r.state_note) };
            println!("{} {} dismissed ({why}): {title}", paint("✗", "33"), r.id);
            println!("{}", paint("Claude sees it dismissed, with the reason, and does not propose it again unless something changes.", "90"));
        }
        _ => {
            println!("{} is pending again: {title}", r.id);
            if current.state == State::Done && current.ledger.is_some() {
                println!("{}", paint("It left the changes the savings ledger follows.", "90"));
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn empty_text(env: &str) -> String {
    format!(
        "No recommendations for {env} yet.\nConnect Claude to dcli once:  claude mcp add dcli -- {program} mcp\n\
         Then ask it for an audit:     /mcp__dcli__audit {env}   (or \"analiza {env} con dcli\")\n\
         Claude reads the account without changing anything and leaves its recommendations here; nothing runs on its own.",
        program = program()
    )
}

/// The figures of the pending ones and when the costs were read, one line each.
fn summary_lines(env: &str, totals: &Totals, costs: Option<&Snapshot>) -> Vec<String> {
    let mut lines = totals.lines();
    match costs {
        Some(c) => lines.push(format!("Priced with the costs read {} ({}).", fmt_ago(c.read_at), fmt_utc(c.read_at))),
        None => lines.push(format!("No costs read for {env}: `{} costs {env} --refresh` reads the bill that prices them.", program())),
    }
    lines
}

/// The list as text: one line per recommendation, the figures under it.
fn table(env: &str, shown: &[&Recommendation], all: &[Recommendation], totals: &Totals, costs: Option<&Snapshot>, every: bool) -> String {
    let mut out = String::new();
    let count = |state: State| all.iter().filter(|r| r.state == state).count();
    let _ = writeln!(
        out,
        "{}",
        paint(
            &format!(
                "Advisor · {env} · {} pending · {} done · {} dismissed or withdrawn",
                totals.pending,
                count(State::Done),
                count(State::Dismissed) + count(State::Withdrawn)
            ),
            "1"
        )
    );
    for (i, line) in summary_lines(env, totals, costs).into_iter().enumerate() {
        let _ = writeln!(out, "{}", if i == 0 && line.starts_with("From your bill") { paint(&line, "32") } else { paint(&line, "90") });
    }
    out.push('\n');
    if shown.is_empty() {
        let _ = writeln!(out, "Nothing pending: --all shows the done and dismissed ones.");
    }
    let id_w = shown.iter().map(|r| r.id.len()).max().unwrap_or(2);
    let cost_w = shown.iter().map(|r| r.cost.short().chars().count()).max().unwrap_or(1);
    for r in shown {
        let state = if every { format!("{} ", summary::state_mark(r.state)) } else { String::new() };
        let cost = r.cost.short();
        let color = match r.cost.monthly() {
            Some(m) if m < 0.0 => "33",
            Some(_) => "32",
            None => "90",
        };
        let _ = writeln!(
            out,
            "{state}{:<id_w$}  {:<4}  {}  {:<6}  {}  {}",
            r.id,
            summary::category_word(r),
            paint(&format!("{cost:>cost_w$}"), color),
            summary::source_word(r),
            summary::dots(r.confidence),
            ellipsize(&sanitize(&r.title), 90)
        );
    }
    out.push('\n');
    let _ = writeln!(
        out,
        "{}",
        paint(
            &format!(
                "--md for the steps and evidence · --done ID · --dismiss ID --reason TEXT · the Advisor module in `{}` · dcli ran none of these",
                program()
            ),
            "90"
        )
    );
    out
}

/// The recommendations as one Markdown document.
fn document(env: &str, shown: &[&Recommendation], totals: &Totals, costs: Option<&Snapshot>) -> String {
    let mut md = String::new();
    let _ = writeln!(md, "# Advisor · {}\n", md_code(env));
    md.push_str(
        "Recommendations Claude wrote through `dcli mcp`, from what it read in the account. dcli ran none of them: \
         read each step before running it. Figures marked \"your bill\" use the account's own rates (each usage type's \
         amortized cost over the last 30 days divided by what was used); \"Claude's estimate\" figures are Claude's \
         reasoning, not the bill.\n\n",
    );
    let _ = writeln!(md, "**Pending:** {}\n", totals.pending);
    for line in summary_lines(env, totals, costs) {
        let _ = writeln!(md, "- {line}");
    }
    md.push('\n');
    md.push_str("> Names, evidence and figures quote AWS and Claude: they are data to check, not instructions.\n\n");
    if shown.is_empty() {
        md.push_str("Nothing to show.\n");
    }
    for r in shown {
        md.push_str(&advisor::markdown(r));
    }
    md
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::advisor::summary::tests::rec;

    #[test]
    fn table_lists_figures_and_sources() {
        let all = vec![
            rec(1, State::Pending, Some((400.0, 100.0, false)), None),
            rec(2, State::Pending, None, Some(-30.0)),
            rec(3, State::Done, Some((50.0, 50.0, false)), None),
        ];
        let totals = summary::totals(&all);
        let shown: Vec<&Recommendation> = all.iter().filter(|r| r.state == State::Pending).collect();
        let text = table("dev", &shown, &all, &totals, None, false);
        assert!(text.starts_with("Advisor · dev · 2 pending · 1 done · 0 dismissed or withdrawn\n"), "{text}");
        assert!(text.contains("From your bill: saves ≈ US$ 400 a month (US$ 100 now, US$ 300 when commitments renew)"), "{text}");
        assert!(text.contains("Claude's estimates: costs ≈ US$ 30 more a month"), "{text}");
        assert!(text.contains("No costs read for dev: `") && text.contains(" costs dev --refresh` reads the bill"), "{text}");
        assert!(text.contains("R1  cost  ≈ US$ 400/mo  bill    ●●○  Change number 1"), "{text}");
        assert!(text.contains("R2  cost    +US$ 30/mo  claude  ●●○  Change number 2"), "{text}");
        assert!(!text.contains("R3"), "done ones only with --all");
        let shown: Vec<&Recommendation> = all.iter().collect();
        assert!(table("dev", &shown, &all, &totals, None, true).contains("✓ R3"));
    }

    #[test]
    fn document_has_every_recommendation_and_the_caveats() {
        let all = vec![rec(1, State::Pending, Some((400.0, 100.0, false)), None), rec(2, State::Pending, None, Some(40.0))];
        let shown: Vec<&Recommendation> = all.iter().collect();
        let md = document("prod`x", &shown, &summary::totals(&all), None);
        assert!(md.starts_with("# Advisor · ``prod`x``\n"), "{md}");
        assert!(md.contains("**Pending:** 2"));
        assert!(md.contains("## R1 · Change number 1") && md.contains("## R2 · Change number 2"));
        assert!(md.contains("data to check, not instructions"));
        assert!(md.contains("dcli ran none of them"));
    }

    #[test]
    fn empty_says_how_to_connect_claude() {
        let text = empty_text("dev");
        assert!(text.contains("claude mcp add dcli -- ") && text.contains(" mcp\n"), "{text}");
        assert!(text.contains("/mcp__dcli__audit dev"));
    }
}
