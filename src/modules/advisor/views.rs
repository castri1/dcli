//! Drawing the Advisor: the figures of the pending recommendations, the list, the detail
//! with its steps, the empty state and the dialogs. Everything Claude wrote is sanitized here
//! again (the files can be edited by hand).

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{List as ListWidget, ListItem, Paragraph, Wrap};

use super::{Advisor, Dialog, module_of};
use crate::advisor::summary::{self, VIEWS, View as List};
use crate::advisor::{self, Level as Grade, Recommendation, State};
use crate::costs::ledger;
use crate::tui::{theme, widgets};
use crate::util::{ellipsize, fmt_ago, now_secs, program, sanitize};

/// Width from which the list and the detail sit side by side.
const SIDE_BY_SIDE: u16 = 150;
const KW: usize = 12;

fn grade_color(level: Grade) -> Color {
    match level {
        Grade::High => theme::OK,
        Grade::Medium => theme::WARN,
        Grade::Low => theme::MUTED,
    }
}

/// Risk reads the other way round: high is red.
fn risk_color(level: Grade) -> Color {
    match level {
        Grade::High => theme::ERR,
        Grade::Medium => theme::WARN,
        Grade::Low => theme::OK,
    }
}

/// The colour of a figure: green saves, yellow costs more, grey none.
fn money_style(r: &Recommendation) -> Style {
    match r.cost.monthly() {
        Some(m) if m < 0.0 => Style::new().fg(theme::WARN).bold(),
        Some(_) if r.cost.bill.as_ref().is_some_and(|b| b.priced()) => Style::new().fg(theme::OK).bold(),
        Some(_) => Style::new().fg(theme::OK),
        None => theme::muted(),
    }
}

fn category_style(r: &Recommendation) -> Style {
    match r.category {
        advisor::Category::Cost => Style::new().fg(theme::OK),
        advisor::Category::Performance => Style::new().fg(theme::APPROVAL),
    }
}

/// Lines of `text` (which may hold line breaks), sanitized, in `style`.
fn paragraph(text: &str, style: Style, indent: &str) -> Vec<Line<'static>> {
    sanitize(text).lines().map(|l| Line::from(vec![Span::raw(indent.to_string()), Span::styled(l.to_string(), style)])).collect()
}

/// `key  value` for a value that may span lines: the first beside the key, the rest under it.
fn kv_text(key: &str, text: &str, style: Style) -> Vec<Line<'static>> {
    let clean = sanitize(text);
    let mut lines = clean.lines();
    let mut out = vec![widgets::kv(key, Span::styled(lines.next().unwrap_or_default().to_string(), style), KW)];
    out.extend(lines.map(|l| Line::from(vec![Span::raw(" ".repeat(KW + 1)), Span::styled(l.to_string(), style)])));
    out
}

impl Advisor {
    pub(super) fn render_all(&mut self, f: &mut Frame, area: Rect) {
        if self.all.is_empty() {
            self.list_area = Rect::default();
            self.detail_area = Rect::default();
            self.render_empty(f, area);
            return;
        }
        let status = self.status_lines(area.width);
        let height = widgets::wrapped_height(&status, area.width).min(area.height / 3);
        let [top, body] = area.layout(&Layout::vertical([Constraint::Length(height), Constraint::Fill(1)]));
        f.render_widget(Paragraph::new(status).wrap(Wrap { trim: false }), top);
        let [list, detail] = if body.width >= SIDE_BY_SIDE {
            body.layout(&Layout::horizontal([Constraint::Percentage(45), Constraint::Fill(1)]))
        } else {
            body.layout(&Layout::vertical([Constraint::Percentage(40), Constraint::Fill(1)]))
        };
        self.list_area = list;
        self.detail_area = detail;
        self.render_list(f, list);
        self.render_detail(f, detail);
        self.render_dialog(f, area);
    }

