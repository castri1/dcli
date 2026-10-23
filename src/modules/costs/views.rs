//! Drawing the Costs module: the header with the totals, the services table with their usage
//! types, a service's daily cost against the deploys, and the commitments.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Style, Stylize};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{List, ListItem, Paragraph, Wrap};

use super::{Costs, Dialog, Row, Tab};
use crate::costs::analysis::{Deploy, change, money, rate};
use crate::costs::explorer;
use crate::costs::{ServiceCost, Snapshot, UsageCost, services, settings};
use crate::savings::qty;
use crate::tui::chart::{Agg, Chart, columns, fmt_count, sparkline};
use crate::tui::module::View;
use crate::tui::{theme, widgets};
use crate::util::{ellipsize, fmt_ago, now_secs, utc_parts};

const DAY: f64 = 86_400.0;
const MONTHS: [&str; 12] = ["January", "February", "March", "April", "May", "June", "July", "August", "September", "October", "November", "December"];

fn month_name(t: f64) -> &'static str {
    MONTHS[(utc_parts(t as i64).1 as usize).clamp(1, 12) - 1]
}

pub fn render(m: &mut Costs, f: &mut Frame, area: Rect, view: &View) {
    let [head, body] = area.layout(&Layout::vertical([Constraint::Length(3), Constraint::Fill(1)]));
    f.render_widget(Paragraph::new(header(m, view, head.width as usize)), head);
    match m.tab {
        _ if m.snapshot.is_none() => empty(m, f, body),
        Tab::Services => services_tab(m, f, body),
        Tab::Commitments => commitments_tab(m, f, body),
    }
    match &m.dialog {
        Some(Dialog::Setup(form) | Dialog::Read(form)) => form.render(f, area, theme::ACCENT),
        None => {}
    }
}

fn header(m: &Costs, view: &View, width: usize) -> Vec<Line<'static>> {
    let first_line = |short: bool| {
        let mut first = vec![Span::styled(format!(" {}", m.env), theme::bold())];
        match &m.snapshot {
            Some(s) => {
                first.push(Span::raw(format!(" · account {}", s.account)));
                if !m.shared_with.is_empty() {
                    first.push(Span::styled(format!(" (also {})", m.shared_with.join(", ")), theme::muted()));
                }
                // `the management account (profile billing)` → `profile billing` when narrow.
                let reader = match s.reader.split_once("(profile ") {
                    Some((_, p)) if short => format!("profile {}", p.trim_end_matches(')')),
                    None if short => "its own profile".to_string(),
                    _ => s.reader.clone(),
                };
                first.push(Span::styled(format!(" · amortized · read {} by {reader}", fmt_ago(s.read_at)), theme::muted()));
            }
            None => first.push(Span::styled(" · not read yet", theme::muted())),
        }
        if let Some(step) = &m.reading {
            first.push(Span::styled(format!("   {} reading: {step}…", theme::spinner(view.tick)), theme::accent()));
        }
        Line::from(first)
    };
    let first = Some(first_line(false)).filter(|l| l.width() <= width).unwrap_or_else(|| first_line(true));
    let totals = match &m.snapshot {
        // Without the comparison's detail when the full line would not fit.
        Some(s) => Some(totals(s, true)).filter(|l| l.width() <= width).unwrap_or_else(|| totals(s, false)),
        None => Line::raw(""),
    };
    let tab = |t: Tab, label: &str| {
        if m.tab == t { Span::styled(format!(" {label} "), theme::selected()) } else { Span::styled(format!(" {label} "), theme::muted()) }
    };
    let tabs = Line::from(vec![
        Span::raw(" "),
        tab(Tab::Services, "Services"),
        Span::raw(" "),
        tab(Tab::Commitments, "Commitments"),
        Span::styled(format!("    Cost Explorer: {} requests this month, each billed by AWS", settings::requests_this_month()), theme::muted()),
    ]);
    vec![first, totals, tabs]
}

