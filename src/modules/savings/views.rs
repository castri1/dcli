//! The By cost and Verified tabs of the Savings module.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Style, Stylize};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{List, ListItem, Paragraph, Wrap};

use super::{CostRow, Savings, Splits};
use crate::costs::ledger::{self, Source, Status};
use crate::savings::price::money;
use crate::savings::short_name;
use crate::tui::module::View;
use crate::tui::{theme, widgets};
use crate::util::{ellipsize, fmt_ago, fmt_utc, now_secs};

fn status_style(s: Status) -> Style {
    match s {
        Status::AsExpected => Style::new().fg(theme::OK),
        Status::Less | Status::NoChange => Style::new().fg(theme::WARN),
        Status::Up => Style::new().fg(theme::ERR),
        Status::Waiting(_) => theme::muted(),
        Status::Ready => theme::accent(),
    }
}

impl Savings {
    pub(super) fn render_by_cost(&mut self, f: &mut Frame, area: Rect, view: &View) {
        let title = " By cost · the bill's usage types split over their resources ";
        let placeholder = |text: Line<'static>| widgets::placeholder(text).block(widgets::panel(title, true));
        if self.costs.is_none() {
            let text = Line::styled("No costs read yet: the Costs module reads the bill (r there), then this tab splits it", theme::muted());
            return f.render_widget(placeholder(text), area);
        }
        let splits = match &self.splits {
            Splits::Idle => return f.render_widget(placeholder(Line::styled("r splits the bill over the resources", theme::muted())), area),
            Splits::Running(done, total) => {
                let text = Line::from(vec![
                    Span::styled(theme::spinner(view.tick), theme::accent()),
                    Span::raw(format!(" measuring what each resource used ({done} of {total} usage types)…")),
                ]);
                return f.render_widget(placeholder(text), area);
            }
            Splits::Done { splits, .. } if splits.is_empty() => {
                let text = Line::styled("None of the bill's usage types in this region is one dcli can split", theme::muted());
                return f.render_widget(placeholder(text), area);
            }
            Splits::Done { splits, .. } => splits,
        };
        let title = match &self.splits {
            Splits::Done { at, .. } => format!(" By cost · the bill's usage types split over their resources · measured {} ", fmt_ago(*at)),
            _ => title.to_string(),
        };
        let width = area.width.saturating_sub(4) as usize;
        let items: Vec<ListItem> = self
            .cost_rows
            .iter()
            .map(|row| match *row {
                CostRow::Usage(i) => {
                    let s = &splits[i];
                    let mut spans = vec![
                        Span::styled(format!("{} ", s.usage), theme::bold()),
                        Span::styled(format!("{} (30 days)", money(s.cost, &s.currency)), Style::new().fg(theme::OK).bold()),
                        Span::styled(format!(" · {} · by {}", s.service, s.by), theme::muted()),
                    ];
                    if let Some(e) = &s.error {
                        spans.push(Span::styled(format!(" · not split: {e}"), Style::new().fg(theme::ERR)));
                    } else if s.shares.is_empty() {
                        spans.push(Span::styled(" · no resource here used it", theme::muted()));
                    }
                    ListItem::new(Line::from(spans))
                }
                CostRow::Resource(i, j) => {
                    let s = &splits[i];
                    let share = &s.shares[j];
                    let found = self.findings_on(share);
                    let first = found.first().map(|f| {
                        let done = self.followed(f);
                        let more = if found.len() > 1 { format!("  +{}", found.len() - 1) } else { String::new() };
                        (format!("{} {}{more}", if done { "✓" } else { f.impact.dots() }, f.summary), done)
                    });
                    let resource_w = (width / 3).clamp(12, 48);
                    let mut spans = vec![
                        Span::raw("   "),
                        Span::styled(format!("{:<resource_w$} ", ellipsize(&share.resource, resource_w)), Style::new().bold()),
                        Span::styled(format!("{:>4.0} %  ", share.share * 100.0), theme::muted()),
                        Span::styled(format!("{:>12}  ", format!("≈ {}", money(share.cost, &s.currency))), theme::accent()),
                    ];
                    match first {
                        Some((text, done)) => spans.push(Span::styled(
                            ellipsize(&text, width.saturating_sub(resource_w + 24)),
                            if done { theme::muted() } else { Style::new().fg(theme::WARN) },
                        )),
                        None => spans.push(Span::styled("no finding", theme::muted())),
                    }
                    ListItem::new(Line::from(spans))
                }
            })
            .collect();
        let list = List::new(items).block(widgets::panel(title, true)).highlight_style(theme::selected());
        f.render_stateful_widget(list, area, &mut self.cost_state);
    }

