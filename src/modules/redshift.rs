//! Redshift: clusters, their nodes and performance, running and queued queries, table health; pause, resize, snapshots.

use ratatui::Frame;
use ratatui::crossterm::event::KeyEvent;
use ratatui::layout::Rect;
use ratatui::text::Line;

use crate::tui::module::{Ctx, Handled, Module, Payload, View};
use crate::tui::{theme, widgets};

pub struct Redshift;

impl Redshift {
    pub fn new() -> Redshift {
        Redshift
    }
}

impl Module for Redshift {
    fn title(&self) -> &'static str {
        "Redshift"
    }

    fn description(&self) -> &'static str {
        "Redshift clusters · nodes, queries, WLM queue, table health"
    }

    fn reset(&mut self, _ctx: &mut Ctx) {}

    fn key(&mut self, _key: KeyEvent, _ctx: &mut Ctx) -> Handled {
        Handled::No
    }

    fn task(&mut self, _payload: Payload, _ctx: &mut Ctx) {}

    fn render(&mut self, f: &mut Frame, area: Rect, _view: &View) {
        let text = Line::styled("Not built yet", theme::muted());
        f.render_widget(widgets::placeholder(text).block(widgets::panel(" Redshift ", false)), area);
    }

    fn hints(&self) -> Vec<(&'static str, &'static str)> {
        Vec::new()
    }
}
