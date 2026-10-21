//! Executions drawn like the console draws them: the state machine's graph with each
//! state colored by what happened to it, and a timeline of when each state ran.

use std::collections::{HashMap, HashSet};

use ratatui::style::Style;
use ratatui::text::{Line, Span};

use crate::asl::{Flow, Item, Machine, State};
use crate::aws::sfn::{HistoryEvent, Step, StepStatus};
use crate::tui::theme;
use crate::util::{ellipsize, fmt_duration};

// ---- what happened in an execution -----------------------------------------------------

/// One state in an execution, over all its visits (loops, Map items).
#[derive(Debug, Clone)]
pub struct Run {
    pub visits: usize,
    pub succeeded: usize,
    pub failed: usize,
    pub retries: usize,
    /// When each visit started and ended (no end while it runs).
    pub spans: Vec<(f64, Option<f64>)>,
    /// The status of the last visit.
    last_status: StepStatus,
    running: usize,
    /// Index in the steps of the last visit.
    pub last: usize,
    /// The error of the last visit that failed.
    pub error: Option<String>,
}

impl Run {
    pub fn status(&self) -> StepStatus {
        if self.running > 0 {
            StepStatus::Running
        } else if self.failed > 0 {
            StepStatus::Failed
        } else {
            self.last_status
        }
    }

    /// Time spent in the state: its visits together, overlapping ones (Map items) counted once.
    pub fn busy(&self, now: f64) -> f64 {
        let mut spans: Vec<(f64, f64)> = self.spans.iter().map(|(s, e)| (*s, e.unwrap_or(now))).collect();
        spans.sort_by(|a, b| a.0.total_cmp(&b.0));
        let (mut total, mut reached) = (0.0, f64::MIN);
        for (s, e) in spans {
            let s = s.max(reached);
            if e > s {
                total += e - s;
                reached = e;
            }
        }
        total
    }
}

/// What happened to each state, and the transitions taken (which Choice arm, which catch).
#[derive(Debug, Default)]
pub struct Trace {
    pub runs: HashMap<String, Run>,
    pub taken: HashSet<(String, String)>,
}

impl Trace {
    pub fn new(steps: &[Step], events: &[HistoryEvent]) -> Trace {
        let mut runs: HashMap<String, Run> = HashMap::new();
        for (i, s) in steps.iter().enumerate() {
            let run = runs.entry(s.name.clone()).or_insert(Run {
                visits: 0,
                succeeded: 0,
                failed: 0,
                retries: 0,
                spans: Vec::new(),
                last_status: s.status,
                running: 0,
                last: i,
                error: None,
            });
            run.visits += 1;
            run.retries += s.retries;
            run.last = i;
            run.last_status = s.status;
            match s.status {
                StepStatus::Running => run.running += 1,
                StepStatus::Succeeded => run.succeeded += 1,
                StepStatus::Failed => run.failed += 1,
                StepStatus::Caught => {}
            }
            if matches!(s.status, StepStatus::Failed | StepStatus::Caught) {
                run.error = s.error.clone();
            }
            if let Some(start) = s.start {
                run.spans.push((start, s.end));
            }
        }
        let exited: HashMap<i64, &str> = events
            .iter()
            .filter(|e| e.kind.ends_with("StateExited"))
            .filter_map(|e| Some((e.id, e.state_exited_event_details.as_ref()?.name.as_str())))
            .collect();
        let taken = events
            .iter()
            .filter_map(|e| {
                let to = &e.state_entered_event_details.as_ref()?.name;
                Some((exited.get(&e.previous_event_id)?.to_string(), to.clone()))
            })
            .collect();
        Trace { runs, taken }
    }
}

pub fn status_look(status: Option<StepStatus>, tick: usize) -> (&'static str, Style) {
    match status {
        None => ("·", theme::muted()),
        Some(StepStatus::Running) => (theme::spinner(tick), Style::new().fg(theme::RUNNING)),
        Some(StepStatus::Succeeded) => ("✓", Style::new().fg(theme::OK)),
        Some(StepStatus::Failed) => ("✗", Style::new().fg(theme::ERR)),
        Some(StepStatus::Caught) => ("↷", Style::new().fg(theme::WARN)),
    }
}

// ---- a canvas of box-drawing lines -----------------------------------------------------

const U: u8 = 1;
const D: u8 = 2;
const L: u8 = 4;
const R: u8 = 8;

