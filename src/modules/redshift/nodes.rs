//! A cluster's nodes drawn as a diagram: the leader node, which takes the connections and
//! plans the queries, with lines down to the compute nodes, which hold the data and run them;
//! each with its live CPU and disk use, a compute node far above the others flagged as skew.
//! When the diagram does not fit, the same as a list. Beside or below it, the selected
//! node's details.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use crate::aws::rds::Metrics;
use crate::aws::redshift::{self, CPU_SKEW_GAP, Cluster, DISK_SKEW_GAP, Health, Node, Skew};
use crate::tui::canvas::Canvas;
use crate::tui::chart::{Agg, Chart, columns, sparkline_to};
use crate::tui::{theme, widgets};
use crate::util::ellipsize;

/// What the diagram shows. Items are the cluster's nodes in order: the leader first.
pub struct Nodes<'a> {
    pub cluster: &'a Cluster,
    /// The last hour of each node's CPU and disk, and the cluster's connections and queue.
    pub pulse: Option<&'a Metrics>,
    pub selected: usize,
}

impl Nodes<'_> {
    fn key(&self, n: &Node) -> String {
        redshift::node_key(&self.cluster.id, n)
    }

    fn last(&self, n: &Node, id: &str) -> Option<f64> {
        self.pulse.and_then(|p| p.last(&self.key(n), id))
    }

    fn cluster_last(&self, id: &str) -> Option<f64> {
        self.pulse.and_then(|p| p.last(&self.cluster.id, id))
    }

    pub fn health(&self, n: &Node) -> Health {
        if self.cluster.status != "available" {
            return redshift::status_health(&self.cluster.status);
        }
        redshift::use_health(self.last(n, "cpu"), if n.leader() { None } else { self.last(n, "disk") })
    }

    /// The compute node well above the others in CPU, or else in disk use, over the hour.
    pub fn skew(&self) -> Option<(Skew, &'static str)> {
        let p = self.pulse?;
        redshift::skew(&redshift::node_values(self.cluster, p, "cpu", true), CPU_SKEW_GAP)
            .map(|s| (s, "CPU"))
            .or_else(|| redshift::skew(&redshift::node_values(self.cluster, p, "disk", true), DISK_SKEW_GAP).map(|s| (s, "disk")))
    }
}

/// Where an item was drawn, in canvas cells.
#[derive(Debug, Clone, PartialEq)]
pub struct Hit {
    pub item: usize,
    pub x: usize,
    pub y: usize,
    pub w: usize,
    pub h: usize,
}

pub fn health_color(h: Health) -> Color {
    match h {
        Health::Ok => theme::OK,
        Health::Changing => theme::RUNNING,
        Health::Paused => theme::MUTED,
        Health::Strained => theme::WARN,
        Health::Failing => theme::ERR,
    }
}

/// `██████░░░░`, red from the warning level.
fn gauge(percent: Option<f64>, width: usize) -> (String, Color) {
    let Some(p) = percent else { return ("·".repeat(width), theme::MUTED) };
    let filled = ((p / 100.0) * width as f64).round().clamp(0.0, width as f64) as usize;
    let color = health_color(redshift::use_health(None, Some(p)));
    (format!("{}{}", "█".repeat(filled), "░".repeat(width - filled)), color)
}

fn pct(v: Option<f64>) -> String {
    v.map(|v| format!("{v:.0}%")).unwrap_or_else(|| "—".into())
}

const GAP: usize = 2;
const NODE_MIN: usize = 22;
const NODE_MAX: usize = 30;
const NODE_H: usize = 5;

