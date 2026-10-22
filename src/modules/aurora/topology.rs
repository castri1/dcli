//! A cluster's topology drawn as a diagram: its endpoints at the top, each with lines to the
//! nodes it sends connections to; the nodes in a row, by Availability Zone, with their live
//! figures (CPU, connections, replica lag) and health; and the cluster volume they share.
//! When the diagram does not fit, the same as a list. Below or beside it, the selected
//! endpoint's or node's details.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use crate::aws::rds::{self, Cluster, Endpoint, EndpointKind, Health, Instance, Metrics};
use crate::tui::canvas::Canvas;
use crate::tui::chart::{Agg, Chart, columns, fmt_ms, sparkline_to};
use crate::tui::theme;
use crate::tui::widgets;
use crate::util::ellipsize;

/// What the diagram shows. Items are numbered endpoints first, then nodes.
pub struct Topology<'a> {
    pub cluster: &'a Cluster,
    /// The writer first, then the readers by failover priority.
    pub instances: &'a [&'a Instance],
    pub endpoints: &'a [&'a Endpoint],
    /// The last hour of CPU, connections, replica lag and capacity.
    pub pulse: Option<&'a Metrics>,
    pub selected: usize,
    /// Bytes in the cluster volume, when known.
    pub volume: Option<f64>,
}

impl Topology<'_> {
    /// The selected node, when a node (not an endpoint) is selected.
    pub fn node(&self, item: usize) -> Option<&Instance> {
        item.checked_sub(self.endpoints.len()).and_then(|n| self.instances.get(n)).copied()
    }

    fn last(&self, instance: &str, id: &str) -> Option<f64> {
        self.pulse.and_then(|p| p.last(instance, id))
    }

    fn health(&self, i: &Instance) -> Health {
        rds::node_health(&i.status, self.last(&i.id, "cpu"), self.pulse.and_then(|p| p.max(&i.id, "lag")))
    }

    fn routes(&self, e: &Endpoint) -> Vec<String> {
        rds::routes(e, self.instances)
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
        Health::Stopped => theme::MUTED,
        Health::Strained => theme::WARN,
        Health::Failing => theme::ERR,
    }
}

/// Each endpoint's colour, used for its box, its lines and its markers on the nodes.
pub fn endpoint_color(endpoints: &[&Endpoint], index: usize) -> Color {
    const CUSTOM: [Color; 4] = [Color::LightBlue, Color::Yellow, Color::LightGreen, Color::LightRed];
    match endpoints.get(index).map(|e| e.kind) {
        Some(EndpointKind::Writer) => Color::LightMagenta,
        Some(EndpointKind::Reader) => theme::ACCENT,
        _ => {
            let custom = endpoints[..index.min(endpoints.len())].iter().filter(|e| matches!(e.kind, EndpointKind::Custom { .. })).count();
            CUSTOM[custom % CUSTOM.len()]
        }
    }
}

/// `r6g.xlarge`, `Serverless v2`.
fn short_class(i: &Instance) -> String {
    if i.serverless() { "Serverless v2".into() } else { i.class.trim_start_matches("db.").to_string() }
}

const GAP: usize = 2;
const NODE_MIN: usize = 22;
const NODE_MAX: usize = 30;
const EP_MIN: usize = 18;
const EP_WANT: usize = 30;
const EP_MAX: usize = 36;
const EP_H: usize = 4;
const NODE_H: usize = 6;

/// Widths of the node boxes and the endpoint boxes in `width` columns, or `None` when the
/// diagram does not fit.
fn widths(nodes: usize, endpoints: usize, width: usize) -> Option<(usize, usize)> {
    let fit = |n: usize, min: usize, max: usize| {
        let w = (width.saturating_sub(n.saturating_sub(1) * GAP) / n.max(1)).min(max);
        (w >= min).then_some(w)
    };
    let node_w = fit(nodes, NODE_MIN, NODE_MAX)?;
    let nodes_w = nodes * node_w + nodes.saturating_sub(1) * GAP;
    // Endpoint boxes spread over the nodes' width, and get room for their address.
    let span = nodes_w.max(endpoints * EP_WANT + endpoints.saturating_sub(1) * GAP).min(width);
    let ep_w = (span.saturating_sub(endpoints.saturating_sub(1) * GAP) / endpoints.max(1)).min(EP_MAX);
    (ep_w >= EP_MIN).then_some((node_w, ep_w))
}

