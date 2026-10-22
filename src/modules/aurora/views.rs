//! The tabs of an Aurora cluster other than the topology, as lines: a node's metrics, its
//! database load (Performance Insights), the cluster's events and pending maintenance, and
//! its configuration.

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use super::topology::health_color;
use crate::aws::lambda::Range;
use crate::aws::pi::{self, Top};
use crate::aws::rds::{self, Cluster, Event, Health, Instance, Maintenance, MetricDef, Metrics, Unit};
use crate::aws::{self, AwsError};
use crate::tui::chart::{Agg, Chart, fmt_window};
use crate::tui::{theme, widgets};
use crate::util::{ellipsize, fmt_ago, fmt_utc};

/// What Performance Insights says about one instance over a window.
pub struct Insights {
    pub window: (f64, f64),
    pub load: aws::Result<Vec<(f64, f64)>>,
    pub sql: aws::Result<Vec<Top>>,
    pub waits: aws::Result<Vec<Top>>,
}

fn loading(tick: usize, what: &str) -> Line<'static> {
    Line::from(vec![Span::styled(theme::spinner(tick), theme::accent()), Span::raw(format!(" {what}"))])
}

fn error_line(e: &AwsError) -> Line<'static> {
    Line::styled(e.short(), Style::new().fg(theme::ERR))
}

/// The formatter of a unit for a chart's scale, which has six columns: `20.7G`, `4.0`.
fn unit_fmt(unit: Unit) -> fn(f64) -> String {
    match unit {
        Unit::Percent => |v| Unit::Percent.fmt(v),
        Unit::Count => |v| Unit::Count.fmt(v),
        Unit::PerSecond => |v| Unit::PerSecond.fmt(v),
        Unit::Bytes => |v| {
            // `20.7 GB` → `20.7G`
            let size = rds::fmt_size(v);
            let (n, unit) = size.split_once(' ').unwrap_or((&size, ""));
            format!("{n}{}", unit.trim_end_matches('B'))
        },
        Unit::Seconds => |v| Unit::Seconds.fmt(v),
        Unit::Millis => |v| Unit::Millis.fmt(v),
        Unit::Acu => |v| format!("{v:.1}"),
        Unit::Sessions => |v| Unit::Sessions.fmt(v),
    }
}

fn chart_lines(m: &Metrics, source: &str, def: &MetricDef, threshold: Option<f64>, width: usize) -> Vec<Line<'static>> {
    let points = m.points(source, def.id);
    let mut c = Chart::new(def.label, points, m.window);
    c.agg = match def.stat {
        "Sum" => Agg::Sum,
        "Maximum" => Agg::Max,
        _ => Agg::Mean,
    };
    c.fmt = unit_fmt(def.unit);
    c.threshold = threshold;
    c.rows = 3;
    let summary = match def.stat {
        "Sum" => format!("{} in the window", def.unit.fmt(m.sum(source, def.id))),
        "Minimum" => format!("lowest {} · latest {}", def.unit.fmt(m.min(source, def.id).unwrap_or_default()), def.unit.fmt(m.last(source, def.id).unwrap_or_default())),
        _ => format!("avg {} · max {}", def.unit.fmt(m.avg(source, def.id).unwrap_or_default()), def.unit.fmt(m.max(source, def.id).unwrap_or_default())),
    };
    c.summary = vec![Span::styled(summary, theme::muted())];
    c.lines(width)
}

