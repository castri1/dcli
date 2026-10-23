//! Costs: what each environment's AWS account costs, by service and usage type, against last
//! month and the forecast; Savings Plans and reservations, their use and coverage; and cost
//! changes next to the deploys that came before them. From Cost Explorer (see `crate::costs`).

use ratatui::Frame;
use ratatui::crossterm::event::KeyEvent;
use ratatui::layout::Rect;
use ratatui::text::Line;

use crate::tui::module::{Ctx, Handled, Module, Payload, View};
use crate::tui::{theme, widgets};

pub struct Costs;

impl Costs {
    pub fn new() -> Costs {
        Costs
    }
}

impl Module for Costs {
    fn title(&self) -> &'static str {
        "Costs"
    }

    fn description(&self) -> &'static str {
        "What the account costs · services, usage types, commitments, cost after deploys"
    }

    fn reset(&mut self, _ctx: &mut Ctx) {}

    fn key(&mut self, _key: KeyEvent, _ctx: &mut Ctx) -> Handled {
        Handled::No
    }

    fn task(&mut self, _payload: Payload, _ctx: &mut Ctx) {}

    fn render(&mut self, f: &mut Frame, area: Rect, _view: &View) {
        let text = Line::styled("Not built yet", theme::muted());
        f.render_widget(widgets::placeholder(text).block(widgets::panel(" Costs ", false)), area);
    }

    fn hints(&self) -> Vec<(&'static str, &'static str)> {
        Vec::new()
    }
}