/// The diagram in at most `width` columns, with where each item was drawn; `None` when it
/// does not fit (then [`list`]).
pub fn diagram(t: &Topology, width: usize) -> Option<(Canvas, Vec<Hit>)> {
    let (n, e) = (t.instances.len(), t.endpoints.len());
    if n == 0 {
        return None;
    }
    let (node_w, ep_w) = widths(n, e, width)?;
    let nodes_w = n * node_w + (n - 1) * GAP;
    let eps_w = e * ep_w + e.saturating_sub(1) * GAP;
    let total = nodes_w.max(eps_w);
    let lanes_top = EP_H;
    let arrow_y = lanes_top + e;
    let node_y = arrow_y + 1;
    let volume_y = node_y + NODE_H + 1;
    let mut canvas = Canvas::new(total, volume_y + 1);
    let mut hits = Vec::new();

    // Nodes in zone order: the writer's zone first.
    let order: Vec<&Instance> = rds::by_zone(t.instances).into_iter().flat_map(|(_, list)| list).collect();
    let node_x0 = (total - nodes_w) / 2;
    let node_x = |id: &str| order.iter().position(|i| i.id == id).map(|p| node_x0 + p * (node_w + GAP));
    let item_of = |id: &str| t.endpoints.len() + t.instances.iter().position(|i| i.id == id).unwrap_or(0);

    // Endpoints and their lines.
    let ep_x0 = (total - eps_w) / 2;
    for (k, ep) in t.endpoints.iter().enumerate() {
        let color = endpoint_color(t.endpoints, k);
        let selected = t.selected == k;
        let (x, w) = (ep_x0 + k * (ep_w + GAP), ep_w);
        let border = if selected { Style::new().fg(theme::ACCENT).add_modifier(Modifier::BOLD) } else { Style::new().fg(color) };
        let mut boxed = Canvas::new(w, EP_H);
        boxed.frame(w, EP_H, border);
        let title = format!(" {} ", ep.name);
        let title_style = if selected { theme::selected() } else { Style::new().fg(color).add_modifier(Modifier::BOLD) };
        boxed.text(2, 0, &title, w - 4, title_style);
        let routes = t.routes(ep);
        let what = format!("{} · {} node{}", ep.kind_label(), routes.len(), if routes.len() == 1 { "" } else { "s" });
        boxed.text(2, 1, &what, w - 4, theme::muted());
        boxed.text(2, 2, &ep.address, w - 4, Style::new());
        canvas.blit(&boxed, x, 0);
        hits.push(Hit { item: k, x, y: 0, w, h: EP_H });

        let line = Style::new().fg(color);
        let cx = x + w / 2;
        let lane = lanes_top + k;
        canvas.vline(cx, EP_H - 1, lane, line);
        let anchors: Vec<usize> = routes.iter().filter_map(|id| node_x(id)).map(|nx| nx + 2 + (2 * k) % (node_w - 5)).collect();
        if anchors.is_empty() {
            continue;
        }
        let lo = anchors.iter().copied().chain([cx]).min().unwrap_or(cx);
        let hi = anchors.iter().copied().chain([cx]).max().unwrap_or(cx);
        if hi > lo {
            canvas.hline(lo, hi, lane, line);
        }
        for ax in anchors {
            canvas.vline(ax, lane, arrow_y, line);
            canvas.text(ax, arrow_y, "▼", 1, line);
        }
    }

    // Nodes.
    for i in &order {
        let Some(x) = node_x(&i.id) else { continue };
        let item = item_of(&i.id);
        let selected = t.selected == item;
        let health = t.health(i);
        let border = if selected { Style::new().fg(theme::ACCENT).add_modifier(Modifier::BOLD) } else { Style::new().fg(Color::Gray) };
        let mut boxed = Canvas::new(node_w, NODE_H);
        boxed.frame(node_w, NODE_H, border);
        let inner = node_w - 4;
        let role = if i.writer { " writer ".to_string() } else { format!(" tier {} ", i.promotion_tier.unwrap_or(1)) };
        let name_max = inner.saturating_sub(role.chars().count() + 1);
        let name_style = if selected { theme::selected() } else { Style::new().add_modifier(Modifier::BOLD) };
        boxed.text(2, 0, &format!(" {} ", ellipsize(&i.id, name_max.saturating_sub(2))), name_max, name_style);
        boxed.text(node_w - 1 - role.chars().count(), 0, &role, role.chars().count(), if i.writer { Style::new().fg(Color::LightMagenta) } else { theme::muted() });
        // ● available · r6g.xlarge
        boxed.text(2, 1, "●", 1, Style::new().fg(health_color(health)));
        let state = if i.status == "available" { short_class(i) } else { format!("{} · {}", i.status, short_class(i)) };
        boxed.text(4, 1, &state, inner - 2, if i.status == "available" { Style::new() } else { Style::new().fg(health_color(rds::status_health(&i.status))) });
        // CPU ▂▃▅▇ 72%
        let cpu = t.last(&i.id, "cpu");
        let cpu_text = cpu.map(|c| format!("{c:.0}%")).unwrap_or_else(|| "—".into());
        let spark_w = inner.saturating_sub(5 + cpu_text.len()).min(12);
        let spark = t.pulse.map(|p| sparkline_to(&columns(p.points(&i.id, "cpu"), p.window, spark_w, Agg::Mean), 100.0)).unwrap_or_default();
        let cpu_color = health_color(rds::node_health("available", cpu, None));
        boxed.text(2, 2, "CPU", 3, theme::muted());
        boxed.text(6, 2, &spark, spark_w, Style::new().fg(cpu_color));
        boxed.text(7 + spark_w, 2, &cpu_text, cpu_text.len(), Style::new().fg(cpu_color));
        // 184 connections · 4.0 ACU
        let mut use_line = t.last(&i.id, "connections").map(|c| format!("{c:.0} connections")).unwrap_or_else(|| "connections —".into());
        if let Some(acu) = t.last(&i.id, "acu") {
            use_line.push_str(&format!(" · {acu:.1} ACU"));
        }
        boxed.text(2, 3, &use_line, inner, Style::new());
        if i.writer {
            boxed.text(2, 4, "reads and writes", inner, theme::muted());
        } else {
            let lag = t.pulse.and_then(|p| p.max(&i.id, "lag"));
            let lag_color = health_color(rds::node_health("available", None, lag));
            boxed.text(2, 4, "lag", 3, theme::muted());
            boxed.text(6, 4, &lag.map(fmt_ms).unwrap_or_else(|| "—".into()), inner - 4, Style::new().fg(lag_color));
        }
        let zone = format!(" {} ", i.az.as_deref().unwrap_or("?"));
        boxed.text(2, NODE_H - 1, &zone, inner, theme::muted());
        canvas.blit(&boxed, x, node_y);
        hits.push(Hit { item, x, y: node_y, w: node_w, h: NODE_H });
        // Down to the shared volume.
        canvas.vline(x + node_w - 4, node_y + NODE_H - 1, volume_y, theme::muted());
    }

    let storage = if t.cluster.io_optimized() { "I/O-Optimized" } else { "Standard storage" };
    let volume = match t.volume {
        Some(v) => format!(" cluster volume · {} · {storage} ", rds::fmt_size(v)),
        None => format!(" cluster volume · {storage} "),
    };
    let label: String = ellipsize(&volume, total.saturating_sub(4));
    let left = (total - label.chars().count()) / 2;
    let bar = format!("{}{label}{}", "━".repeat(left), "━".repeat(total - left - label.chars().count()));
    canvas.text(0, volume_y, &bar, total, theme::muted());
    Some((canvas, hits))
}