/// Charts two by two when there is room for both.
fn grid(charts: Vec<Vec<Line<'static>>>, width: usize) -> Vec<Line<'static>> {
    const COL_MIN: usize = 52;
    let mut out = Vec::new();
    if width < COL_MIN * 2 + 3 {
        for c in charts {
            out.extend(c);
            out.push(Line::raw(""));
        }
        return out;
    }
    let col = (width - 3) / 2;
    let mut it = charts.into_iter();
    while let Some(left) = it.next() {
        let right = it.next().unwrap_or_default();
        for r in 0..left.len().max(right.len()) {
            let mut spans = left.get(r).cloned().map(|l| l.spans).unwrap_or_default();
            let used: usize = spans.iter().map(|s| s.content.chars().count()).sum();
            spans.push(Span::raw(" ".repeat(col.saturating_sub(used) + 3)));
            if let Some(l) = right.get(r) {
                spans.extend(l.spans.clone());
            }
            out.push(Line::from(spans));
        }
        out.push(Line::raw(""));
    }
    out
}

/// A node's metrics over the range, then the cluster volume's.
pub fn performance(cluster: &Cluster, node: &Instance, metrics: Option<&aws::Result<Metrics>>, width: usize, tick: usize) -> Vec<Line<'static>> {
    let m = match metrics {
        None => return vec![loading(tick, "reading metrics…")],
        Some(Err(e)) => return vec![error_line(e)],
        Some(Ok(m)) => m,
    };
    let chart_w = if width >= 107 { (width - 3) / 2 } else { width };
    let vcpus = rds::class_specs(&node.class).map(|(v, _)| v as f64);
    let charts: Vec<Vec<Line<'static>>> = rds::INSTANCE_METRICS
        .iter()
        .filter(|d| m.has(&node.id, d.id))
        // Readers do not write: their write figures are zero, not worth a chart.
        .filter(|d| node.writer || !matches!(d.id, "write_latency" | "commit_latency" | "deadlocks") || m.max(&node.id, d.id).is_some_and(|v| v > 0.0))
        .map(|d| {
            let threshold = match d.id {
                "cpu" => Some(rds::CPU_WARN),
                "lag" => Some(rds::LAG_WARN),
                "load" => vcpus,
                _ => None,
            };
            chart_lines(m, &node.id, d, threshold, chart_w)
        })
        .collect();
    let mut lines = Vec::new();
    if charts.is_empty() {
        lines.push(Line::styled(format!("No metrics for {} in the window.", node.id), theme::muted()));
    }
    if let Some(v) = vcpus.filter(|_| m.has(&node.id, "load")) {
        lines.push(Line::styled(format!("DB load is the average number of active sessions: above {v:.0} (the vCPUs), sessions wait."), theme::muted()));
        lines.push(Line::raw(""));
    }
    lines.extend(grid(charts, width));
    let volume: Vec<Vec<Line<'static>>> =
        rds::CLUSTER_METRICS.iter().filter(|d| m.has(&cluster.id, d.id)).map(|d| chart_lines(m, &cluster.id, d, None, chart_w)).collect();
    if !volume.is_empty() {
        lines.push(widgets::section(&format!("Cluster volume · {}", cluster.storage_label())));
        lines.extend(grid(volume, width));
    }
    lines
}

/// `██████░░░░` for a share of the largest.
fn bar(value: f64, max: f64, width: usize) -> String {
    let filled = if max > 0.0 { ((value / max) * width as f64).round() as usize } else { 0 };
    format!("{}{}", "█".repeat(filled.min(width)), "░".repeat(width - filled.min(width)))
}

