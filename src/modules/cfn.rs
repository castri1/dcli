//! CloudFormation: stacks, their events, resources, outputs and drift; failure reports for an AI agent.

use ratatui::Frame;
use ratatui::crossterm::event::KeyEvent;
use ratatui::layout::Rect;
use ratatui::text::Line;

use crate::tui::module::{Ctx, Handled, Module, Payload, View};
use crate::tui::{theme, widgets};

pub struct CloudFormation;

impl CloudFormation {
    pub fn new() -> CloudFormation {
        CloudFormation
    }
}

impl Module for CloudFormation {
    fn title(&self) -> &'static str {
        "CloudFormation"
    }

    fn description(&self) -> &'static str {
        "CloudFormation stacks · events, resources, outputs, drift, failure reports"
    }

    fn reset(&mut self, _ctx: &mut Ctx) {}

    fn key(&mut self, _key: KeyEvent, _ctx: &mut Ctx) -> Handled {
        Handled::No
    }

    fn task(&mut self, _payload: Payload, _ctx: &mut Ctx) {}

    fn render(&mut self, f: &mut Frame, area: Rect, _view: &View) {
        let text = Line::styled("Not built yet", theme::muted());
        f.render_widget(widgets::placeholder(text).block(widgets::panel(" CloudFormation ", false)), area);
    }

    fn hints(&self) -> Vec<(&'static str, &'static str)> {
        Vec::new()
    }
}
