//! DMS: replication tasks and their tables, CDC latency, replication instances, endpoints; resume, reload, scale.

use ratatui::Frame;
use ratatui::crossterm::event::KeyEvent;
use ratatui::layout::Rect;
use ratatui::text::Line;

use crate::tui::module::{Ctx, Handled, Module, Payload, View};
use crate::tui::{theme, widgets};

pub struct Dms;

impl Dms {
    pub fn new() -> Dms {
        Dms
    }
}

impl Module for Dms {
    fn title(&self) -> &'static str {
        "DMS"
    }

    fn description(&self) -> &'static str {
        "Database Migration Service · tasks, tables, CDC latency, instances"
    }

    fn reset(&mut self, _ctx: &mut Ctx) {}

    fn key(&mut self, _key: KeyEvent, _ctx: &mut Ctx) -> Handled {
        Handled::No
    }

    fn task(&mut self, _payload: Payload, _ctx: &mut Ctx) {}

    fn render(&mut self, f: &mut Frame, area: Rect, _view: &View) {
        let text = Line::styled("Not built yet", theme::muted());
        f.render_widget(widgets::placeholder(text).block(widgets::panel(" DMS ", false)), area);
    }

    fn hints(&self) -> Vec<(&'static str, &'static str)> {
        Vec::new()
    }
}