/// The SQL on one line, for lists.
pub fn one_line(sql: &str) -> String {
    sql.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// A node's database load: the load over time against its vCPUs, the wait events and the
/// statements that made it. Returns the lines and the line of each statement.
pub fn insights(
    node: &Instance,
    region: &str,
    data: Option<&Insights>,
    cursor: Option<usize>,
    width: usize,
    tick: usize,
) -> (Vec<Line<'static>>, Vec<usize>) {
    if !node.pi_enabled {
        let lines = vec![
            Line::styled(format!("Performance Insights is off for {}.", node.id), Style::new().fg(theme::WARN)),
            Line::raw(""),
            Line::styled("It shows which SQL and which waits make up the load. dcli does not turn it on; you can with:", theme::muted()),
            Line::raw(pi::how_to_enable(&node.id, region)),
        ];
        return (lines, Vec::new());
    }
    let Some(d) = data else { return (vec![loading(tick, "reading Performance Insights…")], Vec::new()) };
    let mut lines = Vec::new();
    let mut rows = Vec::new();
    let vcpus = rds::class_specs(&node.class).map(|(v, _)| v as f64);
    match &d.load {
        Ok(points) if !points.is_empty() => {
            let mut c = Chart::new("DB load (average active sessions)", points, d.window);
            c.agg = Agg::Mean;
            c.threshold = vcpus;
            c.fmt = |v| format!("{v:.1}");
            c.rows = 4;
            let avg = points.iter().map(|p| p.1).sum::<f64>() / points.len() as f64;
            let max = points.iter().map(|p| p.1).fold(0.0, f64::max);
            let cpus = vcpus.map(|v| format!(" · {v:.0} vCPUs")).unwrap_or_default();
            c.summary = vec![Span::styled(format!("avg {avg:.2} · max {max:.2}{cpus}"), theme::muted())];
            lines.extend(c.lines(width));
        }
        Ok(_) => lines.push(Line::styled("No database load in the window.", theme::muted())),
        Err(e) => lines.push(error_line(e)),
    }
    lines.push(Line::raw(""));
    lines.push(widgets::section("Wait events"));
    match &d.waits {
        Ok(waits) if !waits.is_empty() => {
            let max = waits.iter().map(|w| w.load).fold(0.0, f64::max);
            let name_w = waits.iter().map(|w| w.label.chars().count()).max().unwrap_or(10).min(32);
            for w in waits {
                lines.push(Line::from(vec![
                    Span::styled(format!("{:>6.2} ", w.load), Style::new().add_modifier(Modifier::BOLD)),
                    Span::styled(bar(w.load, max, 16), theme::accent()),
                    Span::raw(format!("  {:<name_w$}", ellipsize(&w.label, name_w))),
                    Span::styled(format!("  {}", w.kind.as_deref().unwrap_or("")), theme::muted()),
                ]));
            }
        }
        Ok(_) => lines.push(Line::styled("None in the window.", theme::muted())),
        Err(e) => lines.push(error_line(e)),
    }
    lines.push(Line::raw(""));
    lines.push(Line::from(vec![
        widgets::section("Top SQL").spans.remove(0),
        Span::styled("   average active sessions · Enter selects, Enter again shows the whole statement", theme::muted()),
    ]));
    match &d.sql {
        Ok(sql) if !sql.is_empty() => {
            let max = sql.iter().map(|s| s.load).fold(0.0, f64::max);
            let splits: Vec<String> = sql
                .iter()
                .map(|s| {
                    let parts: Vec<String> =
                        s.partitions.iter().filter(|(_, v)| *v > 0.0).map(|(k, v)| format!("{k} {:.0}%", v / s.load.max(1e-9) * 100.0)).collect();
                    parts.join(" · ")
                })
                .collect();
            let split_w = splits.iter().map(|s| s.chars().count()).max().unwrap_or(0);
            for (n, (s, split)) in sql.iter().zip(splits).enumerate() {
                rows.push(lines.len());
                let split = format!("{split:<split_w$}");
                let room = width.saturating_sub(6 + 1 + 10 + 2 + split_w + 2);
                let selected = cursor == Some(n);
                let text_style = if selected { theme::selected() } else { Style::new() };
                lines.push(Line::from(vec![
                    Span::styled(format!("{:>6.2} ", s.load), Style::new().add_modifier(Modifier::BOLD)),
                    Span::styled(bar(s.load, max, 10), theme::accent()),
                    Span::styled(format!("  {split}  "), theme::muted()),
                    Span::styled(ellipsize(&one_line(&s.label), room.max(10)), text_style),
                ]));
            }
        }
        Ok(_) => lines.push(Line::styled("No statements in the window.", theme::muted())),
        Err(e) => lines.push(error_line(e)),
    }
    (lines, rows)
}

fn severity_color(h: Health) -> ratatui::style::Color {
    match h {
        Health::Ok => theme::MUTED,
        other => health_color(other),
    }
}

/// The cluster's pending maintenance, then its events of the last 7 days on a timeline (a
/// row per source) and as a list.
pub fn events(
    cluster: &Cluster,
    instances: &[&Instance],
    events: Option<&aws::Result<Vec<Event>>>,
    pending: Option<&aws::Result<Vec<Maintenance>>>,
    width: usize,
    now: f64,
    tick: usize,
) -> Vec<Line<'static>> {
    let sources: Vec<&str> = [cluster.id.as_str()].into_iter().chain(instances.iter().map(|i| i.id.as_str())).collect();
    let mine = |s: &str| sources.contains(&s);
    let mut lines = Vec::new();
    if let Some(Ok(pending)) = pending {
        let pending: Vec<&Maintenance> = pending.iter().filter(|m| mine(m.target())).collect();
        if !pending.is_empty() {
            lines.push(widgets::section("Pending maintenance"));
            for m in pending {
                let when = match (m.auto_applied_after, m.forced_apply, m.current_apply) {
                    (_, _, Some(t)) => format!("applies {}", fmt_utc(t)),
                    (Some(t), _, _) => format!("in the maintenance window after {}", fmt_utc(t)),
                    (_, Some(t), _) => format!("forced on {}", fmt_utc(t)),
                    _ => "when you apply it".into(),
                };
                lines.push(Line::from(vec![
                    Span::styled("◆ ", Style::new().fg(theme::WARN)),
                    Span::styled(m.target().to_string(), Style::new().add_modifier(Modifier::BOLD)),
                    Span::styled(format!("  {}", m.action), Style::new().fg(theme::WARN)),
                    Span::styled(format!("  {when}"), theme::muted()),
                ]));
                if let Some(d) = &m.description {
                    lines.push(Line::styled(format!("  {d}"), theme::muted()));
                }
            }
            lines.push(Line::raw(""));
        }
    }
    let events = match events {
        None => {
            lines.push(loading(tick, "reading events…"));
            return lines;
        }
        Some(Err(e)) => {
            lines.push(error_line(e));
            return lines;
        }
        Some(Ok(events)) => events.iter().filter(|e| mine(&e.source)).collect::<Vec<_>>(),
    };
    const WEEK: f64 = 7.0 * 86_400.0;
    let label_w = sources.iter().map(|s| s.chars().count()).max().unwrap_or(8).min(24);
    let track = width.saturating_sub(label_w + 2).max(10);
    lines.push(widgets::section("Last 7 days"));
    for source in &sources {
        let mut cells: Vec<(char, Style)> = vec![('·', theme::muted()); track];
        for e in events.iter().filter(|e| e.source == *source) {
            let x = (((e.date - (now - WEEK)) / WEEK) * track as f64) as isize;
            if let Some(cell) = usize::try_from(x).ok().and_then(|x| cells.get_mut(x.min(track - 1))) {
                // The most severe event wins a shared cell.
                let style = Style::new().fg(severity_color(e.severity()));
                if cell.0 != '◆' || severity_color(e.severity()) != theme::MUTED {
                    *cell = ('◆', style);
                }
            }
        }
        let mut spans = vec![Span::raw(format!("{:<label_w$}  ", ellipsize(source, label_w)))];
        spans.extend(cells.into_iter().map(|(c, s)| Span::styled(c.to_string(), s)));
        lines.push(Line::from(spans));
    }
    let mut axis = vec![' '; track];
    for (at, label) in [(0, format!("-{}", fmt_window(WEEK))), (track * 4 / 7, "-3d".into()), (track.saturating_sub(3), "now".into())] {
        for (k, c) in label.chars().enumerate() {
            if let Some(cell) = axis.get_mut(at + k) {
                *cell = c;
            }
        }
    }
    lines.push(Line::styled(format!("{}  {}", " ".repeat(label_w), axis.into_iter().collect::<String>()), theme::muted()));
    lines.push(Line::raw(""));
    if events.is_empty() {
        lines.push(Line::styled("No events in the last 7 days.", theme::muted()));
    }
    let category_w = events.iter().map(|e| e.categories.join(", ").chars().count()).max().unwrap_or(0).min(24);
    for e in events {
        lines.push(Line::from(vec![
            Span::styled("◆ ", Style::new().fg(severity_color(e.severity()))),
            Span::styled(format!("{:<9}", fmt_ago(e.date)), theme::muted()),
            Span::raw(format!("{:<w$}  ", e.source, w = label_w)),
            Span::styled(format!("{:<category_w$}  ", ellipsize(&e.categories.join(", "), category_w)), theme::muted()),
            Span::raw(e.message.clone()),
        ]));
    }
    lines
}