/// The diagram in at most `width` columns, with where each node was drawn; `None` when the
/// compute nodes do not fit in one row (then [`list`]).
pub fn diagram(t: &Nodes, width: usize) -> Option<(Canvas, Vec<Hit>)> {
    let nodes = &t.cluster.nodes;
    let leader = nodes.iter().position(Node::leader);
    let compute: Vec<usize> = (0..nodes.len()).filter(|i| Some(*i) != leader).collect();
    let n = compute.len().max(1);
    let node_w = (width.saturating_sub((n - 1) * GAP) / n).min(NODE_MAX);
    if node_w < NODE_MIN || nodes.is_empty() {
        return None;
    }
    let row_w = n * node_w + (n - 1) * GAP;
    let total = row_w.max(node_w);
    let (bus_y, node_y) = if leader.is_some() { (NODE_H + 1, NODE_H + 3) } else { (0, 0) };
    let mut canvas = Canvas::new(total, node_y + NODE_H);
    let mut hits = Vec::new();
    let skew = t.skew();
    let x0 = (total - row_w) / 2;

    if let Some(l) = leader {
        let x = (total - node_w) / 2;
        canvas.blit(&node_box(t, &nodes[l], l, node_w, None), x, 0);
        hits.push(Hit { item: l, x, y: 0, w: node_w, h: NODE_H });
        // The leader hands each compute node its part of the work.
        let line = theme::muted();
        let cx = x + node_w / 2;
        let centers: Vec<usize> = (0..compute.len()).map(|k| x0 + k * (node_w + GAP) + node_w / 2).collect();
        canvas.vline(cx, NODE_H - 1, bus_y, line);
        let lo = centers.iter().copied().chain([cx]).min().unwrap_or(cx);
        let hi = centers.iter().copied().chain([cx]).max().unwrap_or(cx);
        if hi > lo {
            canvas.hline(lo, hi, bus_y, line);
        }
        for c in centers {
            canvas.vline(c, bus_y, bus_y + 1, line);
            canvas.text(c, bus_y + 1, "▼", 1, line);
        }
    }
    for (k, &i) in compute.iter().enumerate() {
        let x = x0 + k * (node_w + GAP);
        let flagged = skew.as_ref().filter(|(s, _)| s.node == nodes[i].label()).map(|(_, what)| *what);
        canvas.blit(&node_box(t, &nodes[i], i, node_w, flagged), x, node_y);
        hits.push(Hit { item: i, x, y: node_y, w: node_w, h: NODE_H });
    }
    Some((canvas, hits))
}

fn node_box(t: &Nodes, n: &Node, item: usize, w: usize, skewed: Option<&str>) -> Canvas {
    let selected = t.selected == item;
    let border = if selected { Style::new().fg(theme::ACCENT).add_modifier(Modifier::BOLD) } else { Style::new().fg(Color::Gray) };
    let mut b = Canvas::new(w, NODE_H);
    b.frame(w, NODE_H, border);
    let inner = w - 4;
    // Which skew (CPU or disk) is in the details: the box keeps room for the name.
    let tag = if skewed.is_some() { " ⚠ skew ".to_string() } else { String::new() };
    let name_max = inner.saturating_sub(tag.chars().count() + 1);
    let name_style = if selected { theme::selected() } else { Style::new().add_modifier(Modifier::BOLD) };
    let name = if n.leader() { "leader".to_string() } else { n.label() };
    b.text(2, 0, &format!(" {} ", ellipsize(&name, name_max.saturating_sub(2))), name_max, name_style);
    if !tag.is_empty() {
        b.text(w - 1 - tag.chars().count(), 0, &tag, tag.chars().count(), Style::new().fg(theme::WARN).add_modifier(Modifier::BOLD));
    }
    // ● CPU ▂▃▅▇ 72%
    let cpu = t.last(n, "cpu");
    let cpu_text = pct(cpu);
    let spark_w = inner.saturating_sub(7 + cpu_text.len()).min(12);
    let spark = t.pulse.map(|p| sparkline_to(&columns(p.points(&t.key(n), "cpu"), p.window, spark_w, Agg::Mean), 100.0)).unwrap_or_default();
    let cpu_color = health_color(redshift::use_health(cpu, None));
    b.text(2, 1, "●", 1, Style::new().fg(health_color(t.health(n))));
    b.text(4, 1, "CPU", 3, theme::muted());
    b.text(8, 1, &spark, spark_w, Style::new().fg(cpu_color));
    b.text(9 + spark_w, 1, &cpu_text, cpu_text.len(), Style::new().fg(cpu_color));
    if n.leader() {
        let mut usage = t.cluster_last("connections").map(|c| format!("{c:.0} conn")).unwrap_or_else(|| "conn —".into());
        if let Some(q) = t.cluster_last("queued") {
            usage.push_str(&format!(" · {q:.0} queued"));
        }
        b.text(2, 2, &usage, inner, if t.cluster_last("queued").is_some_and(|q| q >= 1.0) { Style::new().fg(theme::WARN) } else { Style::new() });
        b.text(2, 3, "plans the queries", inner, theme::muted());
    } else {
        // disk ██████░░ 78%
        let disk = t.last(n, "disk");
        let disk_text = pct(disk);
        let bar_w = inner.saturating_sub(6 + disk_text.len()).min(12);
        let (bar, color) = gauge(disk, bar_w);
        b.text(2, 2, "disk", 4, theme::muted());
        b.text(7, 2, &bar, bar_w, Style::new().fg(color));
        b.text(8 + bar_w, 2, &disk_text, disk_text.len(), Style::new().fg(color));
        let where_ = n.private_ip.clone().unwrap_or_else(|| if t.cluster.paused() { "paused".into() } else { String::new() });
        b.text(2, 3, &where_, inner, theme::muted());
    }
    b
}