#[derive(Clone, Copy, PartialEq)]
enum Cell {
    Empty,
    /// Which sides the line leaves through; crossing lines merge into ┼, ├, ┴…
    Line(u8, Style),
    Text(char, Style),
}

/// The character for each combination of sides, indexed by the bits.
const GLYPHS: [char; 16] = [' ', '│', '│', '│', '─', '╯', '╮', '┤', '─', '╰', '╭', '├', '─', '┴', '┬', '┼'];

pub struct Canvas {
    pub w: usize,
    pub h: usize,
    cells: Vec<Cell>,
}

impl Canvas {
    fn new(w: usize, h: usize) -> Canvas {
        Canvas { w, h, cells: vec![Cell::Empty; w * h] }
    }

    fn line(&mut self, x: usize, y: usize, bits: u8, style: Style) {
        if x < self.w && y < self.h {
            let cell = &mut self.cells[y * self.w + x];
            *cell = match *cell {
                Cell::Line(b, _) => Cell::Line(b | bits, style),
                _ => Cell::Line(bits, style),
            };
        }
    }

    /// A horizontal line whose ends turn into corners or junctions with what they meet.
    fn hline(&mut self, x0: usize, x1: usize, y: usize, style: Style) {
        for x in x0..=x1 {
            self.line(x, y, if x > x0 { L } else { 0 } | if x < x1 { R } else { 0 }, style);
        }
    }

    fn vline(&mut self, x: usize, y0: usize, y1: usize, style: Style) {
        for y in y0..=y1 {
            self.line(x, y, if y > y0 { U } else { 0 } | if y < y1 { D } else { 0 }, style);
        }
    }

    /// A vertical line that also continues above `y0` and below `y1`.
    fn through(&mut self, x: usize, y0: usize, y1: usize, style: Style) {
        for y in y0..=y1 {
            self.line(x, y, U | D, style);
        }
    }

    fn frame(&mut self, w: usize, h: usize, style: Style) {
        self.hline(0, w - 1, 0, style);
        self.hline(0, w - 1, h - 1, style);
        self.vline(0, 0, h - 1, style);
        self.vline(w - 1, 0, h - 1, style);
    }

    /// Writes `text` from `x`, at most `max` characters.
    fn text(&mut self, x: usize, y: usize, text: &str, max: usize, style: Style) {
        for (i, c) in ellipsize(text, max).chars().enumerate() {
            if x + i < self.w && y < self.h {
                self.cells[y * self.w + x + i] = Cell::Text(c, style);
            }
        }
    }

    fn blit(&mut self, src: &Canvas, dx: usize, dy: usize) {
        for y in 0..src.h {
            for x in 0..src.w {
                match src.cells[y * src.w + x] {
                    Cell::Empty => {}
                    Cell::Line(bits, style) => self.line(dx + x, dy + y, bits, style),
                    cell => {
                        if dx + x < self.w && dy + y < self.h {
                            self.cells[(dy + y) * self.w + dx + x] = cell;
                        }
                    }
                }
            }
        }
    }

    /// The part of the canvas from (`x0`, `y0`), `w` × `h` cells.
    pub fn lines(&self, x0: usize, y0: usize, w: usize, h: usize) -> Vec<Line<'static>> {
        (y0..(y0 + h).min(self.h))
            .map(|y| {
                let mut spans = Vec::new();
                let (mut text, mut style) = (String::new(), Style::new());
                for x in x0..(x0 + w).min(self.w) {
                    let (c, s) = match self.cells[y * self.w + x] {
                        Cell::Empty => (' ', Style::new()),
                        Cell::Line(bits, s) => (GLYPHS[usize::from(bits & 15)], s),
                        Cell::Text(c, s) => (c, s),
                    };
                    if s != style && !text.is_empty() {
                        spans.push(Span::styled(std::mem::take(&mut text), style));
                    }
                    style = s;
                    text.push(c);
                }
                spans.push(Span::styled(text, style));
                Line::from(spans)
            })
            .collect()
    }
}

// ---- the graph -------------------------------------------------------------------------

/// Where a state's box was drawn.
#[derive(Debug, Clone)]
pub struct Node {
    pub name: String,
    pub x: usize,
    pub y: usize,
    pub w: usize,
    pub h: usize,
}

