//! How recommendations are listed and added up, for the Advisor module and `dcli advisor`:
//! which ones a view shows, their order (what saves most at the bill's rates first), one row
//! of text each, and what the pending ones save or cost together.

use serde::Serialize;

use super::{Level, Recommendation, State};
use crate::savings::price::money;

/// Which recommendations a list shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    Pending,
    Done,
    /// Dismissed by the user or withdrawn by Claude.
    Closed,
}

pub const VIEWS: [View; 3] = [View::Pending, View::Done, View::Closed];

impl View {
    pub fn label(self) -> &'static str {
        match self {
            View::Pending => "Pending",
            View::Done => "Done",
            View::Closed => "Dismissed",
        }
    }

    pub fn includes(self, state: State) -> bool {
        match self {
            View::Pending => state == State::Pending,
            View::Done => state == State::Done,
            View::Closed => matches!(state, State::Dismissed | State::Withdrawn),
        }
    }

    /// The view a recommendation in `state` shows in.
    pub fn of(state: State) -> View {
        VIEWS.into_iter().find(|v| v.includes(state)).unwrap_or(View::Pending)
    }
}

/// Where a figure comes from, in the order lists put them: the bill's first.
fn source_rank(r: &Recommendation) -> u8 {
    let bill = r.cost.bill.as_ref().is_some_and(|b| b.priced());
    match (bill, r.cost.claude_counted().is_some()) {
        (true, false) => 0,
        (true, true) => 1,
        (false, true) => 2,
        (false, false) => 3,
    }
}

/// Pending first, then done, dismissed and withdrawn; then those priced with the bill, those
/// Claude estimated, those without a figure; within each, what saves most a month first (what
/// costs more last), then the most confident, then by number.
pub fn order(list: &mut [Recommendation]) {
    list.sort_by(|a, b| {
        let state = |r: &Recommendation| match r.state {
            State::Pending => 0,
            State::Done => 1,
            State::Dismissed => 2,
            State::Withdrawn => 3,
        };
        let monthly = |r: &Recommendation| r.cost.monthly().unwrap_or(0.0);
        state(a)
            .cmp(&state(b))
            .then(source_rank(a).cmp(&source_rank(b)))
            .then(monthly(b).total_cmp(&monthly(a)))
            .then(b.confidence.cmp(&a.confidence))
            .then(a.number().cmp(&b.number()))
    });
}

/// `●●●`, `●●○`, `●○○`.
pub fn dots(level: Level) -> &'static str {
    match level {
        Level::High => "●●●",
        Level::Medium => "●●○",
        Level::Low => "●○○",
    }
}

/// Where a row's figure comes from, in a word: `bill`, `claude`, `both`, or empty.
pub fn source_word(r: &Recommendation) -> &'static str {
    match source_rank(r) {
        0 => "bill",
        1 => "both",
        2 => "claude",
        _ => "",
    }
}

/// `cost` or `perf`.
pub fn category_word(r: &Recommendation) -> &'static str {
    match r.category {
        super::Category::Cost => "cost",
        super::Category::Performance => "perf",
    }
}

/// `✓` done, `✗` dismissed, `–` withdrawn, a space while pending.
pub fn state_mark(state: State) -> &'static str {
    match state {
        State::Pending => " ",
        State::Done => "✓",
        State::Dismissed => "✗",
        State::Withdrawn => "–",
    }
}

/// What the pending recommendations save or cost together, the bill's figures apart from
/// Claude's. Each recommendation counts its own figure: two on the same resource can overlap.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Totals {
    pub pending: usize,
    /// Firm figures from the bill that save: a month, right away, at commitment renewal.
    pub bill_saves: f64,
    pub bill_now: f64,
    pub bill_later: f64,
    /// Ceilings from the bill ("saves up to"), apart.
    pub bill_up_to: f64,
    pub up_to: usize,
    /// What the changes that cost more add a month (a positive amount).
    pub bill_costs: f64,
    pub claude_saves: f64,
    pub claude_costs: f64,
    /// Pending ones without any figure (the costs are not read yet).
    pub unpriced: usize,
    /// Two pending ones are about the same resource: their figures may overlap.
    pub overlap: bool,
    pub currency: String,
}