/// `October to the 23rd  US$ 4,812  ▲ 12 % vs the same days of September (US$ 4,296) ·
/// September US$ 5,950 · forecast US$ 6,480`; without the part in brackets unless `full`.
fn totals(s: &Snapshot, full: bool) -> Line<'static> {
    let c = s.currency.as_str();
    let (_, _, day, ..) = utc_parts(s.read_at as i64);
    let last = s.read_at - f64::from(day) * DAY;
    let mut spans = vec![
        Span::styled(format!(" {} to the {}  ", month_name(s.read_at), ordinal(day)), theme::muted()),
        Span::styled(money(s.month_to_date, c), theme::bold()),
    ];
    // Complete days on both sides: today is still being counted (nothing to compare on the 1st).
    if let Some(ch) = change(s.month_to_date_complete, s.last_month_to_date) {
        let color = if s.month_to_date_complete > s.last_month_to_date * 1.05 { theme::WARN } else { theme::OK };
        spans.push(Span::styled(format!("  {ch}"), Style::new().fg(color)));
        if full {
            spans.push(Span::styled(
                format!(" to yesterday vs the same days of {} ({})", month_name(last), money(s.last_month_to_date, c)),
                theme::muted(),
            ));
        }
    }
    spans.push(Span::styled(format!(" · {} ", month_name(last)), theme::muted()));
    spans.push(Span::raw(money(s.last_month, c)));
    if let Some(fc) = s.forecast {
        spans.push(Span::styled(" · forecast ", theme::muted()));
        spans.push(Span::raw(money(fc, c)));
    }
    Line::from(spans)
}

fn ordinal(d: u32) -> String {
    let suffix = match (d % 10, d % 100) {
        (1, n) if n != 11 => "st",
        (2, n) if n != 12 => "nd",
        (3, n) if n != 13 => "rd",
        _ => "th",
    };
    format!("{d}{suffix}")
}

fn empty(m: &Costs, f: &mut Frame, area: Rect) {
    let mut lines = vec![Line::raw("")];
    if let Some(e) = &m.error {
        lines.push(Line::styled(format!("Could not read the costs: {}", e.short()), Style::new().fg(theme::ERR)));
        lines.push(Line::raw(""));
    }
    if m.reading.is_none() {
        lines.push(Line::raw(format!("The costs of {} have not been read yet.", m.env)));
        lines.push(Line::raw(""));
        lines.push(Line::from(vec![
            Span::styled("r", theme::key()),
            Span::raw(format!(" reads them from Cost Explorer: {}, which AWS bills. What is read is kept.", explorer::requests_text(&m.env))),
        ]));
        lines.push(Line::from(vec![
            Span::styled("s", theme::key()),
            Span::raw(" chooses who reads them: with AWS Organizations, a profile in the management account reads every account."),
        ]));
    }
    let block = widgets::panel(" Costs ", true);
    f.render_widget(Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false }).block(block), area);
}

// ---- services --------------------------------------------------------------------------------

fn services_tab(m: &mut Costs, f: &mut Frame, area: Rect) {
    let (table, detail) = if area.width >= 130 {
        let [a, b] = area.layout(&Layout::horizontal([Constraint::Percentage(56), Constraint::Fill(1)]));
        (a, b)
    } else {
        let [a, b] = area.layout(&Layout::vertical([Constraint::Fill(1), Constraint::Length((area.height / 2).clamp(8, 24))]));
        (a, b)
    };
    services_table(m, f, table);
    let Some(s) = m.snapshot.as_ref() else { return };
    // Borders and the panel's padding.
    let width = detail.width.saturating_sub(4) as usize;
    let (title, lines) = match (m.selected_usage(), m.selected_service()) {
        (Some(u), Some(svc)) => (format!(" {} ", u.usage_type), usage_detail(s, svc, u)),
        (None, Some(svc)) => (format!(" {} ", svc.service), service_detail(m, s, svc, width)),
        _ => (" Service ".to_string(), Vec::new()),
    };
    let block = widgets::panel(Line::from(title).bold(), false);
    f.render_widget(Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false }).scroll((m.detail_scroll, 0)).block(block), detail);
}