/// The nodes as lines, for terminals too narrow for the diagram, with the line each node is
/// on.
pub fn list(t: &Nodes, width: usize) -> (Vec<Line<'static>>, Vec<usize>) {
    let mut lines = vec![widgets::section("Nodes")];
    let mut rows = Vec::new();
    let skew = t.skew();
    for (i, n) in t.cluster.nodes.iter().enumerate() {
        rows.push(lines.len());
        let style = if t.selected == i { theme::selected() } else { Style::new() };
        let cpu = t.last(n, "cpu");
        let spark = t.pulse.map(|p| sparkline_to(&columns(p.points(&t.key(n), "cpu"), p.window, 8, Agg::Mean), 100.0)).unwrap_or_default();
        let mut spans = vec![
            Span::styled(if n.leader() { "◆ " } else { "● " }, Style::new().fg(health_color(t.health(n)))),
            Span::styled(format!("{:<10}", n.label()), style),
            Span::raw(format!(" CPU {spark} {:>4}", pct(cpu))),
        ];
        if !n.leader() {
            let disk = t.last(n, "disk");
            let (bar, color) = gauge(disk, 8);
            spans.push(Span::styled("  disk ", theme::muted()));
            spans.push(Span::styled(format!("{bar} {:>4}", pct(disk)), Style::new().fg(color)));
        } else if let Some(c) = t.cluster_last("connections") {
            spans.push(Span::styled(format!("  {c:.0} connections"), theme::muted()));
        }
        if let Some((s, what)) = skew.as_ref().filter(|(s, _)| s.node == n.label()) {
            spans.push(Span::styled(format!("  ⚠ {what} skew ({:.0}% vs {:.0}%)", s.value, s.others), Style::new().fg(theme::WARN)));
        }
        if width >= 90
            && let Some(ip) = &n.private_ip
        {
            spans.push(Span::styled(format!("  {ip}"), theme::muted()));
        }
        lines.push(Line::from(spans));
    }
    (lines, rows)
}

/// One line about the whole cluster, under the diagram.
pub fn summary(t: &Nodes) -> Line<'static> {
    let mut parts = Vec::new();
    if let Some(c) = t.cluster_last("cpu_all") {
        parts.push(format!("CPU {c:.0}%"));
    }
    if let Some(d) = t.cluster_last("disk_all") {
        parts.push(format!("disk {d:.0}%"));
    }
    if let Some(c) = t.cluster_last("connections") {
        parts.push(format!("{c:.0} connections"));
    }
    if let Some(q) = t.cluster_last("queued") {
        parts.push(format!("{q:.0} queued in WLM"));
    }
    if t.cluster.ra3() {
        parts.push("data in managed storage".into());
    }
    let mut spans = vec![Span::styled(format!("cluster · {}", parts.join(" · ")), theme::muted())];
    if t.cluster_last("health").is_some_and(|h| h < 0.5) {
        spans.push(Span::styled("  unhealthy", Style::new().fg(theme::ERR).add_modifier(Modifier::BOLD)));
    }
    Line::from(spans)
}