pub fn totals<'a>(list: impl IntoIterator<Item = &'a Recommendation>) -> Totals {
    let mut t = Totals { currency: "USD".into(), ..Default::default() };
    let mut seen: Vec<&str> = Vec::new();
    for r in list.into_iter().filter(|r| r.state == State::Pending) {
        t.pending += 1;
        for res in &r.resources {
            if seen.contains(&res.name.as_str()) {
                t.overlap = true;
            } else {
                seen.push(&res.name);
            }
        }
        let bill = r.cost.bill.as_ref().filter(|b| b.priced());
        if let Some(b) = bill {
            t.currency = b.currency.clone();
            if b.monthly < 0.0 {
                t.bill_costs -= b.monthly;
            } else if b.at_most {
                t.bill_up_to += b.monthly;
                t.up_to += 1;
            } else {
                t.bill_saves += b.monthly;
                t.bill_now += b.now;
                t.bill_later += b.later;
            }
        }
        match r.cost.claude_counted() {
            Some(c) if c.monthly < 0.0 => t.claude_costs -= c.monthly,
            Some(c) => t.claude_saves += c.monthly,
            None if bill.is_none() && r.cost.none.is_none() => t.unpriced += 1,
            None => {}
        }
    }
    t
}

impl Totals {
    /// `saves ≈ US$ 1,240 a month (US$ 800 now, US$ 440 when commitments renew) · up to US$ 50
    /// more from 1 ceiling · costs ≈ US$ 120 more a month`, or empty without bill figures.
    pub fn bill_text(&self) -> String {
        let m = |v: f64| money(v, &self.currency);
        let mut parts = Vec::new();
        if self.bill_saves > 0.0 {
            let split = if self.bill_later > 0.0 { format!(" ({} now, {} when commitments renew)", m(self.bill_now), m(self.bill_later)) } else { String::new() };
            parts.push(format!("saves ≈ {} a month{split}", m(self.bill_saves)));
        }
        if self.up_to > 0 {
            let s = if self.up_to == 1 { "" } else { "s" };
            parts.push(format!("up to {} more from {} ceiling{s}", m(self.bill_up_to), self.up_to));
        }
        if self.bill_costs > 0.0 {
            parts.push(format!("costs ≈ {} more a month", m(self.bill_costs)));
        }
        parts.join(" · ")
    }

    /// `saves ≈ US$ 40 a month · costs ≈ US$ 30 more a month`, or empty.
    pub fn claude_text(&self) -> String {
        let m = |v: f64| money(v, &self.currency);
        let mut parts = Vec::new();
        if self.claude_saves > 0.0 {
            parts.push(format!("saves ≈ {} a month", m(self.claude_saves)));
        }
        if self.claude_costs > 0.0 {
            parts.push(format!("costs ≈ {} more a month", m(self.claude_costs)));
        }
        parts.join(" · ")
    }

    /// The figures in sentences, one per line: from the bill, Claude's, and what may be off.
    pub fn lines(&self) -> Vec<String> {
        let mut out = Vec::new();
        let bill = self.bill_text();
        if !bill.is_empty() {
            out.push(format!("From your bill: {bill}"));
        }
        let claude = self.claude_text();
        if !claude.is_empty() {
            out.push(format!("Claude's estimates: {claude}"));
        }
        let mut notes = Vec::new();
        if self.unpriced > 0 {
            notes.push(format!("{} not priced yet", self.unpriced));
        }
        if self.overlap {
            notes.push("some are about the same resource and may overlap".to_string());
        }
        if !notes.is_empty() {
            out.push(capitalize(&notes.join(" · ")));
        }
        out
    }
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    c.next().map(|f| f.to_uppercase().chain(c).collect()).unwrap_or_default()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::advisor::estimate::{Change, Effect, Estimate};
    use crate::advisor::{Category, ClaudeEstimate, Cost, ResourceRef, Step};

