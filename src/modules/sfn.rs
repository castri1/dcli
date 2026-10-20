//! Step Functions · executions, failed steps, logs, redrive, start, stop
//! (Being built: scaffold.)

use ratatui::Frame;
use ratatui::crossterm::event::KeyEvent;
use ratatui::layout::Rect;
use ratatui::text::Line;

use crate::tui::module::{Ctx, Handled, Module, Payload, View};
use crate::tui::{theme, widgets};

pub struct StepFunctions {}

impl StepFunctions {
    pub fn new() -> StepFunctions {
        StepFunctions {}
    }
}

impl Module for StepFunctions {
    fn title(&self) -> &'static str {
        "Step Functions"
    }

    fn description(&self) -> &'static str {
        "Step Functions · executions, failed steps, logs, redrive, start, stop"
    }

    fn reset(&mut self, _ctx: &mut Ctx) {}

    fn key(&mut self, _key: KeyEvent, _ctx: &mut Ctx) -> Handled {
        Handled::No
    }

    fn task(&mut self, _payload: Payload, _ctx: &mut Ctx) {}

    fn render(&mut self, f: &mut Frame, area: Rect, _view: &View) {
        f.render_widget(
            widgets::placeholder(Line::styled("Coming soon", theme::muted())).block(widgets::panel(" Step Functions ", true)),
            area,
        );
    }

    fn hints(&self) -> Vec<(&'static str, &'static str)> {
        Vec::new()
    }
}