/// The cluster's configuration (never a password).
pub fn config(cluster: &Cluster, instances: &[&Instance]) -> Vec<Line<'static>> {
    let c = cluster;
    let k = 22;
    let on = |b: bool| if b { "on".to_string() } else { "off".to_string() };
    let mut lines = vec![
        widgets::kv("Engine", c.engine_label(), k),
        widgets::kv("Capacity", c.capacity_label(), k),
        widgets::kv("Storage", format!("{} · {}", c.storage_type.as_deref().unwrap_or("aurora"), c.storage_label()), k),
        widgets::kv("Status", c.status.clone(), k),
    ];
    if let Some((g, primary)) = &c.global {
        lines.push(widgets::kv("Global database", format!("{g} · {}", if *primary { "primary cluster" } else { "secondary cluster" }), k));
    }
    lines.push(widgets::kv("Port", c.port.map(|p| p.to_string()).unwrap_or_default(), k));
    lines.push(widgets::kv("Master user", c.master_username.clone().unwrap_or_default(), k));
    if let Some(db) = &c.database_name {
        lines.push(widgets::kv("Database", db.clone(), k));
    }
    lines.push(widgets::kv("IAM authentication", on(c.iam_auth), k));
    lines.push(widgets::kv("Encrypted", on(c.storage_encrypted), k));
    let protection = if c.deletion_protection { Span::raw("on") } else { Span::styled("off", Style::new().fg(theme::WARN)) };
    lines.push(widgets::kv("Deletion protection", protection, k));
    lines.push(widgets::kv(
        "Backups",
        format!("{} days · window {}", c.backup_retention.map(|d| d.to_string()).unwrap_or_else(|| "?".into()), c.backup_window.as_deref().unwrap_or("?")),
        k,
    ));
    if let Some(w) = c.backtrack_window {
        lines.push(widgets::kv("Backtrack", format!("{} h", w / 3600), k));
    }
    lines.push(widgets::kv("Maintenance window", c.maintenance_window.clone().unwrap_or_default(), k));
    lines.push(widgets::kv("Parameter group", c.parameter_group.clone().unwrap_or_default(), k));
    if let Some(t) = c.created {
        lines.push(widgets::kv("Created", format!("{} ({})", fmt_utc(t), fmt_ago(t)), k));
    }
    lines.push(Line::raw(""));
    lines.push(widgets::section("Instances"));
    for i in instances {
        let pending = c.members.iter().find(|m| m.instance == i.id).and_then(|m| m.parameter_status.clone()).filter(|s| s != "in-sync");
        let mut notes = Vec::new();
        if let Some(p) = pending {
            notes.push(format!("cluster parameters {p}"));
        }
        if i.parameter_status.as_deref().is_some_and(|s| s != "in-sync") {
            notes.push(format!("instance parameters {}", i.parameter_status.clone().unwrap_or_default()));
        }
        if let Some(next) = &i.pending_class {
            notes.push(format!("class change to {next} pending"));
        }
        let style = if notes.is_empty() { theme::muted() } else { Style::new().fg(theme::WARN) };
        lines.push(Line::from(vec![
            Span::styled(format!("{:<20} ", i.id), Style::new().add_modifier(Modifier::BOLD)),
            Span::styled(format!("{} · {} · {} · Insights {}", i.role(), i.class, i.az.as_deref().unwrap_or("?"), on(i.pi_enabled)), theme::muted()),
            Span::styled(if notes.is_empty() { String::new() } else { format!(" · {}", notes.join(" · ")) }, style),
        ]));
    }
    lines
}