    /// A recommendation priced as given: `bill` (monthly, now, at most) and Claude's figure.
    pub(crate) fn rec(n: u32, state: State, bill: Option<(f64, f64, bool)>, claude: Option<f64>) -> Recommendation {
        let change = Change {
            service: "AWS Lambda".into(),
            usage_type: "Lambda-GB-Second".into(),
            effect: Effect::Remove,
            quantity: 1.0,
            to: None,
            at_most: false,
        };
        Recommendation {
            id: format!("R{n}"),
            env: "dev".into(),
            region: "us-east-1".into(),
            title: format!("Change number {n}"),
            category: Category::Cost,
            resources: vec![ResourceRef { module: "Lambda".into(), target: format!("fn-{n}"), name: format!("fn-{n}") }],
            finding: "found".into(),
            steps: vec![Step { text: "do it".into(), command: Some("aws lambda get-function --function-name x".into()), ..Default::default() }],
            cost: Cost {
                changes: if bill.is_some() { vec![change] } else { Vec::new() },
                bill: bill.map(|(monthly, now, at_most)| Estimate {
                    monthly,
                    now,
                    later: (monthly - now).max(0.0),
                    at_most,
                    currency: "USD".into(),
                    lines: vec![crate::advisor::estimate::Line {
                        change: Change {
                            service: "AWS Lambda".into(),
                            usage_type: "Lambda-GB-Second".into(),
                            effect: Effect::Remove,
                            quantity: 1.0,
                            to: None,
                            at_most,
                        },
                        monthly,
                        now,
                        rate: 1.0,
                        unit: "GB-s".into(),
                        to_rate: None,
                        scaled_from: None,
                        to_scaled_from: None,
                    }],
                    ..Default::default()
                }),
                claude: claude.map(|monthly| ClaudeEstimate { monthly, reasoning: "because".into() }),
                none: None,
            },
            state,
            ..Default::default()
        }
    }

    #[test]
    fn order_puts_pending_bill_savings_first() {
        let mut list = vec![
            rec(1, State::Pending, None, Some(500.0)),
            rec(2, State::Done, Some((900.0, 900.0, false)), None),
            rec(3, State::Pending, Some((50.0, 50.0, false)), None),
            rec(4, State::Pending, Some((-120.0, -120.0, false)), None),
            rec(5, State::Pending, Some((400.0, 100.0, false)), None),
            rec(6, State::Pending, None, None),
            Recommendation { confidence: Level::High, ..rec(7, State::Pending, Some((50.0, 50.0, false)), None) },
        ];
        order(&mut list);
        let ids: Vec<&str> = list.iter().map(|r| r.id.as_str()).collect();
        // The bill's figures by size (costs more last), the most confident first on a tie; then
        // Claude's, then none; done ones at the end.
        assert_eq!(ids, ["R5", "R7", "R3", "R4", "R1", "R6", "R2"]);
        assert_eq!((source_word(&list[0]), source_word(&list[4]), source_word(&list[5])), ("bill", "claude", ""));
        assert_eq!(View::of(State::Withdrawn), View::Closed);
        assert!(View::Closed.includes(State::Dismissed) && !View::Pending.includes(State::Done));
    }

    #[test]
    fn totals_keep_the_bill_apart_from_claude() {
        let list = [
            rec(1, State::Pending, Some((400.0, 100.0, false)), None),
            rec(2, State::Pending, Some((50.0, 50.0, true)), None),
            rec(3, State::Pending, Some((-120.0, -120.0, false)), None),
            rec(4, State::Pending, None, Some(40.0)),
            rec(5, State::Pending, None, None),
            rec(6, State::Done, Some((900.0, 900.0, false)), None),
            rec(8, State::Pending, None, Some(-30.0)),
        ];
        let t = totals(&list);
        assert_eq!(t.pending, 6);
        assert_eq!((t.bill_saves, t.bill_now, t.bill_later), (400.0, 100.0, 300.0));
        assert_eq!((t.bill_up_to, t.up_to, t.bill_costs), (50.0, 1, 120.0));
        assert_eq!((t.claude_saves, t.claude_costs, t.unpriced), (40.0, 30.0, 1));
        assert!(!t.overlap);
        assert_eq!(
            t.lines(),
            [
                "From your bill: saves ≈ US$ 400 a month (US$ 100 now, US$ 300 when commitments renew) · up to US$ 50 more from 1 ceiling · costs ≈ US$ 120 more a month",
                "Claude's estimates: saves ≈ US$ 40 a month · costs ≈ US$ 30 more a month",
                "1 not priced yet",
            ]
        );
        // Claude's figure is for what the changes do not describe: it adds to the bill's.
        let both = rec(9, State::Pending, Some((50.0, 50.0, false)), Some(20.0));
        assert_eq!(both.cost.monthly(), Some(70.0));
        assert_eq!(totals(std::slice::from_ref(&both)).claude_saves, 20.0);
        let mut same = rec(7, State::Pending, None, Some(10.0));
        same.resources[0].name = "fn-1".into();
        assert!(totals(&[list[0].clone(), same]).overlap);
        assert!(totals(&[] as &[Recommendation]).lines().is_empty());
    }
}