/// A drawn piece of the graph. The flow enters at column `x` and, when `exits`, leaves
/// below it at the same column, so pieces stack into a straight line.
struct Block {
    canvas: Canvas,
    x: usize,
    exits: bool,
    nodes: Vec<Node>,
}

impl Block {
    fn empty() -> Block {
        Block { canvas: Canvas::new(1, 0), x: 0, exits: true, nodes: Vec::new() }
    }

    fn place(&mut self, other: Block, dx: usize, dy: usize) {
        self.canvas.blit(&other.canvas, dx, dy);
        self.nodes.extend(other.nodes.into_iter().map(|n| Node { x: n.x + dx, y: n.y + dy, ..n }));
    }
}

/// Blocks one under the other, their entry columns lined up.
fn stack(blocks: Vec<Block>) -> Block {
    let Some(exits) = blocks.last().map(|b| b.exits) else { return Block::empty() };
    let x = blocks.iter().map(|b| b.x).max().unwrap_or(0);
    let w = blocks.iter().map(|b| x - b.x + b.canvas.w).max().unwrap_or(1);
    let h = blocks.iter().map(|b| b.canvas.h).sum();
    let mut out = Block { canvas: Canvas::new(w, h), x, exits, nodes: Vec::new() };
    let mut y = 0;
    for b in blocks {
        let (dx, bh) = (x - b.x, b.canvas.h);
        out.place(b, dx, y);
        y += bh;
    }
    out
}

/// The widest a state's box gets, inside its borders.
const BOX_MAX: usize = 32;
const LABEL_MAX: usize = 28;
/// Room kept for the duration, so boxes do not change size while an execution runs.
const DURATION_W: usize = 7;

struct Painter<'a> {
    states: HashMap<&'a str, &'a State>,
    trace: &'a Trace,
    now: f64,
    tick: usize,
    selected: Option<&'a str>,
}

/// Draws the graph of `machine` (arranged as `flow`) with what `trace` says happened.
pub fn graph(machine: &Machine, flow: &Flow, trace: &Trace, now: f64, tick: usize, selected: Option<&str>) -> (Canvas, Vec<Node>) {
    let painter = Painter { states: machine.all().into_iter().map(|(s, _)| (s.name.as_str(), s)).collect(), trace, now, tick, selected };
    let block = stack(vec![painter.marker("Start", true), painter.flow(flow)]);
    (block.canvas, block.nodes)
}

