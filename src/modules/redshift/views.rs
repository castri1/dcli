//! The tabs of a Redshift cluster other than its nodes, as lines: the metrics, the queries
//! running, queued and slowest, the tables and their upkeep, the events, and the
//! configuration with its snapshots.

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use super::nodes::health_color;
use crate::aws::lambda::Range;
use crate::aws::rds::Metrics;
use crate::aws::redshift::{self, Activity, Cluster, Event, Health, Lock, MetricDef, Node, Query, Snapshot, TableInfo, Unit, Wlm};
use crate::aws::redshift_data::Identity;
use crate::aws::{self, AwsError};
use crate::tui::chart::{Agg, Chart, fmt_count, fmt_ms, fmt_window};
use crate::tui::{theme, widgets};
use crate::util::{ellipsize, fmt_ago, fmt_utc};

fn loading(tick: usize, what: &str) -> Line<'static> {
    Line::from(vec![Span::styled(theme::spinner(tick), theme::accent()), Span::raw(format!(" {what}"))])
}

fn error_line(e: &AwsError) -> Line<'static> {
    Line::styled(e.short(), Style::new().fg(theme::ERR))
}

/// The formatter of a unit for a chart's scale, which has six columns.
fn unit_fmt(unit: Unit) -> fn(f64) -> String {
    match unit {
        Unit::Percent => |v| Unit::Percent.fmt(v),
        Unit::Count => |v| Unit::Count.fmt(v),
        Unit::PerSecond => |v| if v < 10.0 { format!("{v:.2}") } else { fmt_count(v) },
        Unit::Micros => |v| Unit::Micros.fmt(v),
        Unit::Millis => |v| Unit::Millis.fmt(v),
        Unit::Seconds => |v| Unit::Seconds.fmt(v),
        Unit::Flag => |v| format!("{v:.0}"),
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

/// A node's metrics over the range, then the cluster's: connections, the WLM queue, the
/// queries completed and their duration by latency, concurrency scaling.
pub fn performance(cluster: &Cluster, node: &Node, metrics: Option<&aws::Result<Metrics>>, width: usize, tick: usize) -> Vec<Line<'static>> {
    if cluster.paused() {
        return vec![Line::styled("The cluster is paused: it publishes no metrics until it resumes (p).", theme::muted())];
    }
    let m = match metrics {
        None => return vec![loading(tick, "reading metrics…")],
        Some(Err(e)) => return vec![error_line(e)],
        Some(Ok(m)) => m,
    };
    let chart_w = if width >= 107 { (width - 3) / 2 } else { width };
    let key = redshift::node_key(&cluster.id, node);
    let charts: Vec<Vec<Line<'static>>> = redshift::NODE_METRICS
        .iter()
        .filter(|d| m.has(&key, d.id))
        .map(|d| {
            let threshold = match d.id {
                "cpu" => Some(redshift::CPU_WARN),
                "disk" => Some(redshift::DISK_WARN),
                _ => None,
            };
            chart_lines(m, &key, d, threshold, chart_w)
        })
        .collect();
    let mut lines = Vec::new();
    if charts.is_empty() {
        lines.push(Line::styled(format!("No metrics for {} in the window.", node.label()), theme::muted()));
        lines.push(Line::raw(""));
    }
    lines.extend(grid(charts, width));
    let id = cluster.id.as_str();
    lines.push(widgets::section("Cluster"));
    if m.min(id, "health").is_some_and(|h| h < 0.5) {
        lines.push(Line::styled("Redshift reported the cluster unhealthy at times in the window.", Style::new().fg(theme::ERR)));
    }
    if m.max(id, "maintenance").is_some_and(|v| v >= 0.5) {
        lines.push(Line::styled("The cluster was in maintenance mode at times in the window.", Style::new().fg(theme::WARN)));
    }
    let cluster_charts: Vec<Vec<Line<'static>>> = redshift::CLUSTER_METRICS
        .iter()
        .filter(|d| !matches!(d.id, "health" | "maintenance" | "cpu_all" | "disk_all"))
        // Concurrency scaling only when it happened.
        .filter(|d| !d.id.starts_with("cs_") || m.max(id, d.id).is_some_and(|v| v > 0.0))
        .filter(|d| m.has(id, d.id))
        .map(|d| chart_lines(m, id, d, None, chart_w))
        .collect();
    if cluster_charts.is_empty() {
        lines.push(Line::styled("No cluster metrics in the window.", theme::muted()));
    }
    lines.extend(grid(cluster_charts, width));
    lines
}

/// `◂ compute-0 · 2/5 ▸   1h 24h 7d`: the node the Performance tab is about, and the range.
pub fn node_bar(node: &Node, index: usize, count: usize, range: Range) -> Line<'static> {
    let mut spans = vec![
        Span::styled("◂ ", theme::muted()),
        Span::styled(node.label(), Style::new().add_modifier(Modifier::BOLD)),
        Span::styled(format!(" · {}/{count}", index + 1), theme::muted()),
        Span::styled(" ▸", theme::muted()),
        Span::styled("  ↑↓ node", theme::muted()),
        Span::raw("     "),
    ];
    for r in [Range::Hour, Range::Day, Range::Week] {
        let style = if r == range { theme::accent().add_modifier(Modifier::BOLD) } else { theme::muted() };
        spans.push(Span::styled(format!("{} ", r.label()), style));
    }
    spans.push(Span::styled(" m changes", theme::muted()));
    Line::from(spans)
}

