//! Aurora: clusters, their topology (writer, readers, endpoints), performance and top SQL.

use ratatui::Frame;
use ratatui::crossterm::event::KeyEvent;
use ratatui::layout::Rect;
use ratatui::text::Line;

use crate::tui::module::{Ctx, Handled, Module, Payload, View};
use crate::tui::{theme, widgets};

pub struct Aurora;

impl Aurora {
    pub fn new() -> Aurora {
        Aurora
    }
}

impl Module for Aurora {
    fn title(&self) -> &'static str {
        "Aurora"
    }

    fn description(&self) -> &'static str {
        "Aurora clusters · topology, performance, top SQL"
    }

    fn reset(&mut self, _ctx: &mut Ctx) {}

    fn key(&mut self, _key: KeyEvent, _ctx: &mut Ctx) -> Handled {
        Handled::No
    }

    fn task(&mut self, _payload: Payload, _ctx: &mut Ctx) {}

    fn render(&mut self, f: &mut Frame, area: Rect, _view: &View) {
        let text = Line::styled("Not built yet", theme::muted());
        f.render_widget(widgets::placeholder(text).block(widgets::panel(" Aurora ", false)), area);
    }

    fn hints(&self) -> Vec<(&'static str, &'static str)> {
        Vec::new()
    }
}