impl Painter<'_> {
    fn look(&self, name: &str) -> (&'static str, Style) {
        status_look(self.trace.runs.get(name).map(Run::status), self.tick)
    }

    /// The lines of a state's box (or of a Parallel / Map frame's header): its name with
    /// the time it took, what it does, and the error that stopped it.
    fn header(&self, name: &str) -> (usize, Vec<(String, Style)>, Option<String>) {
        let state = self.states.get(name);
        let run = self.trace.runs.get(name);
        let mut what = state.map(|s| s.summary.clone()).unwrap_or_default();
        if let Some(r) = run {
            if r.visits > 1 {
                what.push_str(&format!(" · ×{}", r.visits));
                if r.failed > 0 && r.succeeded > 0 {
                    what.push_str(&format!(" ✗{}", r.failed));
                }
            }
            if r.retries > 0 {
                what.push_str(&format!(" · ↻{}", r.retries));
            }
        }
        let mut lines = vec![(what, theme::muted())];
        let status = run.map(Run::status);
        if matches!(status, Some(StepStatus::Failed | StepStatus::Caught)) {
            let style = Style::new().fg(if status == Some(StepStatus::Failed) { theme::ERR } else { theme::WARN });
            if let Some(e) = run.and_then(|r| r.error.clone()) {
                lines.push((e, style));
            }
        }
        let width = lines.iter().map(|(t, _)| t.chars().count() + 2).chain([name.chars().count() + 3 + DURATION_W]).max().unwrap_or(10);
        let duration = run.map(|r| fmt_duration(r.busy(self.now)));
        (width.clamp(12, BOX_MAX), lines, duration)
    }

    /// Writes a header at row 1 of `canvas`, inside a box of `inner` columns.
    fn write_header(&self, canvas: &mut Canvas, name: &str, inner: usize, lines: &[(String, Style)], duration: Option<String>) {
        let (symbol, style) = self.look(name);
        let selected = self.selected == Some(name);
        canvas.text(2, 1, symbol, 1, style);
        let duration = duration.unwrap_or_default();
        let name_style = if selected { theme::selected() } else if self.trace.runs.contains_key(name) { theme::bold() } else { Style::new() };
        canvas.text(4, 1, name, inner.saturating_sub(3 + duration.chars().count()), name_style);
        canvas.text(2 + inner - duration.chars().count(), 1, &duration, DURATION_W, theme::muted());
        for (i, (text, style)) in lines.iter().enumerate().filter(|(_, (t, _))| !t.is_empty()) {
            canvas.text(4, 2 + i, text, inner - 2, *style);
        }
    }

    fn border(&self, name: &str) -> Style {
        if self.selected == Some(name) {
            return Style::new().fg(theme::ACCENT).bold();
        }
        match self.trace.runs.get(name) {
            Some(_) => self.look(name).1,
            None => theme::muted(),
        }
    }

    fn node(&self, name: &str, exits: bool) -> Block {
        let (inner, lines, duration) = self.header(name);
        let rows = lines.iter().filter(|(t, _)| !t.is_empty()).count();
        let (w, h) = (inner + 4, 3 + rows);
        let mut canvas = Canvas::new(w, h);
        let style = self.border(name);
        canvas.frame(w, h, style);
        let x = w / 2;
        canvas.line(x, 0, U, style);
        if exits {
            canvas.line(x, h - 1, D, style);
        }
        self.write_header(&mut canvas, name, inner, &lines, duration);
        Block { canvas, x, exits, nodes: vec![Node { name: name.into(), x: 0, y: 0, w, h }] }
    }

    /// A Parallel or Map state: a frame with its header and the flows inside.
    fn container(&self, name: &str, inner: &[Flow], exits: bool) -> Block {
        let (header_w, lines, duration) = self.header(name);
        let rows = lines.iter().filter(|(t, _)| !t.is_empty()).count();
        let content = match inner {
            [one] => self.flow(one),
            many => self.fork(many.iter().map(|f| (String::new(), self.flow(f), f.joins, false)).collect(), false),
        };
        let top = 2 + rows;
        let (cw, ch) = (content.canvas.w, content.canvas.h);
        let w = (cw + 4).max(header_w + 4);
        let h = top + 1 + ch + 2;
        let x = 2 + content.x;
        let style = self.border(name);
        let mut canvas = Canvas::new(w, h);
        canvas.frame(w, h, style);
        canvas.line(x, 0, U, style);
        canvas.through(x, top, top, theme::muted());
        if content.exits {
            canvas.through(x, top + 1 + ch, top + 1 + ch, theme::muted());
            canvas.line(x, h - 1, U, style);
        }
        if exits {
            canvas.line(x, h - 1, D, style);
        }
        self.write_header(&mut canvas, name, w - 4, &lines, duration);
        let mut block = Block { canvas, x, exits, nodes: vec![Node { name: name.into(), x: 0, y: 0, w, h }] };
        block.place(content, 2, top + 1);
        block
    }

    /// Arms side by side under a fork line, joined again below by those that continue.
    /// Each arm: its label, its block, whether it joins, and whether the execution took it.
    fn fork(&self, arms: Vec<(String, Block, bool, bool)>, labels: bool) -> Block {
        const GAP: usize = 3;
        let widths: Vec<usize> = arms
            .iter()
            .map(|(label, b, ..)| b.canvas.w.max(if labels { label.chars().count().min(LABEL_MAX) } else { 1 }))
            .collect();
        let (mut columns, mut anchors, mut col) = (Vec::new(), Vec::new(), 0);
        for ((_, b, ..), w) in arms.iter().zip(&widths) {
            let bx = col + (w - b.canvas.w) / 2;
            columns.push((col, bx));
            anchors.push(bx + b.x);
            col += w + GAP;
        }
        let width = col.saturating_sub(GAP).max(1);
        let joining: Vec<usize> = (0..arms.len()).filter(|&i| arms[i].2).collect();
        let x = joining.first().map_or((anchors[0] + anchors[anchors.len() - 1]) / 2, |&i| anchors[i]);
        let top = if labels { 2 } else { 1 };
        let body = arms.iter().map(|(_, b, ..)| b.canvas.h).max().unwrap_or(0);
        let join_row = top + body;
        let h = join_row + usize::from(!joining.is_empty());
        let edge = theme::muted();
        let mut out = Block { canvas: Canvas::new(width, h), x, exits: !joining.is_empty(), nodes: Vec::new() };

        let lo = anchors.iter().copied().chain([x]).min().unwrap_or(x);
        let hi = anchors.iter().copied().chain([x]).max().unwrap_or(x);
        out.canvas.hline(lo, hi, 0, edge);
        out.canvas.line(x, 0, U, edge);
        for &a in &anchors {
            out.canvas.line(a, 0, D, edge);
        }
        for (i, (label, block, joins, taken)) in arms.into_iter().enumerate() {
            let (a, (col, bx)) = (anchors[i], columns[i]);
            if labels {
                if label.is_empty() {
                    out.canvas.through(a, 1, 1, edge);
                } else {
                    let len = label.chars().count().min(LABEL_MAX);
                    let start = a.saturating_sub(len / 2).clamp(col, col + widths[i] - len);
                    let style = if taken { Style::new().fg(theme::ACCENT).bold() } else { theme::muted() };
                    out.canvas.text(start, 1, &label, LABEL_MAX, style);
                }
            }
            let bh = block.canvas.h;
            out.place(block, bx, top);
            if joins && top + bh < join_row {
                out.canvas.through(a, top + bh, join_row - 1, edge);
            }
        }
        if !joining.is_empty() {
            let lo = joining.iter().map(|&i| anchors[i]).chain([x]).min().unwrap_or(x);
            let hi = joining.iter().map(|&i| anchors[i]).chain([x]).max().unwrap_or(x);
            out.canvas.hline(lo, hi, join_row, edge);
            for &i in &joining {
                out.canvas.line(anchors[i], join_row, U, edge);
            }
            out.canvas.line(x, join_row, D, edge);
        }
        out
    }

    fn items(&self, items: &[Item], joins: bool) -> Block {
        let mut blocks = Vec::new();
        let mut last_state: Option<&str> = None;
        for (i, item) in items.iter().enumerate() {
            let exits = i + 1 < items.len() || joins;
            blocks.push(match item {
                Item::State { name, inner } => {
                    last_state = Some(name);
                    let container = self.states.get(name.as_str()).is_some_and(|s| matches!(s.kind.as_str(), "Parallel" | "Map"));
                    if container && !inner.is_empty() { self.container(name, inner, exits) } else { self.node(name, exits) }
                }
                Item::Split(arms) => {
                    let from = last_state.unwrap_or_default();
                    let arms = arms
                        .iter()
                        .map(|a| {
                            let taken = self.trace.taken.contains(&(from.to_string(), a.target.clone()));
                            (a.label.clone(), self.items(&a.items, a.joins), a.joins, taken)
                        })
                        .collect();
                    self.fork(arms, true)
                }
                Item::Goto { target, back } => self.goto(target, *back),
                Item::End => self.marker("End", false),
            });
        }
        stack(blocks)
    }

    /// A flow, with its catch handlers to the right of it.
    fn flow(&self, flow: &Flow) -> Block {
        let main = self.items(&flow.main, flow.joins);
        if flow.handlers.is_empty() {
            return main;
        }
        const GAP: usize = 4;
        let handlers: Vec<Block> = flow
            .handlers
            .iter()
            .map(|(from, items)| stack(vec![self.marker(&format!("catch from {}", from.join(", ")), true), self.items(items, false)]))
            .collect();
        let w = main.canvas.w + handlers.iter().map(|b| GAP + b.canvas.w).sum::<usize>();
        let h = handlers.iter().map(|b| b.canvas.h).chain([main.canvas.h]).max().unwrap_or(0);
        let mut out = Block { canvas: Canvas::new(w, h), x: main.x, exits: main.exits, nodes: Vec::new() };
        let (mx, mw, mh, exits) = (main.x, main.canvas.w, main.canvas.h, main.exits);
        out.place(main, 0, 0);
        // Inside a Parallel or Map frame, the main flow continues down to the frame's way out.
        if exits && mh < h {
            out.canvas.through(mx, mh, h - 1, theme::muted());
        }
        let mut x = mw + GAP;
        for b in handlers {
            let bw = b.canvas.w;
            out.place(b, x, 0);
            x += bw + GAP;
        }
        out
    }

    /// `◉ Start`, `◉ End`, `◉ catch from …`.
    fn marker(&self, text: &str, exits: bool) -> Block {
        let label = ellipsize(text, 40);
        let mut canvas = Canvas::new(label.chars().count() + 2, 1);
        canvas.text(0, 0, "◉", 1, theme::muted());
        canvas.text(2, 0, &label, 40, theme::muted());
        Block { canvas, x: 0, exits, nodes: Vec::new() }
    }

    /// `╰─▶ HandleError`, or `╰─↺ Poll` for a loop back.
    fn goto(&self, target: &str, back: bool) -> Block {
        let name = ellipsize(target, LABEL_MAX);
        let mut canvas = Canvas::new(name.chars().count() + 4, 1);
        canvas.line(0, 0, U | R, theme::muted());
        canvas.line(1, 0, L | R, theme::muted());
        canvas.text(2, 0, if back { "↺" } else { "▶" }, 1, theme::muted());
        let style = if self.trace.runs.contains_key(target) { self.look(target).1 } else { theme::muted() };
        canvas.text(4, 0, &name, LABEL_MAX, style);
        Block { canvas, x: 0, exits: false, nodes: Vec::new() }
    }
}