    /// The counts, what the pending ones save or cost, when the costs were read, the lists.
    fn status_lines(&self, width: u16) -> Vec<Line<'static>> {
        let mut lines = Vec::new();
        let pending: Vec<&Recommendation> = self.all.iter().filter(|r| r.state == State::Pending).collect();
        let totals = summary::totals(pending.iter().copied());
        let mut head = vec![Span::styled(format!(" {} pending", totals.pending), theme::bold())];
        match &self.costs {
            Some(c) => head.push(Span::styled(format!(" · priced with the costs read {}", fmt_ago(c.read_at)), theme::muted())),
            None => head.push(Span::styled(
                format!(
                    " · no costs read for {}: the Costs module (r there) or `{} costs {} --refresh` reads the bill to price them",
                    sanitize(&self.env_name),
                    program(),
                    sanitize(&self.env_name)
                ),
                Style::new().fg(theme::WARN),
            )),
        }
        lines.push(Line::from(head));
        let bill = totals.bill_text();
        if !bill.is_empty() {
            lines.push(Line::from(vec![Span::styled(" From your bill: ", theme::muted()), Span::styled(bill, Style::new().fg(theme::OK).bold())]));
        }
        let claude = totals.claude_text();
        if !claude.is_empty() {
            lines.push(Line::from(vec![Span::styled(" Claude's estimates: ", theme::muted()), Span::styled(claude, Style::new().fg(theme::WARN))]));
        }
        let mut notes = Vec::new();
        if totals.unpriced > 0 {
            notes.push(format!("{} not priced yet", totals.unpriced));
        }
        if totals.overlap {
            notes.push("some are about the same resource and may overlap".to_string());
        }
        if !notes.is_empty() {
            lines.push(Line::styled(format!(" {}", notes.join(" · ")), theme::muted()));
        }
        let mut tabs = vec![Span::raw(" ")];
        for list in VIEWS {
            let style = if list == self.list { Style::new().fg(Color::Black).bg(theme::ACCENT).bold() } else { theme::muted() };
            tabs.push(Span::styled(format!(" {} {} ", list.label(), self.count(list)), style));
            tabs.push(Span::raw(" "));
        }
        tabs.push(Span::styled("  ←→", theme::key()));
        if width >= 100 {
            tabs.push(Span::styled("   dcli ran none of these: you copy the steps", theme::muted()));
        }
        lines.push(Line::from(tabs));
        lines
    }

    fn render_list(&mut self, f: &mut Frame, area: Rect) {
        let title = format!(" {} · {} ", self.list.label(), self.rows.len());
        if self.rows.is_empty() {
            let text = match self.list {
                List::Pending => "Nothing pending: ←→ shows the done and dismissed ones",
                List::Done => "None marked done yet (d on a pending one)",
                List::Closed => "None dismissed or withdrawn",
            };
            f.render_widget(widgets::placeholder(Line::styled(text, theme::muted())).block(widgets::panel(title, true)), area);
            return;
        }
        let width = area.width.saturating_sub(4) as usize;
        let shown: Vec<&Recommendation> = self.rows.iter().map(|&i| &self.all[i]).collect();
        let id_w = shown.iter().map(|r| r.id.chars().count()).max().unwrap_or(2);
        let cost_w = shown.iter().map(|r| r.cost.short().chars().count()).max().unwrap_or(1);
        // Narrow lists drop where the figure comes from and the confidence.
        let wide = width >= 60;
        let fixed = 2 + id_w + 1 + 5 + cost_w + 1 + if wide { 7 + 4 } else { 0 };
        let title_w = width.saturating_sub(fixed).max(8);
        let items: Vec<ListItem> = shown
            .iter()
            .map(|r| {
                let mark = match r.state {
                    State::Pending if self.fresh.contains(&r.id) => Span::styled("• ", theme::accent().bold()),
                    State::Done => Span::styled("✓ ", Style::new().fg(theme::OK)),
                    s => Span::styled(format!("{} ", summary::state_mark(s)), theme::muted()),
                };
                let dim = if r.state == State::Pending { Style::new() } else { theme::muted() };
                let mut spans = vec![
                    mark,
                    Span::styled(format!("{:<id_w$} ", r.id), theme::muted()),
                    Span::styled(format!("{:<4} ", summary::category_word(r)), category_style(r)),
                    Span::styled(format!("{:>cost_w$} ", r.cost.short()), money_style(r)),
                ];
                if wide {
                    spans.push(Span::styled(format!("{:<6} ", summary::source_word(r)), theme::muted()));
                }
                spans.push(Span::styled(format!("{:<title_w$}", ellipsize(&sanitize(&r.title), title_w)), dim.bold()));
                if wide {
                    spans.push(Span::styled(format!(" {}", summary::dots(r.confidence)), Style::new().fg(grade_color(r.confidence))));
                }
                ListItem::new(Line::from(spans))
            })
            .collect();
        let focused = self.step.is_none();
        let list = ListWidget::new(items).block(widgets::panel(title, focused)).highlight_style(theme::selected());
        f.render_stateful_widget(list, area, &mut self.list_state);
    }

    /// The detail of the selected recommendation, and where each step starts (line index).
    fn detail_lines(&self, r: &Recommendation) -> (Vec<Line<'static>>, Vec<usize>) {
        let mut lines = Vec::new();
        let state = match r.state {
            State::Pending => Span::styled("pending", theme::accent()),
            State::Done => Span::styled("✓ done", Style::new().fg(theme::OK)),
            State::Dismissed => Span::styled("✗ dismissed", theme::muted()),
            State::Withdrawn => Span::styled("– withdrawn by Claude", theme::muted()),
        };
        lines.push(Line::from(vec![
            Span::styled(r.category.label().to_string(), category_style(r).bold()),
            Span::styled(" · confidence ", theme::muted()),
            Span::styled(format!("{} {}", summary::dots(r.confidence), r.confidence.label()), Style::new().fg(grade_color(r.confidence))),
            Span::styled(" · risk ", theme::muted()),
            Span::styled(r.risk.label().to_string(), Style::new().fg(risk_color(r.risk))),
            Span::styled(format!(" · effort {} · ", r.effort.label()), theme::muted()),
            state,
        ]));

        // What it saves or costs, and how that was worked out.
        let source = sanitize(&r.cost.source());
        match advisor::cost_figure(&r.cost) {
            Some(figure) => {
                lines.push(widgets::kv("Cost", Span::styled(figure, money_style(r)), KW));
                let read = r.cost.bill.as_ref().filter(|b| b.priced() && b.priced_at > 0.0).map(|b| format!(" · costs read {}", fmt_ago(b.priced_at)));
                lines.push(widgets::kv("", Span::styled(format!("from {source}{}", read.unwrap_or_default()), theme::muted()), KW));
            }
            None => lines.push(widgets::kv("Cost", Span::styled(source, theme::muted()), KW)),
        }
        if let Some(b) = r.cost.bill.as_ref().filter(|b| b.priced()) {
            for l in b.rate_lines() {
                lines.push(widgets::kv("", Span::styled(format!("your bill: {}", sanitize(&l)), theme::muted()), KW));
            }
            for n in &b.notes {
                lines.push(widgets::kv("", Span::styled(format!("! {}", sanitize(n)), theme::muted()), KW));
            }
        } else if !r.cost.changes.is_empty() && self.costs.is_none() {
            lines.push(widgets::kv("", Span::styled("the Costs module (r there) reads the bill that prices it", theme::muted()), KW));
        }
        if let Some(c) = &r.cost.claude {
            let mut first = true;
            for l in sanitize(&c.reasoning).lines() {
                let label = if first { "Claude's estimate: " } else { "" };
                first = false;
                lines.push(widgets::kv(
                    "",
                    Span::styled(format!("{label}{l}"), Style::new().fg(theme::WARN)),
                    KW,
                ));
            }
        }
        if !r.performance.is_empty() {
            lines.extend(kv_text("Performance", &r.performance, theme::accent()));
        }
        if !r.resources.is_empty() {
            let names: Vec<String> = r
                .resources
                .iter()
                .map(|x| match module_of(&x.module) {
                    Some(m) => format!("{} ({m})", sanitize(&x.name)),
                    None if x.module.is_empty() => sanitize(&x.name),
                    None => format!("{} ({})", sanitize(&x.name), sanitize(&x.module)),
                })
                .collect();
            lines.push(widgets::kv("Resources", Span::styled(names.join(", "), theme::bold()), KW));
        }
        let mut by = Vec::new();
        if !r.author.is_empty() {
            by.push(sanitize(&r.author));
        }
        if r.updated > 0.0 {
            by.push(format!("written {}", fmt_ago(r.updated)));
        }
        if !by.is_empty() {
            lines.push(widgets::kv("By", Span::styled(by.join(" · "), theme::muted()), KW));
        }

        lines.push(Line::raw(""));
        lines.push(widgets::section("Found"));
        lines.extend(paragraph(&r.finding, Style::new(), ""));
        if !r.evidence.is_empty() {
            lines.push(Line::raw(""));
            lines.push(widgets::section("Evidence"));
            for e in &r.evidence {
                lines.push(Line::from(vec![Span::styled("· ", theme::muted()), Span::raw(sanitize(e))]));
            }
        }

        lines.push(Line::raw(""));
        lines.push(widgets::section("Steps"));
        let mut starts = Vec::new();
        for (i, s) in r.steps.iter().enumerate() {
            starts.push(lines.len());
            let focused = self.step == Some(i);
            let marker = if focused { Span::styled("▶ ", theme::accent().bold()) } else { Span::raw("  ") };
            let text_style = if focused { theme::bold() } else { Style::new() };
            let mut text = sanitize(&s.text).lines().map(str::to_string).collect::<Vec<_>>().into_iter();
            lines.push(Line::from(vec![marker, Span::styled(format!("{}. ", i + 1), theme::muted()), Span::styled(text.next().unwrap_or_default(), text_style)]));
            lines.extend(text.map(|l| Line::from(vec![Span::raw("     "), Span::styled(l, text_style)])));
            if let Some(c) = &s.command {
                let flag = if advisor::writes(c) {
                    Span::styled("  ⚠ can change the account", Style::new().fg(theme::WARN))
                } else {
                    Span::styled("  read-only", theme::muted())
                };
                let style = if focused { theme::accent().bold() } else { theme::accent() };
                lines.push(Line::from(vec![Span::raw("     "), Span::styled("$ ", theme::muted()), Span::styled(sanitize(c), style), flag]));
            }
            if let Some(code) = &s.code {
                let language = s.language.as_deref().map(sanitize).unwrap_or_else(|| "snippet".into());
                lines.push(Line::from(vec![Span::raw("     "), Span::styled(format!("{language}:"), theme::muted())]));
                for l in sanitize(code).lines() {
                    lines.push(Line::from(vec![Span::styled("     │ ", theme::muted()), Span::raw(l.to_string())]));
                }
            }
        }

        lines.push(Line::raw(""));
        let risk = if r.risk_note.is_empty() { r.risk.label().to_string() } else { format!("{}: {}", r.risk.label(), r.risk_note) };
        lines.extend(kv_text("Risk", &risk, Style::new().fg(risk_color(r.risk))));
        lines.extend(kv_text("Undo", &r.rollback, Style::new()));
        lines.extend(kv_text("Verify", &r.verify, Style::new()));
        match r.state {
            State::Dismissed => {
                let why = if r.state_note.is_empty() { "no reason given".to_string() } else { r.state_note.clone() };
                lines.extend(kv_text("Dismissed", &format!("{why} · {}", fmt_ago(r.state_at)), theme::muted()));
            }
            State::Withdrawn => {
                lines.extend(kv_text("Withdrawn", &format!("by Claude: {} · {}", r.state_note, fmt_ago(r.state_at)), theme::muted()));
            }
            State::Done => {
                lines.push(widgets::kv("Done", Span::styled(fmt_ago(r.state_at), Style::new().fg(theme::OK)), KW));
                let bill = match (&r.ledger, self.entry_of(r)) {
                    (Some(_), Some(e)) => {
                        let status = ledger::status(e, now_secs());
                        match &e.verified {
                            Some(v) => format!("{} · {} (Savings › Verified)", status.label(), v.difference(&e.currency)),
                            None => format!("{} (Savings › Verified)", status.label()),
                        }
                    }
                    (Some(_), None) => "no longer followed in Savings › Verified".to_string(),
                    (None, _) => "not followed: it names no usage of the bill".to_string(),
                };
                lines.push(widgets::kv("On the bill", Span::styled(bill, theme::muted()), KW));
            }
            State::Pending => {}
        }

        lines.push(Line::raw(""));
        let mut actions = Vec::new();
        if let Some(res) = r.resource()
            && let Some(m) = module_of(&res.module)
        {
            actions.push(format!("Enter opens {} in {m}", ellipsize(&sanitize(&res.name), 30)));
        }
        actions.push(if self.step.is_some() { "c copies this step".to_string() } else { "s goes through the steps · c copies the commands".to_string() });
        actions.push("m copies it as Markdown".to_string());
        match r.state {
            State::Pending => actions.push("d marks it done · x dismisses it".to_string()),
            State::Withdrawn => actions.push("u puts it back to pending · x dismisses it".to_string()),
            _ => actions.push("u puts it back to pending".to_string()),
        }
        lines.push(Line::styled(actions.join(" · "), theme::muted()));
        (lines, starts)
    }

    fn render_detail(&mut self, f: &mut Frame, area: Rect) {
        let Some(r) = self.selected() else {
            f.render_widget(widgets::placeholder(Line::styled("", theme::muted())).block(widgets::panel(" Recommendation ", false)), area);
            return;
        };
        let (lines, starts) = self.detail_lines(r);
        let title = Line::from(vec![
            Span::styled(format!(" {} · ", r.id), theme::muted()),
            Span::styled(format!("{} ", ellipsize(&sanitize(&r.title), area.width.saturating_sub(12) as usize)), theme::bold()),
        ]);
        if self.follow_step {
            self.follow_step = false;
            if let Some(start) = self.step.and_then(|i| starts.get(i)) {
                // Rows above the step once wrapped, less one for context.
                let inner = area.width.saturating_sub(4);
                let above = widgets::wrapped_height(&lines[..*start], inner);
                let visible = area.height.saturating_sub(2);
                if above < self.detail_scroll || above + 3 > self.detail_scroll + visible {
                    self.detail_scroll = above.saturating_sub(1);
                }
            }
        }
        let block = widgets::panel(title, self.step.is_some());
        f.render_widget(Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false }).scroll((self.detail_scroll, 0)).block(block), area);
    }

    fn render_empty(&self, f: &mut Frame, area: Rect) {
        let env = sanitize(&self.env_name);
        let program = program();
        let lines = vec![
            Line::raw(""),
            Line::styled(format!("No recommendations for {env} yet."), theme::bold()),
            Line::raw(""),
            Line::styled("Connect Claude to dcli once:", theme::muted()),
            Line::styled(format!("    claude mcp add dcli -- {program} mcp"), theme::accent()),
            Line::styled("Then ask it for an audit:", theme::muted()),
            Line::from(vec![
                Span::styled(format!("    /mcp__dcli__audit {env}"), theme::accent()),
                Span::styled("   or   ", theme::muted()),
                Span::styled(format!("«analiza {env} con dcli»"), theme::accent()),
            ]),
            Line::raw(""),
            Line::raw(
                "Claude reads the account through dcli, without changing anything, and leaves here what could cost less or run \
                 better: what each saves or costs at your bill's rates, the evidence, and the steps to do it.",
            ),
            Line::styled(
                "Nothing runs on its own: you copy the steps, mark them done, or dismiss them with a reason Claude reads next time.",
                theme::muted(),
            ),
            Line::raw(""),
            Line::styled(
                match &self.costs {
                    Some(c) => format!("Costs read {}: figures come from your bill.", fmt_ago(c.read_at)),
                    None => format!("Read the costs first (the Costs module, or `{program} costs {env} --refresh`) so figures come from your bill."),
                },
                theme::muted(),
            ),
        ];
        let block = widgets::panel(" Advisor ", true);
        f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }).block(block), area);
    }

    fn render_dialog(&self, f: &mut Frame, area: Rect) {
        match &self.dialog {
            Some(Dialog::Done { form, .. } | Dialog::Restore { form, .. }) => form.render(f, area, theme::ACCENT),
            Some(Dialog::Dismiss { form, .. }) => form.render(f, area, theme::WARN),
            None => {}
        }
    }
}
