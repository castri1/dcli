//! DynamoDB · tables, query by key, scan, items as JSON
//! (Being built: scaffold.)

use ratatui::Frame;
use ratatui::crossterm::event::KeyEvent;
use ratatui::layout::Rect;
use ratatui::text::Line;

use crate::tui::module::{Ctx, Handled, Module, Payload, View};
use crate::tui::{theme, widgets};

pub struct DynamoDb {}

impl DynamoDb {
    pub fn new() -> DynamoDb {
        DynamoDb {}
    }
}

impl Module for DynamoDb {
    fn title(&self) -> &'static str {
        "DynamoDB"
    }

    fn description(&self) -> &'static str {
        "DynamoDB · tables, query by key, scan, items as JSON"
    }

    fn reset(&mut self, _ctx: &mut Ctx) {}

    fn key(&mut self, _key: KeyEvent, _ctx: &mut Ctx) -> Handled {
        Handled::No
    }

    fn task(&mut self, _payload: Payload, _ctx: &mut Ctx) {}

    fn render(&mut self, f: &mut Frame, area: Rect, _view: &View) {
        f.render_widget(
            widgets::placeholder(Line::styled("Coming soon", theme::muted())).block(widgets::panel(" DynamoDB ", true)),
            area,
        );
    }

    fn hints(&self) -> Vec<(&'static str, &'static str)> {
        Vec::new()
    }
}