/// The SQL on one line, for lists.
pub fn one_line(sql: &str) -> String {
    sql.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn micros(us: f64) -> String {
    fmt_ms(us / 1000.0)
}

/// The queries of the Queries tab, in the order the cursor walks them: running and queued
/// now, the slowest of the last day, the longest waits.
pub fn listed(a: &Activity) -> Vec<&Query> {
    sections(a).into_iter().flatten().collect()
}

/// The running, slowest and longest-waiting queries, each list empty when it failed.
fn sections(a: &Activity) -> [&[Query]; 3] {
    [&a.running, &a.slowest, &a.waited].map(|r| r.as_ref().map_or(&[][..], Vec::as_slice))
}

/// A query of [`listed`] as (list, session, query id, start second): what a refresh looks
/// for to keep the cursor on the same query.
pub type QueryKey = (usize, Option<i64>, Option<i64>, Option<i64>);

/// Row `n` of [`listed`] as a [`QueryKey`].
pub fn query_key(a: &Activity, n: usize) -> Option<QueryKey> {
    let mut n = n;
    for (list, rows) in sections(a).into_iter().enumerate() {
        if let Some(q) = rows.get(n) {
            return Some((list, q.pid, q.query_id, q.started.map(|s| s as i64)));
        }
        n -= rows.len();
    }
    None
}

/// Where the query `key` (from [`query_key`]) is in [`listed`] now.
pub fn find_query(a: &Activity, key: &QueryKey) -> Option<usize> {
    let mut offset = 0;
    for (list, rows) in sections(a).into_iter().enumerate() {
        if list == key.0 {
            return rows.iter().position(|q| (q.pid, q.query_id, q.started.map(|s| s as i64)) == (key.1, key.2, key.3)).map(|p| offset + p);
        }
        offset += rows.len();
    }
    None
}

/// What a tab that runs SQL shows without a database identity.
pub fn no_identity(cluster: &Cluster) -> Vec<Line<'static>> {
    vec![
        Line::styled("This tab reads Redshift's system tables with SQL, through the Data API.", theme::muted()),
        Line::styled("It runs as a database identity: a database user with temporary credentials (your IAM", theme::muted()),
        Line::styled("permissions), or a Secrets Manager secret. dcli never sees a password.", theme::muted()),
        Line::raw(""),
        Line::from(vec![
            Span::styled("i", theme::key()),
            Span::raw(format!(" sets the identity for {} (database {} by default).", cluster.id, cluster.database())),
        ]),
    ]
}