/// The topology as lines, for terminals too narrow for the diagram, with the line each
/// item is on.
pub fn list(t: &Topology, width: usize) -> (Vec<Line<'static>>, Vec<usize>) {
    let mut lines = vec![widgets::section("Endpoints")];
    let mut rows = Vec::new();
    let name_w = t.endpoints.iter().map(|e| e.name.chars().count()).max().unwrap_or(6).min(24);
    for (k, e) in t.endpoints.iter().enumerate() {
        rows.push(lines.len());
        let style = if t.selected == k { theme::selected() } else { Style::new() };
        let to = t.routes(e).join(", ");
        let room = width.saturating_sub(name_w + 8 + to.chars().count());
        lines.push(Line::from(vec![
            Span::styled("◆ ", Style::new().fg(endpoint_color(t.endpoints, k))),
            Span::styled(format!("{:<name_w$}", ellipsize(&e.name, name_w)), style),
            Span::raw(" "),
            Span::styled(ellipsize(&e.address, room.max(8)), theme::muted()),
            Span::styled(" → ", theme::accent()),
            Span::raw(to),
        ]));
    }
    lines.push(Line::raw(""));
    lines.push(widgets::section("Nodes"));
    let id_w = t.instances.iter().map(|i| i.id.chars().count()).max().unwrap_or(8).min(28);
    for (n, i) in t.instances.iter().enumerate() {
        let item = t.endpoints.len() + n;
        rows.push(lines.len());
        let health = t.health(i);
        let style = if t.selected == item { theme::selected() } else { Style::new() };
        let cpu = t.last(&i.id, "cpu");
        let spark = t.pulse.map(|p| sparkline_to(&columns(p.points(&i.id, "cpu"), p.window, 8, Agg::Mean), 100.0)).unwrap_or_default();
        let mut stats = format!(" CPU {spark} {}", cpu.map(|c| format!("{c:.0}%")).unwrap_or_else(|| "—".into()));
        if let Some(c) = t.last(&i.id, "connections") {
            stats.push_str(&format!(" · {c:.0} conn"));
        }
        if !i.writer {
            stats.push_str(&format!(" · lag {}", t.pulse.and_then(|p| p.max(&i.id, "lag")).map(fmt_ms).unwrap_or_else(|| "—".into())));
        }
        lines.push(Line::from(vec![
            Span::styled(if i.writer { "✎ " } else { "● " }, Style::new().fg(health_color(health))),
            Span::styled(format!("{:<id_w$}", ellipsize(&i.id, id_w)), style),
            Span::styled(
                if width >= 90 {
                    format!(" {:<15} {:<13} {}", i.role(), short_class(i), i.az.as_deref().unwrap_or("?"))
                } else {
                    format!(" {:<15}", i.role())
                },
                theme::muted(),
            ),
            Span::raw(stats),
        ]));
    }
    (lines, rows)
}

