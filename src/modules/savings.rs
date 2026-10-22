//! Savings: what could cost less in an environment, measured in the units AWS bills.

use ratatui::Frame;
use ratatui::crossterm::event::KeyEvent;
use ratatui::layout::Rect;
use ratatui::text::Line;

use crate::tui::module::{Ctx, Handled, Module, Payload, View};
use crate::tui::{theme, widgets};

pub struct Savings;

impl Savings {
    pub fn new() -> Savings {
        Savings
    }
}

impl Module for Savings {
    fn title(&self) -> &'static str {
        "Savings"
    }

    fn description(&self) -> &'static str {
        "What could cost less · unused or oversized resources"
    }

    fn reset(&mut self, _ctx: &mut Ctx) {}

    fn key(&mut self, _key: KeyEvent, _ctx: &mut Ctx) -> Handled {
        Handled::No
    }

    fn task(&mut self, _payload: Payload, _ctx: &mut Ctx) {}

    fn render(&mut self, f: &mut Frame, area: Rect, _view: &View) {
        let text = Line::styled("Not built yet", theme::muted());
        f.render_widget(widgets::placeholder(text).block(widgets::panel(" Savings ", false)), area);
    }

    fn hints(&self) -> Vec<(&'static str, &'static str)> {
        Vec::new()
    }
}
