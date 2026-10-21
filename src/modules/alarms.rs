//! Alarms: CloudWatch alarms in every environment, those firing first.

use ratatui::Frame;
use ratatui::crossterm::event::KeyEvent;
use ratatui::layout::Rect;
use ratatui::text::Line;

use crate::tui::module::{Ctx, Handled, Module, Payload, View};
use crate::tui::{theme, widgets};

pub struct Alarms;

impl Alarms {
    pub fn new() -> Alarms {
        Alarms
    }
}

impl Module for Alarms {
    fn title(&self) -> &'static str {
        "Alarms"
    }

    fn description(&self) -> &'static str {
        "CloudWatch alarms firing, in every environment"
    }

    fn reset(&mut self, _ctx: &mut Ctx) {}

    fn key(&mut self, _key: KeyEvent, _ctx: &mut Ctx) -> Handled {
        Handled::No
    }

    fn task(&mut self, _payload: Payload, _ctx: &mut Ctx) {}

    fn render(&mut self, f: &mut Frame, area: Rect, _view: &View) {
        let text = Line::styled("Not built yet", theme::muted());
        f.render_widget(widgets::placeholder(text).block(widgets::panel(" Alarms ", false)), area);
    }

    fn hints(&self) -> Vec<(&'static str, &'static str)> {
        Vec::new()
    }
}