/// The Queries tab. Returns the lines and the line of each query of [`listed`].
pub fn queries(
    cluster: &Cluster,
    identity: Option<&Identity>,
    data: Option<&Activity>,
    cursor: Option<usize>,
    width: usize,
    now: f64,
    tick: usize,
) -> (Vec<Line<'static>>, Vec<usize>) {
    if cluster.paused() {
        return (vec![Line::styled("The cluster is paused: no queries run.", theme::muted())], Vec::new());
    }
    let Some(identity) = identity else { return (no_identity(cluster), Vec::new()) };
    let mut lines = vec![Line::from(vec![
        Span::styled(format!("as {}", identity.describe()), theme::muted()),
        Span::styled(" · users who are not superusers see only their own queries · i changes it", theme::muted()),
    ])];
    let Some(a) = data else {
        lines.push(loading(tick, "running system-table queries…"));
        return (lines, Vec::new());
    };
    lines.push(Line::styled(format!("read {} · r reads again", fmt_ago(a.at)), theme::muted()));
    lines.push(Line::raw(""));
    let mut rows = Vec::new();
    let mut n = 0usize;
    // The columns before the SQL take 54.
    let text_w = width.saturating_sub(54).max(16);
    let mut section = |lines: &mut Vec<Line<'static>>, title: &str, list: &aws::Result<Vec<Query>>, live: bool| {
        lines.push(widgets::section(title));
        match list {
            Ok(list) if list.is_empty() => lines.push(Line::styled("  none", theme::muted())),
            Ok(list) => {
                lines.push(Line::styled(
                    format!("  {:<7} {:<12} {:<9} {:>9} {:>9}  SQL", "pid", "user", "status", if live { "for" } else { "took" }, "queued"),
                    theme::muted(),
                ));
                for q in list {
                    rows.push(lines.len());
                    let selected = cursor == Some(n);
                    n += 1;
                    let took = if live { q.started.map(|s| fmt_ms((now - s).max(0.0) * 1000.0)).unwrap_or_else(|| micros(q.elapsed_us)) } else { micros(q.elapsed_us) };
                    let status_style = match q.status.as_str() {
                        "queued" => Style::new().fg(theme::WARN),
                        "running" | "returning" | "planning" => Style::new().fg(theme::RUNNING),
                        "failed" | "aborted" | "canceled" => Style::new().fg(theme::ERR),
                        _ => theme::muted(),
                    };
                    let mark = if selected { "▶ " } else { "  " };
                    lines.push(Line::from(vec![
                        Span::styled(mark, theme::accent()),
                        Span::raw(format!("{:<7} ", q.pid.map(|p| p.to_string()).unwrap_or_else(|| "—".into()))),
                        Span::raw(format!("{:<12} ", ellipsize(&q.user, 12))),
                        Span::styled(format!("{:<9} ", ellipsize(&q.status, 9)), status_style),
                        Span::raw(format!("{took:>9} ")),
                        Span::styled(format!("{:>9}  ", micros(q.queue_us)), if q.queue_us >= 1e6 { Style::new().fg(theme::WARN) } else { theme::muted() }),
                        Span::styled(ellipsize(&one_line(&q.text), text_w), if selected { theme::selected() } else { Style::new() }),
                    ]));
                }
            }
            Err(e) => lines.push(error_line(e)),
        }
        lines.push(Line::raw(""));
    };
    let (queued, running) = match &a.running {
        Ok(list) => {
            let q = list.iter().filter(|q| q.queued()).count();
            (q, list.len() - q)
        }
        Err(_) => (0, 0),
    };
    section(&mut lines, &format!("Now · {running} running · {queued} queued"), &a.running, true);
    section(&mut lines, "Slowest, last 24 hours", &a.slowest, false);
    section(&mut lines, "Longest waits in a queue, last 24 hours", &a.waited, false);
    lines.push(widgets::section("Blocked by locks"));
    match &a.locks {
        Ok(locks) if locks.is_empty() => lines.push(Line::styled("  none", theme::muted())),
        Ok(locks) => lines.extend(locks.iter().map(lock_line)),
        Err(e) => lines.push(error_line(e)),
    }
    (lines, rows)
}

