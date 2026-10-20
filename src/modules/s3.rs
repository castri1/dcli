//! S3 · buckets, folders and objects, previews, presigned links, downloads
//! (Being built: scaffold.)

use ratatui::Frame;
use ratatui::crossterm::event::KeyEvent;
use ratatui::layout::Rect;
use ratatui::text::Line;

use crate::tui::module::{Ctx, Handled, Module, Payload, View};
use crate::tui::{theme, widgets};

pub struct S3 {}

impl S3 {
    pub fn new() -> S3 {
        S3 {}
    }
}

impl Module for S3 {
    fn title(&self) -> &'static str {
        "S3"
    }

    fn description(&self) -> &'static str {
        "S3 · buckets, folders and objects, previews, presigned links, downloads"
    }

    fn reset(&mut self, _ctx: &mut Ctx) {}

    fn key(&mut self, _key: KeyEvent, _ctx: &mut Ctx) -> Handled {
        Handled::No
    }

    fn task(&mut self, _payload: Payload, _ctx: &mut Ctx) {}

    fn render(&mut self, f: &mut Frame, area: Rect, _view: &View) {
        f.render_widget(
            widgets::placeholder(Line::styled("Coming soon", theme::muted())).block(widgets::panel(" S3 ", true)),
            area,
        );
    }

    fn hints(&self) -> Vec<(&'static str, &'static str)> {
        Vec::new()
    }
}