fn services_table(m: &mut Costs, f: &mut Frame, area: Rect) {
    m.list_area = area;
    let Some(s) = m.snapshot.as_ref() else { return };
    let c = s.currency.as_str();
    let inner = area.width.saturating_sub(4) as usize;
    let (money_w, change_w, module_w) = (11, 7, 14);
    let spark_w = (inner / 5).clamp(10, 30);
    let name_w = inner.saturating_sub(money_w * 2 + change_w + spark_w + module_w + 5).max(12);
    let now = now_secs();
    let header = Line::styled(
        format!(
            "  {:<name_w$} {:>money_w$} {:>money_w$} {:>change_w$} {:<spark_w$} {}",
            "service",
            month_name(s.read_at),
            month_name(s.read_at - 31.0 * DAY).chars().take(money_w).collect::<String>(),
            "change",
            "last 30 days",
            "in dcli"
        ),
        theme::muted(),
    );
    let items: Vec<ListItem> = m
        .rows
        .iter()
        .map(|row| match *row {
            Row::Service(i) => {
                let svc = &s.services[i];
                let marker = if m.opened.contains(&svc.service) { "▾" } else { "▸" };
                let stepped = m.step_of(&svc.service).is_some();
                let name = ellipsize(&svc.service, name_w.saturating_sub(if stepped { 2 } else { 0 }));
                let values = columns(&svc.daily, (now - 30.0 * DAY, now), spark_w, Agg::Sum);
                let ch = change(svc.month_to_date_complete, svc.last_month_to_date).unwrap_or_default();
                let ch_color =
                    if svc.month_to_date_complete > svc.last_month_to_date * 1.1 && svc.last_month_to_date > 0.0 { theme::WARN } else { theme::MUTED };
                let mut spans = vec![Span::styled(format!("{marker} "), theme::muted()), Span::raw(name.clone())];
                if stepped {
                    spans.push(Span::styled(" ▲", Style::new().fg(theme::WARN)));
                }
                let pad = name_w.saturating_sub(name.chars().count() + if stepped { 2 } else { 0 });
                spans.push(Span::raw(" ".repeat(pad)));
                spans.push(Span::styled(format!(" {:>money_w$}", money(svc.month_to_date, c)), theme::bold()));
                spans.push(Span::raw(format!(" {:>money_w$}", money(svc.last_month, c))));
                spans.push(Span::styled(format!(" {ch:>change_w$}"), Style::new().fg(ch_color)));
                spans.push(Span::styled(format!(" {:<spark_w$}", sparkline(&values)), theme::accent()));
                spans.push(Span::styled(format!(" {}", services::module(&svc.service).unwrap_or("")), theme::muted()));
                ListItem::new(Line::from(spans))
            }
            Row::Usage(_, j) => {
                let u = &s.usage[j];
                let unit_rate = u.rate().map(|r| format!(" · {} per {}", rate(r, c), u.unit)).unwrap_or_default();
                let text = format!("    └ {}  {} in 30 days · {} {}{unit_rate}", u.usage_type, money(u.cost, c), qty(u.quantity), u.unit);
                ListItem::new(Line::styled(ellipsize(&text, inner), theme::muted()))
            }
        })
        .collect();
    let block = widgets::panel(format!(" {} services ", s.services.len()), true);
    let inner_area = block.inner(area);
    f.render_widget(block, area);
    let [head, list] = inner_area.layout(&Layout::vertical([Constraint::Length(1), Constraint::Fill(1)]));
    f.render_widget(Paragraph::new(header), head);
    f.render_stateful_widget(List::new(items).highlight_style(theme::selected()), list, &mut m.list_state);
}