fn lock_line(l: &Lock) -> Line<'static> {
    let pid = |p: Option<i64>| p.map(|p| p.to_string()).unwrap_or_else(|| "?".into());
    Line::from(vec![
        Span::styled("  ⚠ ", Style::new().fg(theme::WARN)),
        Span::raw(format!("session {} ({})", pid(l.waiting_pid), l.waiting_user)),
        Span::styled(format!(" has waited {} for {} on ", fmt_ms(l.waiting_s * 1000.0), l.wanted), theme::muted()),
        Span::styled(l.table.clone(), Style::new().add_modifier(Modifier::BOLD)),
        Span::styled(format!(", held by session {} ({}) for {}", pid(l.blocking_pid), l.blocking_user, fmt_ms(l.held_s * 1000.0)), theme::muted()),
    ])
}

/// How the Tables tab sorts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sort {
    Size,
    Unsorted,
    Stats,
    Skew,
}

impl Sort {
    pub fn next(self) -> Sort {
        match self {
            Sort::Size => Sort::Unsorted,
            Sort::Unsorted => Sort::Stats,
            Sort::Stats => Sort::Skew,
            Sort::Skew => Sort::Size,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Sort::Size => "size",
            Sort::Unsorted => "unsorted",
            Sort::Stats => "stats off",
            Sort::Skew => "skew",
        }
    }

    pub fn apply(self, tables: &mut [&TableInfo]) {
        let key = |t: &TableInfo| match self {
            Sort::Size => t.size_mb,
            Sort::Unsorted => t.unsorted.unwrap_or(-1.0),
            Sort::Stats => t.stats_off.unwrap_or(-1.0),
            Sort::Skew => t.skew_rows.unwrap_or(-1.0),
        };
        tables.sort_by(|a, b| key(b).total_cmp(&key(a)).then(a.name().cmp(&b.name())));
    }
}

/// The Tables tab: the largest tables with their upkeep flags.
pub fn tables(cluster: &Cluster, identity: Option<&Identity>, data: Option<&aws::Result<Vec<TableInfo>>>, sort: Sort, width: usize, tick: usize) -> Vec<Line<'static>> {
    if cluster.paused() {
        return vec![Line::styled("The cluster is paused.", theme::muted())];
    }
    let Some(identity) = identity else { return no_identity(cluster) };
    let mut lines = vec![Line::styled(format!("as {} · t sorts (by {}) · i changes the identity", identity.describe(), sort.label()), theme::muted())];
    let list = match data {
        None => {
            lines.push(loading(tick, "reading SVV_TABLE_INFO…"));
            return lines;
        }
        Some(Err(e)) => {
            lines.push(error_line(e));
            return lines;
        }
        Some(Ok(list)) => list,
    };
    let flagged = list.iter().filter(|t| !t.flags().is_empty()).count();
    lines.push(Line::styled(
        format!(
            "{} largest tables · {flagged} need upkeep (VACUUM above {:.0}% unsorted, ANALYZE above {:.0}% stale statistics, skew above {:.0}× rows on a slice)",
            list.len(),
            redshift::UNSORTED_MAX,
            redshift::STATS_OFF_MAX,
            redshift::SKEW_ROWS_MAX
        ),
        theme::muted(),
    ));
    lines.push(Line::raw(""));
    let mut sorted: Vec<&TableInfo> = list.iter().collect();
    sort.apply(&mut sorted);
    let name_w = sorted.iter().map(|t| t.name().chars().count()).max().unwrap_or(10).clamp(10, width.saturating_sub(80).clamp(16, 48));
    let dist_w = sorted.iter().map(|t| t.diststyle.chars().count()).max().unwrap_or(10).clamp(10, if width >= name_w + 110 { 24 } else { 14 });
    lines.push(Line::styled(
        format!("{:<name_w$} {:>9} {:>9} {:>9} {:>9} {:>6}  {:<dist_w$} flags", "table", "size", "rows", "unsorted", "stats off", "skew", "dist style"),
        theme::muted(),
    ));
    let pct = |v: Option<f64>| v.map(|v| format!("{v:.0}%")).unwrap_or_else(|| "—".into());
    for t in sorted {
        let flags = t.flags();
        let warn = |bad: bool| if bad { Style::new().fg(theme::WARN) } else { Style::new() };
        lines.push(Line::from(vec![
            Span::styled(format!("{:<name_w$} ", ellipsize(&t.name(), name_w)), Style::new().add_modifier(Modifier::BOLD)),
            Span::raw(format!("{:>9} ", t.size())),
            Span::raw(format!("{:>9} ", redshift::fmt_rows(t.rows))),
            Span::styled(format!("{:>9} ", pct(t.unsorted)), warn(t.unsorted.is_some_and(|u| u > redshift::UNSORTED_MAX))),
            Span::styled(format!("{:>9} ", pct(t.stats_off)), warn(t.stats_off.is_some_and(|s| s > redshift::STATS_OFF_MAX))),
            Span::styled(format!("{:>6}  ", t.skew_rows.map(|s| format!("{s:.1}")).unwrap_or_else(|| "—".into())), warn(t.skew_rows.is_some_and(|s| s > redshift::SKEW_ROWS_MAX))),
            Span::styled(format!("{:<dist_w$} ", ellipsize(&t.diststyle, dist_w)), theme::muted()),
            Span::styled(flags.join(", "), Style::new().fg(theme::WARN)),
        ]));
    }
    lines
}