/// The selected endpoint's or node's details, `width` columns wide.
pub fn details(t: &Topology, width: usize) -> Vec<Line<'static>> {
    let key_w = 10;
    let mut lines = Vec::new();
    if let Some(e) = t.endpoints.get(t.selected) {
        let color = endpoint_color(t.endpoints, t.selected);
        lines.push(Line::from(vec![
            Span::styled("◆ ", Style::new().fg(color)),
            Span::styled(e.name.clone(), Style::new().add_modifier(Modifier::BOLD)),
            Span::styled(format!("  {} endpoint", e.kind_label()), theme::muted()),
        ]));
        lines.push(Line::raw(""));
        lines.push(widgets::kv("Address", e.address.clone(), key_w));
        if let Some(p) = t.cluster.port {
            lines.push(widgets::kv("Port", p.to_string(), key_w));
        }
        lines.push(widgets::kv("Status", e.status.clone(), key_w));
        if !e.static_members.is_empty() {
            lines.push(widgets::kv("Static", e.static_members.join(", "), key_w));
        }
        if !e.excluded_members.is_empty() {
            lines.push(widgets::kv("Excluded", e.excluded_members.join(", "), key_w));
        }
        lines.push(Line::raw(""));
        lines.push(widgets::section("Sends connections to"));
        let routes = t.routes(e);
        if routes.is_empty() {
            lines.push(Line::styled("no available instance", Style::new().fg(theme::WARN)));
        }
        for id in routes {
            if let Some(i) = t.instances.iter().find(|i| i.id == id) {
                let cpu = t.last(&i.id, "cpu").map(|c| format!(" · CPU {c:.0}%")).unwrap_or_default();
                lines.push(Line::from(vec![
                    Span::styled("● ", Style::new().fg(health_color(t.health(i)))),
                    Span::raw(i.id.clone()),
                    Span::styled(format!("  {}{cpu}", i.role()), theme::muted()),
                ]));
            }
        }
        lines.push(Line::raw(""));
        lines.push(Line::styled("y copies the address · Y a connection command", theme::muted()));
        return lines;
    }
    let Some(i) = t.node(t.selected) else { return lines };
    let health = t.health(i);
    lines.push(Line::from(vec![
        Span::styled("● ", Style::new().fg(health_color(health))),
        Span::styled(i.id.clone(), Style::new().add_modifier(Modifier::BOLD)),
        Span::styled(format!("  {}", i.role()), theme::muted()),
    ]));
    lines.push(Line::raw(""));
    let class = match rds::class_specs(&i.class) {
        Some((vcpus, gib)) => format!("{} · {vcpus} vCPU · {gib:.0} GiB", i.class),
        None => i.class.clone(),
    };
    lines.push(widgets::kv("Class", class, key_w));
    if let Some(next) = &i.pending_class {
        lines.push(widgets::kv("Next class", Span::styled(format!("{next} (next maintenance window)"), Style::new().fg(theme::WARN)), key_w));
    }
    lines.push(widgets::kv("Zone", i.az.clone().unwrap_or_else(|| "?".into()), key_w));
    lines.push(widgets::kv("Status", Span::styled(i.status.clone(), Style::new().fg(health_color(rds::status_health(&i.status)))), key_w));
    if let Some(a) = &i.address {
        lines.push(widgets::kv("Endpoint", a.clone(), key_w));
    }
    lines.push(widgets::kv("Insights", if i.pi_enabled { "on".to_string() } else { "off".to_string() }, key_w));
    if let Some(group) = &i.parameter_group {
        let status = i.parameter_status.as_deref().unwrap_or("");
        let style = if status == "pending-reboot" { Style::new().fg(theme::WARN) } else { theme::muted() };
        lines.push(Line::from(vec![
            Span::styled(format!("{:<key_w$} ", "Parameters"), theme::muted()),
            Span::raw(group.clone()),
            Span::styled(if status.is_empty() { String::new() } else { format!(" · {status}") }, style),
        ]));
    }
    let Some(p) = t.pulse else { return lines };
    lines.push(Line::raw(""));
    lines.push(widgets::section("Last hour"));
    let chart_w = width.saturating_sub(1).max(20);
    let mut chart = |title: &str, id: &str, fmt: fn(f64) -> String, threshold: Option<f64>| {
        let points = p.points(&i.id, id);
        if points.is_empty() {
            return;
        }
        let mut c = Chart::new(title, points, p.window);
        c.agg = if id == "lag" { Agg::Max } else { Agg::Mean };
        c.rows = 2;
        c.fmt = fmt;
        c.threshold = threshold;
        c.axis = false;
        let latest = p.last(&i.id, id).map(fmt).unwrap_or_default();
        c.summary = vec![Span::styled(latest, theme::muted())];
        lines.extend(c.lines(chart_w));
    };
    chart("CPU", "cpu", |v| format!("{v:.0}%"), Some(rds::CPU_WARN));
    chart("Connections", "connections", |v| format!("{v:.0}"), None);
    if !i.writer {
        chart("Replica lag", "lag", fmt_ms, Some(rds::LAG_WARN));
    }
    if i.serverless() {
        chart("Capacity (ACU)", "acu", |v| format!("{v:.1}"), None);
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn instance(id: &str, writer: bool, az: &str) -> Instance {
        Instance {
            id: id.into(),
            cluster: "c".into(),
            class: "db.r6g.large".into(),
            status: "available".into(),
            az: Some(az.into()),
            writer,
            promotion_tier: Some(1),
            ..Default::default()
        }
    }

    fn endpoint(name: &str, kind: EndpointKind) -> Endpoint {
        Endpoint {
            name: name.into(),
            cluster: "c".into(),
            kind,
            address: format!("{name}.cluster-x.us-east-1.rds.amazonaws.com"),
            status: "available".into(),
            static_members: Vec::new(),
            excluded_members: Vec::new(),
        }
    }

    fn text(canvas: &Canvas) -> Vec<String> {
        canvas.lines(0, 0, canvas.w, canvas.h).iter().map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect::<String>()).collect()
    }

    #[test]
    fn endpoints_reach_their_nodes() {
        let cluster = Cluster { id: "c".into(), storage_type: Some("aurora".into()), ..Default::default() };
        let (w, r) = (instance("w", true, "us-east-1a"), instance("r", false, "us-east-1b"));
        let instances = [&w, &r];
        let (we, re) = (endpoint("writer", EndpointKind::Writer), endpoint("reader", EndpointKind::Reader));
        let endpoints = [&we, &re];
        let t = Topology { cluster: &cluster, instances: &instances, endpoints: &endpoints, pulse: None, selected: 2, volume: Some(1e9) };
        let (canvas, hits) = diagram(&t, 60).expect("fits in 60 columns");
        let rows = text(&canvas);
        // Writer endpoint (lane 0) drops onto the writer, reader (lane 1) onto the reader.
        assert_eq!(rows.len(), EP_H + 2 + 1 + NODE_H + 2);
        assert_eq!(hits.iter().map(|h| (h.item, h.y)).collect::<Vec<_>>(), [(0, 0), (1, 0), (2, 7), (3, 7)]);
        let arrows = &rows[EP_H + 2];
        assert_eq!(arrows.matches('▼').count(), 2, "{rows:#?}");
        let node_row = &rows[EP_H + 3];
        assert!(node_row.contains(" w ") && node_row.contains(" writer ") && node_row.contains(" r ") && node_row.contains(" tier 1 "), "{rows:#?}");
        assert!(rows.last().unwrap().contains("cluster volume · 954 MB"), "{rows:#?}");
        assert!(rows.iter().any(|r| r.contains(" us-east-1a ")) && rows.iter().any(|r| r.contains(" us-east-1b ")));
        // Each arrow sits under a line coming down from a lane.
        let x = arrows.chars().position(|c| c == '▼').unwrap();
        assert_eq!(rows[EP_H + 1].chars().nth(x), Some('│'), "{rows:#?}");
    }

    #[test]
    fn too_narrow_falls_back_to_a_list() {
        let cluster = Cluster::default();
        let nodes: Vec<Instance> = (0..5).map(|n| instance(&format!("n{n}"), n == 0, "us-east-1a")).collect();
        let instances: Vec<&Instance> = nodes.iter().collect();
        let e = endpoint("writer", EndpointKind::Writer);
        let endpoints = [&e];
        let t = Topology { cluster: &cluster, instances: &instances, endpoints: &endpoints, pulse: None, selected: 0, volume: None };
        assert!(diagram(&t, 100).is_none());
        assert!(diagram(&t, 130).is_some());
        let (lines, rows) = list(&t, 100);
        assert_eq!(rows.len(), 6);
        let text: String = lines[rows[1]].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.starts_with("✎ n0") && text.contains("writer"), "{text}");
    }
}