// ---- the timeline ----------------------------------------------------------------------

/// A state that ran, for the timeline, and how deeply it is nested (Parallel, Map).
pub struct Row {
    pub name: String,
    pub depth: usize,
}

/// The states that ran, in the order they first started.
pub fn rows(machine: Option<&Machine>, trace: &Trace) -> Vec<Row> {
    let depth: HashMap<&str, usize> = machine.map(|m| m.all().into_iter().map(|(s, d)| (s.name.as_str(), d)).collect()).unwrap_or_default();
    let mut rows: Vec<(f64, &String)> =
        trace.runs.iter().map(|(name, r)| (r.spans.iter().map(|s| s.0).fold(f64::MAX, f64::min), name)).collect();
    rows.sort_by(|a, b| a.0.total_cmp(&b.0).then_with(|| a.1.cmp(b.1)));
    rows.into_iter().map(|(_, name)| Row { name: name.clone(), depth: depth.get(name.as_str()).copied().unwrap_or(0) }).collect()
}

/// Left to right: how much of a cell a bar covers, in eighths.
const EIGHTHS: [char; 8] = [' ', '▏', '▎', '▍', '▌', '▋', '▊', '▉'];

/// The timeline of `rows` between `t0` and `t1` in `width` columns: the time axis, and a
/// line per state with a bar for each of its visits.
pub fn timeline(
    rows: &[Row],
    trace: &Trace,
    (t0, t1): (f64, f64),
    width: usize,
    selected: Option<usize>,
    now: f64,
    tick: usize,
) -> (Line<'static>, Vec<Line<'static>>) {
    let name_w = rows.iter().map(|r| r.depth * 2 + r.name.chars().count()).max().unwrap_or(4).clamp(8, 32);
    let prefix = 4 + name_w + 1 + 8 + 1 + 5 + 1;
    let bar_w = width.saturating_sub(prefix).max(10);
    let span = (t1 - t0).max(0.001);

    let mut axis = vec![' '; bar_w];
    for k in 0..=4 {
        let label: Vec<char> = fmt_duration(span * k as f64 / 4.0).chars().collect();
        let x = (k * bar_w / 4).min(bar_w.saturating_sub(label.len()));
        if axis[x.saturating_sub(1)..(x + label.len()).min(bar_w)].iter().all(|c| *c == ' ') {
            axis[x..x + label.len()].copy_from_slice(&label);
        }
    }
    let header = Line::styled(format!("{}{}", " ".repeat(prefix), axis.into_iter().collect::<String>()), theme::muted());

    let lines = rows
        .iter()
        .enumerate()
        .filter_map(|(i, row)| {
            let run = trace.runs.get(&row.name)?;
            let (symbol, style) = status_look(Some(run.status()), tick);
            // How much of each cell the bars cover, in eighths: from its left edge (where a
            // bar ends) and from its right edge (where a bar starts).
            let mut cells = vec![(0u8, 0u8); bar_w];
            let eighths = |x: f64| ((x * 8.0).round() as u8).max(1);
            for (s, e) in &run.spans {
                let a = ((s - t0) / span * bar_w as f64).clamp(0.0, bar_w as f64 - 0.125);
                let b = ((e.unwrap_or(now) - t0) / span * bar_w as f64).clamp(a + 0.125, bar_w as f64);
                for (c, cell) in cells.iter_mut().enumerate().take(b.ceil() as usize).skip(a.floor() as usize) {
                    let (lo, hi) = (a.max(c as f64) - c as f64, b.min(c as f64 + 1.0) - c as f64);
                    if lo < 0.06 || hi < 0.94 {
                        cell.0 = cell.0.max(eighths(hi - lo));
                    } else {
                        cell.1 = cell.1.max(eighths(1.0 - lo));
                    }
                }
            }
            let bar: String = cells
                .into_iter()
                .map(|(l, r)| match (l, r) {
                    (0, 0) => ' ',
                    (l, r) if l + r >= 7 => '█',
                    (l, 0) => EIGHTHS[usize::from(l)],
                    (0, r) if r >= 3 => '▐',
                    (0, _) => '▕',
                    _ => '█',
                })
                .collect();
            let extra = if run.visits > 1 {
                format!("×{}", run.visits)
            } else if run.retries > 0 {
                format!("↻{}", run.retries)
            } else {
                String::new()
            };
            let selected = selected == Some(i);
            let name = ellipsize(&format!("{}{}", "  ".repeat(row.depth), row.name), name_w);
            let mut line = Line::from(vec![
                Span::styled(if selected { "▶ " } else { "  " }, theme::accent()),
                Span::styled(format!("{symbol} "), style),
                Span::styled(format!("{name:<name_w$} "), if selected { theme::bold() } else { Style::new() }),
                Span::styled(format!("{:>8} ", fmt_duration(run.busy(now))), theme::muted()),
                Span::styled(format!("{extra:<5} "), Style::new().fg(theme::WARN)),
                Span::styled(bar, style),
            ]);
            if selected {
                line = line.patch_style(theme::selected());
            }
            Some(line)
        })
        .collect();
    (header, lines)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::asl::{Machine, arrange};

    fn text(canvas: &Canvas) -> Vec<String> {
        canvas
            .lines(0, 0, canvas.w, canvas.h)
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect::<String>().trim_end().to_string())
            .collect()
    }

    #[test]
    fn draws_a_choice_that_joins_again() {
        let m = Machine::parse(
            r#"{"StartAt": "Check", "States": {
                "Check": {"Type": "Choice", "Choices": [{"Variable": "$.ok", "BooleanEquals": true, "Next": "Ship"}], "Default": "Hold"},
                "Ship": {"Type": "Pass", "Next": "Done"}, "Hold": {"Type": "Pass", "Next": "Done"},
                "Done": {"Type": "Succeed"}}}"#,
        )
        .unwrap();
        let (canvas, nodes) = graph(&m, &arrange(&m, false), &Trace::default(), 0.0, 0, None);
        let names: Vec<&str> = nodes.iter().map(|n| n.name.as_str()).collect();
        assert_eq!(names, ["Check", "Ship", "Hold", "Done"]);
        let drawn = text(&canvas).join("\n");
        assert_eq!(
            drawn,
            [
                "         ◉ Start",
                "╭────────┴────────╮",
                "│ · Check         │",
                "│   choice        │",
                "╰────────┬────────╯",
                "         ├────────────────────╮",
                "     ok = true             default",
                "╭────────┴───────╮   ╭────────┴───────╮",
                "│ · Ship         │   │ · Hold         │",
                "│   pass         │   │   pass         │",
                "╰────────┬───────╯   ╰────────┬───────╯",
                "         ├────────────────────╯",
                "╭────────┴───────╮",
                "│ · Done         │",
                "│   succeed      │",
                "╰────────┬───────╯",
                "         ◉ End",
            ]
            .join("\n")
        );
    }

    #[test]
    fn timeline_bars_follow_the_clock() {
        let step = |name: &str, start: f64, end: f64| Step {
            name: name.into(),
            kind: "Task".into(),
            status: StepStatus::Succeeded,
            start: Some(start),
            end: Some(end),
            resource: None,
            lambda: None,
            retries: 0,
            error: None,
            cause: None,
            input: None,
            output: None,
        };
        let trace = Trace::new(&[step("A", 0.0, 2.0), step("B", 2.0, 4.0)], &[]);
        let rows = rows(None, &trace);
        let (_, lines) = timeline(&rows, &trace, (0.0, 4.0), 24 + 10, None, 4.0, 0);
        let bars: Vec<String> = lines.iter().map(|l| l.spans.last().unwrap().content.to_string()).collect();
        assert_eq!(bars, ["█████     ", "     █████"]);
    }
}
