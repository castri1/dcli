//! Colours and symbols. Only the 16 ANSI colours are used so the UI follows the
//! terminal's own palette (light or dark) and works in every terminal.

use ratatui::style::{Color, Modifier, Style};

use crate::pipeline::Status;

pub const ACCENT: Color = Color::Cyan;
pub const OK: Color = Color::Green;
pub const ERR: Color = Color::Red;
pub const WARN: Color = Color::Yellow;
pub const APPROVAL: Color = Color::Magenta;
pub const RUNNING: Color = Color::LightBlue;
pub const MUTED: Color = Color::DarkGray;

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

pub fn spinner(tick: usize) -> &'static str {
    SPINNER[tick % SPINNER.len()]
}

pub fn muted() -> Style {
    Style::new().fg(MUTED)
}

pub fn bold() -> Style {
    Style::new().add_modifier(Modifier::BOLD)
}

pub fn accent() -> Style {
    Style::new().fg(ACCENT)
}

pub fn selected() -> Style {
    Style::new().bg(Color::DarkGray).add_modifier(Modifier::BOLD)
}

pub fn key() -> Style {
    Style::new().fg(ACCENT).add_modifier(Modifier::BOLD)
}

pub fn status_color(s: Status) -> Color {
    match s {
        Status::Succeeded => OK,
        Status::Failed => ERR,
        Status::InProgress => RUNNING,
        Status::Stopping | Status::Stopped => WARN,
        Status::Approval => APPROVAL,
        Status::Superseded | Status::Cancelled | Status::Abandoned => MUTED,
        Status::Pending | Status::NotRun | Status::Unknown => MUTED,
    }
}

/// Status symbol; running states animate.
pub fn status_symbol(s: Status, tick: usize) -> &'static str {
    match s {
        Status::InProgress | Status::Stopping => spinner(tick),
        other => other.symbol(),
    }
}

pub fn status_style(s: Status) -> Style {
    Style::new().fg(status_color(s))
}

/// Production-looking environments are shown in red, the rest in green.
pub fn env_color(env_name: &str) -> Color {
    if crate::config::is_production(env_name) { ERR } else { OK }
}
