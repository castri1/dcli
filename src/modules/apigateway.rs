//! API Gateway: REST, HTTP and WebSocket APIs, their routes, stages, metrics; test a route.

use ratatui::Frame;
use ratatui::crossterm::event::KeyEvent;
use ratatui::layout::Rect;
use ratatui::text::Line;

use crate::tui::module::{Ctx, Handled, Module, Payload, View};
use crate::tui::{theme, widgets};

pub struct ApiGateway;

impl ApiGateway {
    pub fn new() -> ApiGateway {
        ApiGateway
    }
}

impl Module for ApiGateway {
    fn title(&self) -> &'static str {
        "API Gateway"
    }

    fn description(&self) -> &'static str {
        "REST, HTTP and WebSocket APIs · routes, stages, metrics"
    }

    fn reset(&mut self, _ctx: &mut Ctx) {}

    fn key(&mut self, _key: KeyEvent, _ctx: &mut Ctx) -> Handled {
        Handled::No
    }

    fn task(&mut self, _payload: Payload, _ctx: &mut Ctx) {}

    fn render(&mut self, f: &mut Frame, area: Rect, _view: &View) {
        let text = Line::styled("Not built yet", theme::muted());
        f.render_widget(widgets::placeholder(text).block(widgets::panel(" API Gateway ", false)), area);
    }

    fn hints(&self) -> Vec<(&'static str, &'static str)> {
        Vec::new()
    }
}