fn service_detail(m: &Costs, s: &Snapshot, svc: &ServiceCost, width: usize) -> Vec<Line<'static>> {
    let c = s.currency.as_str();
    let kw = 12;
    let mut lines = Vec::new();
    let vs = change(svc.month_to_date_complete, svc.last_month_to_date)
        .map(|ch| format!("  {ch} to yesterday vs the same days of last month ({})", money(svc.last_month_to_date, c)))
        .unwrap_or_default();
    lines.push(widgets::kv("This month", Span::styled(format!("{}{vs}", money(svc.month_to_date, c)), theme::bold()), kw));
    lines.push(widgets::kv("Last month", money(svc.last_month, c), kw));
    let covered = match s.coverage.iter().find(|cv| cv.service == svc.service) {
        Some(cv) if cv.covered > 0.0 => format!("{:.0} % by {} · {} outside them in 30 days", cv.covered * 100.0, cv.by, money(cv.on_demand_cost, c)),
        Some(cv) => format!("nothing: on demand ({} in 30 days)", money(cv.on_demand_cost, c)),
        None => "—".into(),
    };
    lines.push(widgets::kv("Covered", covered, kw));
    if let Some(end) = s.renewal(&svc.service) {
        lines.push(widgets::kv("Commitment", format!("one that can cover it ends {}", crate::costs::explorer::date(end)), kw));
    }
    lines.push(widgets::kv("In dcli", services::module(&svc.service).unwrap_or("—"), kw));
    if let Some(step) = m.step_of(&svc.service) {
        lines.push(Line::raw(""));
        lines.push(Line::styled(step.sentence(c), Style::new().fg(theme::WARN)));
        lines.push(Line::styled("A correlation, not proof: traffic or other changes can explain it.", theme::muted()));
    }
    lines.push(Line::raw(""));
    // Whole days, today's included: the axis then counts days.
    let today_end = crate::costs::explorer::day_start(now_secs()) + DAY;
    let window = (s.services.iter().flat_map(|x| x.daily.first().map(|(t, _)| *t)).fold(today_end - DAY, f64::min), today_end);
    let mut chart = Chart::new(format!("Daily cost ({c})"), &svc.daily, window);
    chart.rows = 4;
    chart.fmt = fmt_count;
    chart.agg = Agg::Sum;
    lines.extend(chart.lines(width));
    if let Some((deploys, failed)) = &m.deploys {
        lines.push(deploy_marks(deploys, window, width));
        if !failed.is_empty() {
            lines.push(Line::styled(format!("Deploys not read: {}", failed.join("; ")), theme::muted()));
        }
    } else if m.deploys_loading {
        lines.push(Line::styled("        reading the deploys…", theme::muted()));
    }
    let usage: Vec<&UsageCost> = s.usage.iter().filter(|u| u.service == svc.service).take(6).collect();
    if !usage.is_empty() {
        lines.push(Line::raw(""));
        lines.push(widgets::section("Usage types, last 30 days"));
        for u in usage {
            let unit_rate = u.rate().map(|r| format!(" · {} per {}", rate(r, c), u.unit)).unwrap_or_default();
            lines.push(Line::from(vec![
                Span::raw(format!("{} ", u.usage_type)),
                Span::styled(money(u.cost, c), theme::bold()),
                Span::styled(format!(" · {} {}{unit_rate}", qty(u.quantity), u.unit), theme::muted()),
            ]));
        }
    }
    lines.push(Line::raw(""));
    let mut hints = vec!["Enter usage types".to_string()];
    if let Some(module) = services::module(&svc.service) {
        hints.push(format!("o open in {module}"));
    }
    if let Some(check) = services::check(&svc.service) {
        hints.push(format!("g Savings for {check}"));
    }
    lines.push(Line::styled(hints.join(" · "), theme::muted()));
    lines
}

/// `deploys ▲      ▲▲`: a mark under the chart's columns on the days the environment deployed.
fn deploy_marks(deploys: &[Deploy], (start, end): (f64, f64), width: usize) -> Line<'static> {
    let plot_w = width.saturating_sub(8).max(4);
    let mut cells = vec![' '; plot_w];
    for d in deploys.iter().filter(|d| d.at >= start && d.at <= end) {
        let c = ((d.at - start) / (end - start).max(1.0) * plot_w as f64) as usize;
        cells[c.min(plot_w - 1)] = '▲';
    }
    Line::from(vec![Span::styled("deploys ", theme::muted()), Span::styled(cells.into_iter().collect::<String>(), Style::new().fg(theme::WARN))])
}

fn usage_detail(s: &Snapshot, svc: &ServiceCost, u: &UsageCost) -> Vec<Line<'static>> {
    let c = s.currency.as_str();
    let kw = 15;
    let service_total: f64 = s.usage.iter().filter(|x| x.service == svc.service).map(|x| x.cost).sum();
    let mut lines = vec![
        widgets::kv("Service", svc.service.clone(), kw),
        widgets::kv("Cost, 30 days", Span::styled(money(u.cost, c), theme::bold()), kw),
        widgets::kv("Used", format!("{} {}", qty(u.quantity), u.unit), kw),
        widgets::kv("Effective rate", u.rate().map_or_else(|| "—".into(), |r| format!("{} per {}", rate(r, c), u.unit)), kw),
    ];
    if service_total > 0.0 {
        lines.push(widgets::kv("Share", format!("{:.0} % of the service's usage cost", u.cost / service_total * 100.0), kw));
    }
    lines.push(Line::raw(""));
    lines.push(Line::styled(
        "What one unit cost on average over the last 30 days: the account's own price, discounts and the commitments \
         that cover it included. Savings prices its findings with these rates.",
        theme::muted(),
    ));
    lines
}

// ---- commitments -----------------------------------------------------------------------------

