//! Advisor: the recommendations Claude leaves through `dcli mcp` for the environment, cost and
//! performance, each with what it saves or costs at the bill's rates, the evidence, and the
//! steps to copy. dcli runs none of them: the user copies, marks done (recorded in the savings
//! ledger, so the bill can confirm it) or dismisses with a reason Claude reads next time.

use ratatui::Frame;
use ratatui::crossterm::event::KeyEvent;
use ratatui::layout::Rect;
use ratatui::widgets::Paragraph;

use crate::tui::module::{Ctx, Handled, Module, Payload, View};

pub struct Advisor {}

impl Advisor {
    pub fn new() -> Advisor {
        Advisor {}
    }
}

impl Module for Advisor {
    fn title(&self) -> &'static str {
        "Advisor"
    }

    fn description(&self) -> &'static str {
        "Claude's recommendations (dcli mcp): cost and performance, with steps to copy"
    }

    fn reset(&mut self, _ctx: &mut Ctx) {}

    fn key(&mut self, _key: KeyEvent, _ctx: &mut Ctx) -> Handled {
        Handled::No
    }

    fn task(&mut self, _payload: Payload, _ctx: &mut Ctx) {}

    fn render(&mut self, f: &mut Frame, area: Rect, _view: &View) {
        f.render_widget(Paragraph::new("Advisor"), area);
    }

    fn hints(&self) -> Vec<(&'static str, &'static str)> {
        Vec::new()
    }
}