/// The selected node's details, `width` columns wide.
pub fn details(t: &Nodes, width: usize) -> Vec<Line<'static>> {
    let key_w = 10;
    let Some(n) = t.cluster.nodes.get(t.selected) else { return Vec::new() };
    let mut lines = vec![Line::from(vec![
        Span::styled("● ", Style::new().fg(health_color(t.health(n)))),
        Span::styled(n.label(), Style::new().add_modifier(Modifier::BOLD)),
        Span::styled(
            match n.role.as_str() {
                "LEADER" => "  leader node".to_string(),
                "SHARED" => "  leader and compute (single node)".to_string(),
                _ => format!("  compute node · {}", t.cluster.node_type),
            },
            theme::muted(),
        ),
    ])];
    lines.push(Line::raw(""));
    if let Some(ip) = &n.private_ip {
        lines.push(widgets::kv("Private IP", ip.clone(), key_w));
    }
    if let Some(ip) = &n.public_ip {
        lines.push(widgets::kv("Public IP", Span::styled(ip.clone(), Style::new().fg(theme::WARN)), key_w));
    }
    lines.push(widgets::kv("CloudWatch", format!("NodeID {}", n.metric_id()), key_w));
    if let Some((s, what)) = t.skew().filter(|(s, _)| s.node == n.label()) {
        lines.push(Line::raw(""));
        lines.push(Line::styled(
            format!("⚠ {what} {:.0}% while the other compute nodes average {:.0}%.", s.value, s.others),
            Style::new().fg(theme::WARN),
        ));
        lines.push(Line::styled("Data or work is spread unevenly: look at the distribution keys of the largest tables (Tables tab).", theme::muted()));
    }
    let Some(p) = t.pulse else { return lines };
    lines.push(Line::raw(""));
    lines.push(widgets::section("Last hour"));
    let chart_w = width.saturating_sub(1).max(20);
    let key = t.key(n);
    let mut chart = |title: &str, source: &str, id: &str, fmt: fn(f64) -> String, threshold: Option<f64>| {
        let points = p.points(source, id);
        if points.is_empty() {
            return;
        }
        let mut c = Chart::new(title, points, p.window);
        c.agg = Agg::Mean;
        c.rows = 2;
        c.fmt = fmt;
        c.threshold = threshold;
        c.axis = false;
        c.summary = vec![Span::styled(p.last(source, id).map(fmt).unwrap_or_default(), theme::muted())];
        lines.extend(c.lines(chart_w));
    };
    chart("CPU", &key, "cpu", |v| format!("{v:.0}%"), Some(redshift::CPU_WARN));
    if n.leader() {
        chart("Connections", &t.cluster.id, "connections", |v| format!("{v:.0}"), None);
        chart("Queued in WLM", &t.cluster.id, "queued", |v| format!("{v:.0}"), None);
    } else {
        chart("Disk used", &key, "disk", |v| format!("{v:.0}%"), Some(redshift::DISK_WARN));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cluster(compute: usize) -> Cluster {
        let mut nodes = vec![Node { role: "LEADER".into(), private_ip: Some("10.0.0.1".into()), public_ip: None }];
        nodes.extend((0..compute).map(|k| Node { role: format!("COMPUTE-{k}"), private_ip: Some(format!("10.0.0.{}", k + 2)), public_ip: None }));
        Cluster { id: "dw".into(), node_type: "ra3.4xlarge".into(), nodes_count: compute as u32, status: "available".into(), nodes, ..Default::default() }
    }

    fn text(canvas: &Canvas) -> Vec<String> {
        canvas.lines(0, 0, canvas.w, canvas.h).iter().map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect::<String>()).collect()
    }

    #[test]
    fn the_leader_reaches_every_compute_node_and_skew_is_flagged() {
        let c = cluster(3);
        let mut pulse = Metrics::new((0.0, 3600.0));
        for (node, cpu, disk) in [("Compute-0", 88.0, 78.0), ("Compute-1", 30.0, 45.0), ("Compute-2", 31.0, 46.0)] {
            pulse.insert(&format!("dw/{node}"), "cpu", vec![(1800.0, cpu), (3500.0, cpu)]);
            pulse.insert(&format!("dw/{node}"), "disk", vec![(3500.0, disk)]);
        }
        let t = Nodes { cluster: &c, pulse: Some(&pulse), selected: 1 };
        let (canvas, hits) = diagram(&t, 90).expect("three nodes fit in 90 columns");
        let rows = text(&canvas);
        assert_eq!(hits.iter().map(|h| (h.item, h.y)).collect::<Vec<_>>(), [(0, 0), (1, 8), (2, 8), (3, 8)]);
        assert_eq!(rows[NODE_H + 2].matches('▼').count(), 3, "{rows:#?}");
        assert!(rows[8].contains(" compute-0 ") && rows[8].contains("⚠ skew"), "{rows:#?}");
        assert_eq!(rows.iter().map(|r| r.matches("skew").count()).sum::<usize>(), 1, "{rows:#?}");
        assert!(rows.iter().any(|r| r.contains("disk ") && r.contains("78%")), "{rows:#?}");
        // Each arrow sits under the bus.
        let x = rows[NODE_H + 2].chars().position(|c| c == '▼').unwrap();
        assert!(matches!(rows[NODE_H + 1].chars().nth(x), Some('╭' | '┬' | '╮' | '┼' | '│')), "{rows:#?}");
    }

    #[test]
    fn too_many_nodes_fall_back_to_a_list() {
        let c = cluster(8);
        let t = Nodes { cluster: &c, pulse: None, selected: 0 };
        assert!(diagram(&t, 120).is_none());
        let (lines, rows) = list(&t, 120);
        assert_eq!(rows.len(), 9);
        let first: String = lines[rows[0]].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(first.starts_with("◆ leader"), "{first}");
        // A single node has no leader box and no lines.
        let mut solo = cluster(0);
        solo.nodes = vec![Node { role: "SHARED".into(), ..Default::default() }];
        let t = Nodes { cluster: &solo, pulse: None, selected: 0 };
        let (canvas, hits) = diagram(&t, 40).unwrap();
        assert_eq!((canvas.h, hits.len()), (NODE_H, 1));
    }
}