    pub(super) fn render_share(&self, f: &mut Frame, area: Rect) {
        let kw = 11;
        let Some((split, share)) = self.selected_share() else {
            let note = Line::styled(
                "Each usage type's cost over the last 30 days, shared out by what each resource used: estimated by usage, not lines of the bill.",
                theme::muted(),
            );
            f.render_widget(Paragraph::new(note).wrap(Wrap { trim: false }).block(widgets::panel(" By cost ", false)), area);
            return;
        };
        let mut lines = Vec::new();
        let title = match share {
            None => {
                lines.push(widgets::kv("Usage type", Span::styled(split.usage.clone(), theme::bold()), kw));
                lines.push(widgets::kv("Service", split.service.clone(), kw));
                lines.push(widgets::kv("Cost", Span::styled(format!("{} over the last 30 days", money(split.cost, &split.currency)), Style::new().fg(theme::OK)), kw));
                lines.push(widgets::kv("Split by", split.by, kw));
                lines.push(widgets::kv("Resources", split.shares.len().to_string(), kw));
                lines.push(Line::raw(""));
                lines.push(Line::styled(
                    "Estimated by usage: the bill has the usage type's total, and dcli shares it out by what each resource used (its metrics or configuration).",
                    theme::muted(),
                ));
                format!(" {} ", split.usage)
            }
            Some(share) => {
                lines.push(widgets::kv("Resource", Span::styled(share.resource.clone(), theme::bold()), kw));
                lines.push(widgets::kv("Usage type", format!("{} ({})", split.usage, split.service), kw));
                lines.push(widgets::kv(
                    "Its part",
                    Span::styled(format!("≈ {} of {} (30 days), {:.0} %", money(share.cost, &split.currency), money(split.cost, &split.currency), share.share * 100.0), Style::new().fg(theme::OK)),
                    kw,
                ));
                lines.push(widgets::kv("Split by", split.by, kw));
                let found = self.findings_on(share);
                lines.push(Line::raw(""));
                if found.is_empty() {
                    lines.push(Line::styled("No finding on it: Enter opens it in its module.", theme::muted()));
                } else {
                    lines.push(widgets::section("Findings"));
                    for f in found {
                        let price = f.price.as_ref().map(|p| format!(" · {}", p.describe())).unwrap_or_default();
                        lines.push(Line::from(vec![
                            Span::styled(format!("{} ", f.impact.dots()), Style::new().fg(super::impact_color(f.impact))),
                            Span::raw(format!("{} → {}", f.summary, f.recommendation)),
                            Span::styled(price, Style::new().fg(theme::OK)),
                        ]));
                    }
                    lines.push(Line::styled("Enter shows the first in Findings · g opens the resource", theme::muted()));
                }
                lines.push(Line::raw(""));
                lines.push(Line::styled("Estimated by usage, not a line of the bill.", theme::muted()));
                format!(" {} ", share.resource)
            }
        };
        let block = widgets::panel(Line::from(title).bold(), false);
        f.render_widget(Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false }).scroll((self.detail_scroll, 0)).block(block), area);
    }

    pub(super) fn render_ledger(&mut self, f: &mut Frame, area: Rect) {
        let title = format!(" Changes followed · {} ", self.ledger.len());
        if self.ledger.is_empty() {
            let text = Line::styled(
                "No change followed yet: applying a fix (a), marking a finding done (d) or a cost action in Aurora, Redshift or DMS adds one",
                theme::muted(),
            );
            return f.render_widget(widgets::placeholder(text).block(widgets::panel(title, true)), area);
        }
        let now = now_secs();
        let width = area.width.saturating_sub(4) as usize;
        let items: Vec<ListItem> = self
            .ledger
            .iter()
            .map(|e| {
                let status = ledger::status(e, now);
                let result = match (&e.verified, status) {
                    (Some(v), _) => format!("{} · {}", status.label(), v.difference(&e.currency)),
                    (None, _) => status.label(),
                };
                let expected = e.expected.map(|x| format!("≈ {}/mo", money(x, &e.currency))).unwrap_or_else(|| "—".into());
                let what_w = width.saturating_sub(12 + 16 + result.chars().count() + 4).max(12);
                ListItem::new(Line::from(vec![
                    Span::styled(format!("{} ", fmt_utc(e.at).split(' ').next().unwrap_or_default()), theme::muted()),
                    Span::styled(format!("{:<what_w$} ", ellipsize(&e.what, what_w)), Style::new().bold()),
                    Span::styled(format!("{expected:>14}  "), theme::accent()),
                    Span::styled(result, status_style(status)),
                ]))
            })
            .collect();
        let list = List::new(items).block(widgets::panel(title, true)).highlight_style(theme::selected());
        f.render_stateful_widget(list, area, &mut self.ledger_state);
    }

    pub(super) fn render_entry(&self, f: &mut Frame, area: Rect) {
        let Some(e) = self.selected_entry() else {
            f.render_widget(widgets::placeholder(Line::styled("", theme::muted())).block(widgets::panel(" Change ", false)), area);
            return;
        };
        let kw = 11;
        let status = ledger::status(e, now_secs());
        let source = match e.source {
            Source::Applied => "applied by dcli",
            Source::Done => "marked done",
            Source::Action => "an action in dcli",
        };
        let mut lines = vec![
            widgets::kv("Change", Span::styled(e.what.clone(), theme::bold()), kw),
            widgets::kv("When", format!("{} ({}, {source})", fmt_utc(e.at), fmt_ago(e.at)), kw),
            widgets::kv("Resource", format!("{} · {}", e.resource, short_name(&e.module)), kw),
            widgets::kv("On the bill", format!("{} · {}", e.service, e.usage.join(", ")), kw),
            widgets::kv("Expected", e.expected.map_or_else(|| "not priced".to_string(), |x| format!("≈ {} a month", money(x, &e.currency))), kw),
            widgets::kv("Status", Span::styled(status.label(), status_style(status)), kw),
        ];
        if let Some(v) = &e.verified {
            lines.push(widgets::kv(
                "Bill",
                format!(
                    "{} a day over {} days before, {} a day over {} days after (read {})",
                    money(v.before, &e.currency),
                    v.days_before,
                    money(v.after, &e.currency),
                    v.days_after,
                    fmt_ago(v.at)
                ),
                kw,
            ));
            lines.push(widgets::kv("Difference", Span::styled(v.difference(&e.currency), status_style(status)), kw));
            for note in &v.notes {
                lines.push(Line::styled(format!("! {note}"), theme::muted()));
            }
        }
        lines.push(Line::raw(""));
        lines.push(Line::styled(
            "The bill has each usage type's total: other resources, traffic and changes move it too, so this is what the bill did around the change, not what the change alone did.",
            theme::muted(),
        ));
        lines.push(Line::raw(""));
        let action = match status {
            Status::Waiting(_) => "v reads the bill once a week has passed".to_string(),
            _ => format!("v reads the bill: 1 Cost Explorer request (AWS bills each one) · {} this month", crate::costs::settings::requests_this_month()),
        };
        lines.push(Line::styled(format!("{action} · x stops following it"), theme::muted()));
        let block = widgets::panel(Line::from(format!(" {} ", ellipsize(&e.what, 60))).bold(), false);
        f.render_widget(Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false }).scroll((self.detail_scroll, 0)).block(block), area);
    }
}