fn severity_color(h: Health) -> ratatui::style::Color {
    match h {
        Health::Ok => theme::MUTED,
        other => health_color(other),
    }
}

/// The cluster's events of the last 7 days on a timeline and as a list.
pub fn events(cluster: &Cluster, events: Option<&aws::Result<Vec<Event>>>, width: usize, now: f64, tick: usize) -> Vec<Line<'static>> {
    let events = match events {
        None => return vec![loading(tick, "reading events…")],
        Some(Err(e)) => return vec![error_line(e)],
        Some(Ok(events)) => events.iter().filter(|e| e.source == cluster.id).collect::<Vec<_>>(),
    };
    const WEEK: f64 = 7.0 * 86_400.0;
    let track = width.saturating_sub(2).max(10);
    let mut lines = vec![widgets::section("Last 7 days")];
    let mut cells: Vec<(char, Style)> = vec![('·', theme::muted()); track];
    for e in &events {
        let x = (((e.date - (now - WEEK)) / WEEK) * track as f64) as isize;
        if let Some(cell) = usize::try_from(x).ok().and_then(|x| cells.get_mut(x.min(track - 1))) {
            let color = severity_color(e.severity());
            if cell.0 != '◆' || color != theme::MUTED {
                *cell = ('◆', Style::new().fg(color));
            }
        }
    }
    lines.push(Line::from(cells.into_iter().map(|(c, s)| Span::styled(c.to_string(), s)).collect::<Vec<_>>()));
    let mut axis = vec![' '; track];
    for (at, label) in [(0, format!("-{}", fmt_window(WEEK))), (track * 4 / 7, "-3d".into()), (track.saturating_sub(3), "now".into())] {
        for (k, c) in label.chars().enumerate() {
            if let Some(cell) = axis.get_mut(at + k) {
                *cell = c;
            }
        }
    }
    lines.push(Line::styled(axis.into_iter().collect::<String>(), theme::muted()));
    lines.push(Line::raw(""));
    if events.is_empty() {
        lines.push(Line::styled("No events in the last 7 days.", theme::muted()));
    }
    let category_w = events.iter().map(|e| e.categories.join(", ").chars().count()).max().unwrap_or(0).min(24);
    for e in events {
        lines.push(Line::from(vec![
            Span::styled("◆ ", Style::new().fg(severity_color(e.severity()))),
            Span::styled(format!("{:<9}", fmt_ago(e.date)), theme::muted()),
            Span::styled(format!("{:<category_w$}  ", ellipsize(&e.categories.join(", "), category_w)), theme::muted()),
            Span::raw(e.message.clone()),
        ]));
    }
    lines
}