/// `◂ orders-1 · writer ▸   1h 24h 7d`: the node the tab is about and the range.
pub fn node_bar(node: &Instance, index: usize, count: usize, range: Option<Range>) -> Line<'static> {
    let mut spans = vec![
        Span::styled("◂ ", theme::muted()),
        Span::styled(node.id.clone(), Style::new().add_modifier(Modifier::BOLD)),
        Span::styled(format!(" · {} · {}/{count}", node.role(), index + 1), theme::muted()),
        Span::styled(" ▸", theme::muted()),
        Span::styled("  ↑↓ node", theme::muted()),
    ];
    if let Some(range) = range {
        spans.push(Span::raw("     "));
        for r in [Range::Hour, Range::Day, Range::Week] {
            let style = if r == range { theme::accent().add_modifier(Modifier::BOLD) } else { theme::muted() };
            spans.push(Span::styled(format!("{} ", r.label()), style));
        }
        spans.push(Span::styled(" m changes", theme::muted()));
    }
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chart_scales_fit_six_columns() {
        let gb = 1024.0 * 1024.0 * 1024.0;
        assert_eq!(unit_fmt(Unit::Bytes)(20.7 * gb), "20.7G");
        assert_eq!(unit_fmt(Unit::Bytes)(512.0 * 1024.0 * 1024.0), "512M");
        assert_eq!(unit_fmt(Unit::Bytes)(12.0), "12");
        assert_eq!(unit_fmt(Unit::Acu)(4.0), "4.0");
    }

    #[test]
    fn statements_fit_on_one_line() {
        assert_eq!(one_line("SELECT a\n  FROM t\n\tWHERE x = ?"), "SELECT a FROM t WHERE x = ?");
        assert_eq!(bar(1.0, 2.0, 4), "██░░");
        assert_eq!(bar(0.0, 0.0, 3), "░░░");
    }

    #[test]
    fn events_land_on_the_timeline() {
        let cluster = Cluster { id: "c".into(), ..Default::default() };
        let i = Instance { id: "i".into(), ..Default::default() };
        let now = 1_000_000.0;
        let events = Ok(vec![
            Event { source: "c".into(), source_type: "db-cluster".into(), message: "failover".into(), categories: vec!["failover".into()], date: now - 3600.0 },
            Event { source: "other".into(), source_type: "db-cluster".into(), message: "x".into(), categories: Vec::new(), date: now - 3600.0 },
        ]);
        let lines = super::events(&cluster, &[&i], Some(&events), None, 40, now, 0);
        let text: Vec<String> = lines.iter().map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect()).collect();
        assert!(text[1].starts_with("c  ") && text[1].ends_with('◆'), "{text:#?}");
        assert!(!text[2].contains('◆'), "{text:#?}");
        assert_eq!(text.iter().filter(|l| l.contains("failover")).count(), 1, "events of other clusters are left out: {text:#?}");
    }
}
