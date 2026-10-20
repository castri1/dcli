//! Reusable building blocks for modules: panels, popups, progress bars, inputs.

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Constraint, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, BorderType, Clear, Padding, Paragraph, Wrap};

use super::theme;

/// Rounded panel; the border lights up when focused.
pub fn panel<'a>(title: impl Into<Line<'a>>, focused: bool) -> Block<'a> {
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(if focused { theme::accent() } else { theme::muted() })
        .padding(Padding::horizontal(1))
        .title(title)
}

/// A centred rectangle of at most `width`×`height`.
pub fn popup_area(area: Rect, width: u16, height: u16) -> Rect {
    area.centered(
        Constraint::Length(width.min(area.width.saturating_sub(2))),
        Constraint::Length(height.min(area.height.saturating_sub(2))),
    )
}

/// Clears the area and draws a modal box with `body`.
pub fn popup(f: &mut Frame, area: Rect, title: &str, color: Color, body: Text) {
    f.render_widget(Clear, area);
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(color))
        .padding(Padding::horizontal(1))
        .title(Span::styled(format!(" {title} "), Style::new().fg(color).bold()));
    f.render_widget(Paragraph::new(body).wrap(Wrap { trim: false }).block(block), area);
}

/// `███████░░░░` filled to `ratio`.
pub fn progress_bar(ratio: f64, width: usize, color: Color) -> Vec<Span<'static>> {
    let filled = ((ratio.clamp(0.0, 1.0) * width as f64).round() as usize).min(width);
    vec![
        Span::styled("█".repeat(filled), Style::new().fg(color)),
        Span::styled("░".repeat(width - filled), theme::muted()),
    ]
}

/// A block sliding back and forth, for work without a known duration.
pub fn indeterminate_bar(tick: usize, width: usize, color: Color) -> Vec<Span<'static>> {
    let block = 3.min(width);
    let span = (width - block).max(1);
    let pos = tick % (span * 2);
    let pos = if pos >= span { span * 2 - pos } else { pos };
    vec![
        Span::styled("░".repeat(pos), theme::muted()),
        Span::styled("█".repeat(block), Style::new().fg(color)),
        Span::styled("░".repeat(width.saturating_sub(pos + block)), theme::muted()),
    ]
}

/// `key  value` with an aligned key column.
pub fn kv<'a>(key: &str, value: impl Into<Span<'a>>, key_width: usize) -> Line<'a> {
    Line::from(vec![Span::styled(format!("{key:<key_width$} "), theme::muted()), value.into()])
}

pub fn section<'a>(title: &str) -> Line<'a> {
    Line::from(Span::styled(title.to_uppercase(), theme::accent().bold()))
}

/// `d deploy · a approve`
pub fn hints_line<'a>(hints: &[(&'a str, &'a str)]) -> Line<'a> {
    let mut spans = Vec::new();
    for (i, (k, label)) in hints.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled("  ", theme::muted()));
        }
        spans.push(Span::styled(*k, theme::key()));
        spans.push(Span::raw(" "));
        spans.push(Span::styled(*label, theme::muted()));
    }
    Line::from(spans)
}

/// Message shown in an empty panel (loading, error, nothing found).
pub fn placeholder<'a>(text: impl Into<Line<'a>>) -> Paragraph<'a> {
    Paragraph::new(vec![Line::raw(""), text.into().centered()]).wrap(Wrap { trim: true })
}

#[derive(Debug, PartialEq, Eq)]
pub enum InputEvent {
    Changed,
    Submit,
    Cancel,
    Moved,
    Ignored,
}

/// Single-line text field.
#[derive(Debug, Default, Clone)]
pub struct TextInput {
    pub value: String,
    /// Cursor position in characters.
    cursor: usize,
}

impl TextInput {
    fn byte_index(&self, chars: usize) -> usize {
        self.value.char_indices().nth(chars).map_or(self.value.len(), |(i, _)| i)
    }

    pub fn clear(&mut self) {
        self.value.clear();
        self.cursor = 0;
    }

    pub fn handle(&mut self, key: KeyEvent) -> InputEvent {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let len = self.value.chars().count();
        match key.code {
            KeyCode::Enter => InputEvent::Submit,
            KeyCode::Esc => InputEvent::Cancel,
            KeyCode::Char('u') if ctrl => {
                self.clear();
                InputEvent::Changed
            }
            KeyCode::Char('w') if ctrl => {
                let before: String = self.value.chars().take(self.cursor).collect();
                let trimmed = before.trim_end();
                let keep = trimmed.rfind(char::is_whitespace).map_or(0, |i| i + 1);
                let keep_chars = trimmed[..keep].chars().count();
                let from = self.byte_index(keep_chars);
                let to = self.byte_index(self.cursor);
                self.value.replace_range(from..to, "");
                self.cursor = keep_chars;
                InputEvent::Changed
            }
            KeyCode::Char(c) if !ctrl => {
                let i = self.byte_index(self.cursor);
                self.value.insert(i, c);
                self.cursor += 1;
                InputEvent::Changed
            }
            KeyCode::Backspace if self.cursor > 0 => {
                let i = self.byte_index(self.cursor - 1);
                self.value.remove(i);
                self.cursor -= 1;
                InputEvent::Changed
            }
            KeyCode::Delete if self.cursor < len => {
                let i = self.byte_index(self.cursor);
                self.value.remove(i);
                InputEvent::Changed
            }
            KeyCode::Left => {
                self.cursor = self.cursor.saturating_sub(1);
                InputEvent::Moved
            }
            KeyCode::Right => {
                self.cursor = (self.cursor + 1).min(len);
                InputEvent::Moved
            }
            KeyCode::Home => {
                self.cursor = 0;
                InputEvent::Moved
            }
            KeyCode::End => {
                self.cursor = len;
                InputEvent::Moved
            }
            _ => InputEvent::Ignored,
        }
    }

    /// Renders `prefix` + value; returns the line and the cursor column offset.
    pub fn line<'a>(&self, prefix: &'a str, placeholder: &'a str) -> (Line<'a>, u16) {
        let mut spans = vec![Span::styled(prefix, theme::accent())];
        if self.value.is_empty() {
            spans.push(Span::styled(placeholder, theme::muted()));
        } else {
            spans.push(Span::raw(self.value.clone()));
        }
        let col = prefix.chars().count() + self.cursor;
        (Line::from(spans), col as u16)
    }
}