/// The cluster's configuration (never a password), its pending changes and snapshots.
pub fn config(
    c: &Cluster,
    wlm: Option<&aws::Result<Wlm>>,
    snapshots: Option<&aws::Result<Vec<Snapshot>>>,
    identity: Option<&Identity>,
    tick: usize,
) -> Vec<Line<'static>> {
    let k = 24;
    let on = |b: bool| if b { "on".to_string() } else { "off".to_string() };
    let mut lines = vec![
        widgets::kv("Nodes", format!("{} · {} compute node{}", c.shape(), c.nodes_count, if c.nodes_count == 1 { "" } else { "s" }), k),
        widgets::kv("Status", Span::styled(c.status.clone(), Style::new().fg(health_color(redshift::status_health(&c.status)))), k),
    ];
    if let Some(a) = &c.availability {
        lines.push(widgets::kv("Availability", a.clone(), k));
    }
    lines.push(widgets::kv("Version", c.version.clone().unwrap_or_default(), k));
    if let Some(track) = &c.maintenance_track {
        lines.push(widgets::kv("Maintenance track", track.clone(), k));
    }
    lines.push(widgets::kv("Endpoint", format!("{}:{}", c.endpoint.clone().unwrap_or_else(|| "—".into()), c.port.unwrap_or(5439)), k));
    lines.push(widgets::kv("Database · master user", format!("{} · {}", c.database(), c.master_username.clone().unwrap_or_default()), k));
    let public = if c.publicly_accessible { Span::styled("yes ⚠ reachable from the internet (security groups decide who)", Style::new().fg(theme::WARN)) } else { Span::raw("no") };
    lines.push(widgets::kv("Publicly accessible", public, k));
    let encrypted = if c.encrypted { Span::raw("on") } else { Span::styled("off ⚠", Style::new().fg(theme::WARN)) };
    lines.push(widgets::kv("Encrypted", encrypted, k));
    lines.push(widgets::kv("Enhanced VPC routing", on(c.enhanced_vpc_routing), k));
    if let Some(m) = &c.multi_az {
        lines.push(widgets::kv("Multi-AZ", m.clone(), k));
    }
    lines.push(widgets::kv("VPC · zone", format!("{} · {}", c.vpc.clone().unwrap_or_default(), c.zone.clone().unwrap_or_default()), k));
    for (g, status) in &c.parameter_groups {
        let style = if status == "in-sync" { theme::muted() } else { Style::new().fg(theme::WARN) };
        lines.push(Line::from(vec![
            Span::styled(format!("{:<k$} ", "Parameter group"), theme::muted()),
            Span::raw(g.clone()),
            Span::styled(format!(" · {status}"), style),
        ]));
    }
    let wlm_text = match wlm {
        None => Span::styled(format!("{} reading…", theme::spinner(tick)), theme::muted()),
        Some(Ok(w)) => Span::raw(w.label()),
        Some(Err(e)) => Span::styled(e.short(), Style::new().fg(theme::ERR)),
    };
    lines.push(widgets::kv("Workload management", wlm_text, k));
    lines.push(widgets::kv(
        "Snapshots kept",
        format!(
            "automated {} days · manual {}",
            c.automated_retention.map(|d| d.to_string()).unwrap_or_else(|| "?".into()),
            match c.manual_retention {
                Some(-1) | None => "until deleted".to_string(),
                Some(d) => format!("{d} days"),
            }
        ),
        k,
    ));
    lines.push(widgets::kv("Maintenance window", c.maintenance_window.clone().unwrap_or_default(), k));
    if let Some(t) = c.next_maintenance {
        let now = crate::util::now_secs();
        let when = match t - now {
            ahead if ahead >= 86_400.0 => format!("in {:.0}d", ahead / 86_400.0),
            ahead if ahead > 0.0 => format!("in {:.0}h", (ahead / 3600.0).ceil()),
            _ => fmt_ago(t),
        };
        lines.push(widgets::kv("Next maintenance", format!("{} ({when})", fmt_utc(t)), k));
    }
    if let Some(o) = &c.elastic_options {
        lines.push(widgets::kv("Elastic resize to", format!("{o} nodes"), k));
    }
    if let Some(t) = c.created {
        lines.push(widgets::kv("Created", format!("{} ({})", fmt_utc(t), fmt_ago(t)), k));
    }
    lines.push(widgets::kv("Database identity", identity.map(Identity::describe).unwrap_or_else(|| "not set (i)".into()), k));
    if !c.pending.is_empty() || !c.pending_actions.is_empty() || c.resize.is_some() {
        lines.push(Line::raw(""));
        lines.push(widgets::section("Changes in progress or pending"));
        if let Some(r) = &c.resize {
            lines.push(Line::styled(format!("◆ {r} running"), Style::new().fg(theme::RUNNING)));
        }
        for p in c.pending.iter().chain(&c.pending_actions) {
            lines.push(Line::styled(format!("◆ {p}"), Style::new().fg(theme::WARN)));
        }
    }
    lines.push(Line::raw(""));
    lines.push(widgets::section("Snapshots"));
    match snapshots {
        None => lines.push(loading(tick, "reading snapshots…")),
        Some(Err(e)) => lines.push(error_line(e)),
        Some(Ok(list)) if list.is_empty() => lines.push(Line::styled("none", theme::muted())),
        Some(Ok(list)) => {
            for s in list.iter().take(40) {
                let size = s.size_mb.map(|mb| crate::aws::rds::fmt_size(mb * 1024.0 * 1024.0)).unwrap_or_else(|| "?".into());
                let kept = match (s.manual(), s.retention) {
                    (false, _) => String::new(),
                    (true, Some(-1) | None) => " · kept until deleted".into(),
                    (true, Some(d)) => format!(" · kept {d} days"),
                };
                lines.push(Line::from(vec![
                    Span::styled(format!("{:<9} ", s.kind), if s.manual() { Style::new().fg(theme::ACCENT) } else { theme::muted() }),
                    Span::raw(format!("{:<48} ", ellipsize(&s.id, 48))),
                    Span::styled(format!("{:>9}  {}{kept}", size, s.created.map(fmt_ago).unwrap_or_default()), theme::muted()),
                    Span::styled(if s.status == "available" { String::new() } else { format!("  {}", s.status) }, Style::new().fg(theme::RUNNING)),
                ]));
            }
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(lines: &[Line]) -> Vec<String> {
        lines.iter().map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect()).collect()
    }

    #[test]
    fn queries_list_in_cursor_order_and_mark_the_selected_one() {
        let c = Cluster { id: "dw".into(), status: "available".into(), ..Default::default() };
        let q = |pid: i64, status: &str, sql: &str| Query { pid: Some(pid), status: status.into(), user: "etl".into(), text: sql.into(), ..Default::default() };
        let a = Activity {
            running: Ok(vec![q(1, "running", "insert into t\n select 1"), q(2, "queued", "select 2")]),
            slowest: Ok(vec![q(3, "success", "select 3")]),
            waited: Err(AwsError::Network("x".into())),
            locks: Ok(vec![]),
            at: 0.0,
        };
        assert_eq!(listed(&a).iter().map(|q| q.pid).collect::<Vec<_>>(), [Some(1), Some(2), Some(3)]);
        let id = Identity { database: "dev".into(), db_user: Some("admin".into()), secret_arn: None };
        let (lines, rows) = queries(&c, Some(&id), Some(&a), Some(1), 120, 100.0, 0);
        assert_eq!(rows.len(), 3);
        let t = text(&lines);
        assert!(t[rows[0]].contains("insert into t select 1"), "{t:#?}");
        assert!(t[rows[1]].starts_with("▶ 2"), "{t:#?}");
        assert!(t.iter().any(|l| l.contains("NOW · 1 RUNNING · 1 QUEUED")), "{t:#?}");
        // Without an identity the tab says how to set one.
        let (lines, rows) = queries(&c, None, None, None, 120, 0.0, 0);
        assert!(rows.is_empty() && text(&lines).iter().any(|l| l.contains("i sets the identity")));
    }

    #[test]
    fn tables_sort_by_what_is_asked() {
        let t = |name: &str, size: f64, unsorted: f64| TableInfo { schema: "s".into(), table: name.into(), size_mb: size, unsorted: Some(unsorted), encoded: true, ..Default::default() };
        let all = [t("a", 10.0, 50.0), t("b", 100.0, 1.0), t("c", 50.0, 25.0)];
        let mut list: Vec<&TableInfo> = all.iter().collect();
        Sort::Size.apply(&mut list);
        assert_eq!(list.iter().map(|t| t.table.as_str()).collect::<Vec<_>>(), ["b", "c", "a"]);
        Sort::Unsorted.apply(&mut list);
        assert_eq!(list.iter().map(|t| t.table.as_str()).collect::<Vec<_>>(), ["a", "c", "b"]);
        assert_eq!(Sort::Skew.next(), Sort::Size);
    }
}