fn commitments_tab(m: &Costs, f: &mut Frame, area: Rect) {
    let Some(s) = &m.snapshot else { return };
    let c = s.currency.as_str();
    let now = now_secs();
    let mut lines = vec![widgets::section("Savings Plans and reservations")];
    if s.commitments.is_empty() {
        lines.push(Line::styled(
            if s.reader.contains("management") {
                "None seen."
            } else {
                "None seen. Read with the environment's profile: plans bought in the management account may not show (s sets the reader)."
            },
            theme::muted(),
        ));
    }
    for k in &s.commitments {
        lines.push(Line::from(vec![Span::styled(k.kind.clone(), theme::bold()), Span::styled(format!("  {}", k.id), theme::muted())]));
        let mut spans = vec![Span::raw(format!("  {} an hour", rate(k.hourly, c)))];
        if let Some(end) = k.end {
            let days = ((end - now) / DAY).ceil().max(0.0);
            let soon = days < 60.0;
            spans.push(Span::styled(
                format!(" · ends {} ({days:.0} days){}", crate::costs::explorer::date(end), if soon { " ⚠" } else { "" }),
                if soon { Style::new().fg(theme::WARN) } else { Style::new() },
            ));
        }
        if let Some(u) = k.utilization {
            let color = if u < 0.9 { theme::WARN } else { theme::OK };
            spans.push(Span::styled(format!(" · used {:.0} %", u * 100.0), Style::new().fg(color)));
        }
        if let Some(unused) = k.unused_cost.filter(|v| *v > 0.0) {
            spans.push(Span::styled(format!(" · unused in 30 days {}", money(unused, c)), Style::new().fg(theme::WARN)));
        }
        spans.push(Span::styled(format!(" · {}", k.scope), theme::muted()));
        lines.push(Line::from(spans));
        if !k.services.is_empty() {
            lines.push(Line::styled(format!("  covers {}", k.services.join(", ")), theme::muted()));
        }
    }
    lines.push(Line::raw(""));
    lines.push(widgets::section("Coverage, last 30 days"));
    if s.coverage.is_empty() {
        lines.push(Line::styled("Not read.", theme::muted()));
    }
    let name_w = s.coverage.iter().map(|cv| cv.service.chars().count()).max().unwrap_or(10).min(40);
    for cv in &s.coverage {
        let color = if cv.covered >= 0.8 { theme::OK } else if cv.covered > 0.0 { theme::WARN } else { theme::MUTED };
        let mut spans = vec![Span::raw(format!("{:<name_w$} ", ellipsize(&cv.service, name_w)))];
        spans.extend(widgets::progress_bar(cv.covered, 16, color));
        spans.push(Span::raw(format!(" {:>3.0} % by {}", cv.covered * 100.0, cv.by)));
        spans.push(Span::styled(format!(" · {} outside them", money(cv.on_demand_cost, c)), theme::muted()));
        lines.push(Line::from(spans));
    }
    lines.push(Line::raw(""));
    lines.push(widgets::section("AWS recommends"));
    match &s.recommendation {
        Some(r) => {
            lines.push(Line::from(vec![
                Span::raw(format!("A {} of ", r.kind)),
                Span::styled(format!("{} an hour", rate(r.hourly_commitment, c)), theme::bold()),
                Span::raw(format!(" ({}, {}), from the last {} days: AWS estimates ", r.term.to_lowercase().replace('_', " "), r.payment.to_lowercase().replace('_', " "), r.lookback_days)),
                Span::styled(format!("{} saved a month", money(r.estimated_monthly_savings, c)), Style::new().fg(theme::OK)),
                Span::raw("."),
            ]));
            lines.push(Line::styled(
                "Apply the Savings findings for the services it covers first: committing before optimizing pays for usage you \
                 were about to remove. g opens Savings.",
                Style::new().fg(theme::WARN),
            ));
        }
        None => lines.push(Line::styled("Nothing to buy: AWS has no recommendation for this usage, or has not made it yet.", theme::muted())),
    }
    if !s.missing.is_empty() {
        lines.push(Line::raw(""));
        lines.push(widgets::section("Not read"));
        lines.extend(s.missing.iter().map(|x| Line::styled(format!("· {x}"), theme::muted())));
    }
    let block = widgets::panel(" Commitments ", true);
    f.render_widget(Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false }).scroll((m.commitments_scroll, 0)).block(block), area);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordinals_and_marks() {
        assert_eq!([1, 2, 3, 4, 11, 12, 13, 21, 22, 23, 31].map(ordinal), ["1st", "2nd", "3rd", "4th", "11th", "12th", "13th", "21st", "22nd", "23rd", "31st"]);
        let d = |at: f64| Deploy { pipeline: "p".into(), at, commit: None, message: None };
        let line = deploy_marks(&[d(0.0), d(50.0), d(100.0), d(500.0)], (0.0, 100.0), 18);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "deploys ▲    ▲   ▲", "the last day sits in the last column");
    }
}
