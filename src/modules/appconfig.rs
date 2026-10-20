//! AppConfig · feature flags per environment, edit and deploy with a strategy
//! (Being built: scaffold.)

use ratatui::Frame;
use ratatui::crossterm::event::KeyEvent;
use ratatui::layout::Rect;
use ratatui::text::Line;

use crate::tui::module::{Ctx, Handled, Module, Payload, View};
use crate::tui::{theme, widgets};

pub struct AppConfig {}

impl AppConfig {
    pub fn new() -> AppConfig {
        AppConfig {}
    }
}

impl Module for AppConfig {
    fn title(&self) -> &'static str {
        "AppConfig"
    }

    fn description(&self) -> &'static str {
        "AppConfig · feature flags per environment, edit and deploy with a strategy"
    }

    fn reset(&mut self, _ctx: &mut Ctx) {}

    fn key(&mut self, _key: KeyEvent, _ctx: &mut Ctx) -> Handled {
        Handled::No
    }

    fn task(&mut self, _payload: Payload, _ctx: &mut Ctx) {}

    fn render(&mut self, f: &mut Frame, area: Rect, _view: &View) {
        f.render_widget(
            widgets::placeholder(Line::styled("Coming soon", theme::muted())).block(widgets::panel(" AppConfig ", true)),
            area,
        );
    }

    fn hints(&self) -> Vec<(&'static str, &'static str)> {
        Vec::new()
    }
}
