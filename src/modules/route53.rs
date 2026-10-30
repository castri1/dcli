//! Route 53: hosted zones and their records (routing policies, alias targets, health checks);
//! creating records with values or as aliases to CloudFront, load balancers, API Gateway
//! domains or the zone's own records, and editing what a record answers. Every change is
//! followed until Route 53's name servers all answer with it.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Style, Stylize};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{List, ListItem, ListState, Paragraph, Wrap};

use super::common::{LoadState, filter_bar, matches_filter};
use crate::aws::route53::{self, Alias, Answer, Candidate, Change, Record, Zone, ZoneDetail};
use crate::aws::{self, AwsError};
use crate::config::is_production;
use crate::tui::form::{Field, Form, FormEvent};
use crate::tui::module::{Ctx, Emitter, Handled, Level, Module, Payload, View};
use crate::tui::picker::{Item, Outcome, Picker};
use crate::tui::theme;
use crate::tui::widgets::{self, InputEvent, TextInput};
use crate::util::ellipsize;

/// How often a pending change is checked, and for how long.
const SYNC_EVERY: Duration = Duration::from_secs(2);
const SYNC_FOR: Duration = Duration::from_secs(300);

type Key = (String, String, Option<String>);

enum Msg {
    Zones(aws::Result<Vec<Zone>>),
    Detail { zone: String, result: aws::Result<ZoneDetail> },
    Records { zone: String, page: Vec<Record> },
    RecordsDone { zone: String, result: aws::Result<()> },
    Candidates { zone: String, list: Vec<Candidate>, failed: Vec<String> },
    Saved { zone: String, key: Key, what: String, created: bool, result: aws::Result<Change> },
    Synced { id: String, result: aws::Result<Change> },
}

/// What a record will answer, by how its form is filled.
enum Mode {
    Values,
    /// An alias to a listed target (or the record's current one).
    Alias(Candidate),
    /// An alias to a DNS name and hosted zone typed by hand.
    Other,
}

struct Editing {
    form: Form,
    /// The record edited; none while creating one.
    old: Option<Record>,
    /// The record type, unless the form chooses it.
    kind: Option<String>,
    mode: Mode,
    busy: bool,
}

/// An entry of the alias target list.
#[derive(Clone)]
enum Pick {
    Loading,
    Candidate(usize),
    Keep,
    Other,
}

enum Dialog {
    /// The type of a new record.
    Kind(Picker),
    /// What an alias points at; `edit` is the alias record being changed.
    Target { picker: Picker, picks: Vec<Pick>, edit: Option<Box<Record>> },
    Form(Box<Editing>),
}

/// A change Route 53 accepted, followed until it is in sync.
struct Sync {
    id: String,
    what: String,
    since: Instant,
    checked: Instant,
    in_flight: bool,
}

pub struct Route53 {
    env_name: String,
    zones: Vec<Zone>,
    zones_load: LoadState,
    zone_filter: TextInput,
    zone_visible: Vec<usize>,
    zone_state: ListState,
    details: HashMap<String, aws::Result<ZoneDetail>>,
    details_in_flight: HashSet<String>,
    /// The zone whose records are shown.
    open: Option<Zone>,
    records: Vec<Record>,
    records_load: LoadState,
    filter: TextInput,
    visible: Vec<usize>,
    state: ListState,
    filtering: bool,
    dialog: Option<Dialog>,
    /// The open zone's alias targets and the sources that could not be listed, once read.
    candidates: Option<(Vec<Candidate>, Vec<String>)>,
    candidates_loading: bool,
    /// Record to select once the records reload.
    reselect: Option<Key>,
    /// Zone (id or name) to open once the zones are listed (from the `:` search).
    pending_open: Option<String>,
    syncing: Vec<Sync>,
    detail_scroll: u16,
    list_area: Rect,
}

impl Route53 {
    pub fn new() -> Route53 {
        Route53 {
            env_name: String::new(),
            zones: Vec::new(),
            zones_load: LoadState::Idle,
            zone_filter: TextInput::default(),
            zone_visible: Vec::new(),
            zone_state: ListState::default(),
            details: HashMap::new(),
            details_in_flight: HashSet::new(),
            open: None,
            records: Vec::new(),
            records_load: LoadState::Idle,
            filter: TextInput::default(),
            visible: Vec::new(),
            state: ListState::default(),
            filtering: false,
            dialog: None,
            candidates: None,
            candidates_loading: false,
            reselect: None,
            pending_open: None,
            syncing: Vec::new(),
            detail_scroll: 0,
            list_area: Rect::default(),
        }
    }

    // ---- zones ------------------------------------------------------------------------------

    fn load_zones(&mut self, ctx: &mut Ctx) {
        self.zones_load = LoadState::Loading;
        let aws = ctx.env().aws.clone();
        ctx.spawn(move || Msg::Zones(route53::list_zones(&aws)));
    }

    fn refilter_zones(&mut self) {
        let keep = self.selected_zone().map(|z| z.id.clone());
        let query = self.zone_filter.value.trim().to_lowercase();
        self.zone_visible = (0..self.zones.len())
            .filter(|&i| {
                let z = &self.zones[i];
                matches_filter(&format!("{} {} {} {}", z.name, z.kind(), z.id, z.comment.as_deref().unwrap_or("")), &query)
            })
            .collect();
        let index = keep.and_then(|k| self.zone_visible.iter().position(|&i| self.zones[i].id == k)).unwrap_or(0);
        self.zone_state.select(if self.zone_visible.is_empty() { None } else { Some(index) });
    }

    fn selected_zone(&self) -> Option<&Zone> {
        self.zones.get(*self.zone_visible.get(self.zone_state.selected()?)?)
    }

    fn select_zone(&mut self, index: usize, ctx: &mut Ctx) {
        if !self.zone_visible.is_empty() {
            self.zone_state.select(Some(index.min(self.zone_visible.len() - 1)));
            self.detail_scroll = 0;
            self.load_detail(ctx);
        }
    }

    /// Name servers and VPCs of the selected zone, read once.
    fn load_detail(&mut self, ctx: &mut Ctx) {
        let Some(id) = self.selected_zone().map(|z| z.id.clone()) else { return };
        if self.details.contains_key(&id) || !self.details_in_flight.insert(id.clone()) {
            return;
        }
        let aws = ctx.env().aws.clone();
        ctx.spawn(move || {
            let result = route53::zone_detail(&aws, &id);
            Msg::Detail { zone: id, result }
        });
    }

    fn open_zone(&mut self, ctx: &mut Ctx) {
        let Some(zone) = self.selected_zone().cloned() else { return };
        self.open = Some(zone);
        self.filter.clear();
        self.filtering = false;
        self.candidates = None;
        self.candidates_loading = false;
        self.load_records(ctx);
    }

    // ---- records ----------------------------------------------------------------------------

    fn load_records(&mut self, ctx: &mut Ctx) {
        let Some(zone) = self.open.as_ref().map(|z| z.id.clone()) else { return };
        if self.reselect.is_none() {
            self.reselect = self.selected().map(Record::key);
        }
        self.records.clear();
        self.visible.clear();
        self.state.select(None);
        self.records_load = LoadState::Loading;
        let aws = ctx.env().aws.clone();
        ctx.spawn_stream(move |em: Emitter<Msg>| {
            let mut next = None;
            loop {
                match route53::list_records(&aws, &zone, next.as_ref()) {
                    Ok((page, more)) => {
                        if !em.emit(Msg::Records { zone: zone.clone(), page }) {
                            return;
                        }
                        match more {
                            Some(n) => next = Some(n),
                            None => break,
                        }
                    }
                    Err(e) => {
                        em.emit(Msg::RecordsDone { zone, result: Err(e) });
                        return;
                    }
                }
            }
            em.emit(Msg::RecordsDone { zone, result: Ok(()) });
        });
    }

    fn zone_name(&self) -> &str {
        self.open.as_ref().map_or("", |z| z.name.as_str())
    }

    fn refilter(&mut self) {
        let keep = self.selected().map(Record::key);
        let query = self.filter.value.trim().to_lowercase();
        let zone = self.zone_name().to_string();
        self.visible = (0..self.records.len())
            .filter(|&i| {
                let r = &self.records[i];
                let text = format!(
                    "{} {} {} {} {}",
                    r.relative(&zone),
                    r.name,
                    r.kind,
                    r.summary(),
                    r.routing.as_deref().unwrap_or("")
                );
                matches_filter(&text, &query)
            })
            .collect();
        let index = keep.and_then(|k| self.visible.iter().position(|&i| self.records[i].key() == k)).unwrap_or(0);
        self.state.select(if self.visible.is_empty() { None } else { Some(index) });
    }

    fn selected(&self) -> Option<&Record> {
        self.records.get(*self.visible.get(self.state.selected()?)?)
    }

    fn refilter_current(&mut self, ctx: &mut Ctx) {
        if self.open.is_some() {
            self.refilter();
        } else {
            self.refilter_zones();
            self.load_detail(ctx);
        }
    }

    /// Moves the selection of the list shown.
    fn step(&mut self, delta: isize, ctx: &mut Ctx) {
        if self.open.is_some() {
            self.select(self.state.selected().unwrap_or(0).saturating_add_signed(delta));
        } else {
            self.select_zone(self.zone_state.selected().unwrap_or(0).saturating_add_signed(delta), ctx);
        }
    }

    fn select(&mut self, index: usize) {
        if !self.visible.is_empty() {
            let index = index.min(self.visible.len() - 1);
            if self.state.selected() != Some(index) {
                self.detail_scroll = 0;
            }
            self.state.select(Some(index));
        }
    }

    // ---- new and edited records -------------------------------------------------------------

    fn new_record(&mut self) {
        if !matches!(self.records_load, LoadState::Loaded) {
            return;
        }
        let mut items: Vec<Item> = route53::TYPES.iter().map(|(t, d)| Item::new(*t, *d)).collect();
        items.push(Item::new("Alias", "to CloudFront, a load balancer, an API Gateway domain or a record in this zone"));
        let title = format!("New record in {} · {}", self.zone_name().trim_end_matches('.'), self.env_name);
        self.dialog = Some(Dialog::Kind(Picker::new(title, "next", items, false)));
    }

    fn edit_record(&mut self, ctx: &mut Ctx) {
        let Some(r) = self.selected().cloned() else { return };
        if let Some(why) = r.locked(self.zone_name()) {
            return ctx.toast(Level::Warn, why);
        }
        if r.alias.is_some() {
            self.open_targets(Some(r), ctx);
        } else {
            self.dialog = Some(Dialog::Form(Box::new(self.values_form(&r.kind.clone(), Some(r)))));
        }
    }

    /// The form for a record answering values of type `kind`.
    fn values_form(&self, kind: &str, old: Option<Record>) -> Editing {
        let zone = self.zone_name().trim_end_matches('.');
        let mut fields = Vec::new();
        let title = match &old {
            Some(r) => {
                fields.push(Field::fixed("Record", r.name.clone()));
                fields.push(Field::fixed("Type", r.kind.clone()));
                if let Some(routing) = routing(r) {
                    fields.push(Field::fixed("Routing", routing));
                }
                fields.push(Field::text("TTL", r.ttl.unwrap_or(route53::DEFAULT_TTL).to_string()));
                fields.push(Field::text("Values", r.values.join(", ")).hint(route53::example(kind)));
                format!("Edit {} {} · {}", r.relative(self.zone_name()), r.kind, self.env_name)
            }
            None => {
                fields.push(Field::text("Name", "").optional("api · empty or @ for the zone apex"));
                fields.push(Field::fixed("Type", kind));
                fields.push(Field::text("TTL", route53::DEFAULT_TTL.to_string()));
                fields.push(Field::text("Values", "").hint(route53::example(kind)));
                format!("New {kind} record in {zone} · {}", self.env_name)
            }
        };
        self.editing(Form::new(title, fields), old, Some(kind.to_string()), Mode::Values)
    }

    fn editing(&self, form: Form, old: Option<Record>, kind: Option<String>, mode: Mode) -> Editing {
        let form = if is_production(&self.env_name) { form.confirm_with(&self.env_name) } else { form };
        let mut editing = Editing { form, old, kind, mode, busy: false };
        editing.refresh(self.zone_name());
        editing
    }

    /// Lists alias targets (once per zone) and opens the list; `edit` is the alias record
    /// being changed.
    fn open_targets(&mut self, edit: Option<Record>, ctx: &mut Ctx) {
        if self.candidates.is_none() && !self.candidates_loading {
            self.candidates_loading = true;
            let aws = ctx.env().aws.clone();
            let zone = self.open.clone().unwrap_or_default();
            let records = self.records.clone();
            ctx.spawn(move || {
                let (list, failed) = route53::candidates(&aws, &zone, &records);
                Msg::Candidates { zone: zone.id, list, failed }
            });
        }
        let (items, picks) = self.target_items(edit.as_ref());
        let cursor = picks.iter().position(|p| matches!(p, Pick::Keep)).unwrap_or(0);
        let title = match &edit {
            Some(r) => format!("{} {} points at", r.relative(self.zone_name()), r.kind),
            None => "Alias to".to_string(),
        };
        let picker = Picker::new(title, "choose", items, false).with_cursor(cursor);
        self.dialog = Some(Dialog::Target { picker, picks, edit: edit.map(Box::new) });
    }

    fn target_items(&self, edit: Option<&Record>) -> (Vec<Item>, Vec<Pick>) {
        let (mut items, mut picks) = (Vec::new(), Vec::new());
        if let Some(a) = edit.and_then(|r| r.alias.as_ref()) {
            items.push(Item::new(format!("Keep {}", a.dns_name.trim_end_matches('.')), route53::alias_kind(a, &self.zone_id())).current(true));
            picks.push(Pick::Keep);
        }
        match &self.candidates {
            None => {
                items.push(Item::new("Listing CloudFront, load balancers, API Gateway domains…", ""));
                picks.push(Pick::Loading);
            }
            Some((list, failed)) => {
                for (i, c) in list.iter().enumerate() {
                    let fits = edit.is_none_or(|r| c.kinds.contains(&r.kind) && !(r.name.eq_ignore_ascii_case(&c.alias.dns_name)));
                    if fits {
                        items.push(Item::new(c.label.clone(), c.detail.clone()));
                        picks.push(Pick::Candidate(i));
                    }
                }
                for f in failed {
                    items.push(Item::new(format!("✗ {f}"), "not listed"));
                    picks.push(Pick::Loading);
                }
            }
        }
        items.push(Item::new("Other target…", "type a DNS name and its hosted zone ID (S3 website, VPC endpoint, …)"));
        picks.push(Pick::Other);
        (items, picks)
    }

    fn zone_id(&self) -> String {
        self.open.as_ref().map(|z| z.id.clone()).unwrap_or_default()
    }

    /// The form for an alias record pointing at `pick`.
    fn alias_form(&self, pick: Pick, old: Option<Record>) -> Option<Editing> {
        let zone = self.zone_name().trim_end_matches('.').to_string();
        let candidate = match &pick {
            Pick::Candidate(i) => Some(self.candidates.as_ref()?.0.get(*i)?.clone()),
            Pick::Keep => {
                let r = old.as_ref()?;
                let a = r.alias.clone()?;
                let label = a.dns_name.trim_end_matches('.').to_string();
                Some(Candidate {
                    source: route53::alias_kind(&a, &self.zone_id()),
                    detail: String::new(),
                    label,
                    alias: a,
                    kinds: vec![r.kind.clone()],
                    serves: Vec::new(),
                })
            }
            Pick::Other => None,
            Pick::Loading => return None,
        };
        let mut fields = Vec::new();
        let health = |a: Option<&Alias>| if a.is_some_and(|a| a.evaluate_health) { "yes" } else { "no" };
        let kind = match &old {
            Some(r) => {
                fields.push(Field::fixed("Record", r.name.clone()));
                fields.push(Field::fixed("Type", r.kind.clone()));
                if let Some(routing) = routing(r) {
                    fields.push(Field::fixed("Routing", routing));
                }
                Some(r.kind.clone())
            }
            None => {
                fields.push(Field::text("Name", "").optional("api · empty or @ for the zone apex"));
                match &candidate {
                    Some(c) if c.kinds.len() == 1 => {
                        fields.push(Field::fixed("Type", c.kinds[0].clone()));
                        Some(c.kinds[0].clone())
                    }
                    _ => {
                        fields.push(Field::choice("Type", &["A", "AAAA"], "A"));
                        None
                    }
                }
            }
        };
        let current = old.as_ref().and_then(|r| r.alias.as_ref());
        let mode = match candidate {
            Some(c) => {
                fields.push(Field::fixed("Target", format!("{} · {}", c.label, c.source)));
                let evaluate = if matches!(pick, Pick::Keep) { health(current) } else { health(Some(&c.alias)) };
                fields.push(Field::choice("Evaluate health", &["no", "yes"], evaluate));
                Mode::Alias(c)
            }
            None => {
                fields.push(Field::text("DNS name", current.map(|a| a.dns_name.trim_end_matches('.').to_string()).unwrap_or_default()).hint("my-site.s3-website-us-east-1.amazonaws.com"));
                fields.push(Field::text("Hosted zone ID", current.map(|a| a.zone_id.clone()).unwrap_or_default()).hint("the target's hosted zone, like Z3AQBSTGFYJSTF"));
                fields.push(Field::choice("Evaluate health", &["no", "yes"], health(current)));
                Mode::Other
            }
        };
        let title = match &old {
            Some(r) => format!("Edit {} {} · {}", r.relative(self.zone_name()), r.kind, self.env_name),
            None => format!("New alias in {zone} · {}", self.env_name),
        };
        Some(self.editing(Form::new(title, fields), old, kind, mode))
    }

    fn submit(&mut self, ctx: &mut Ctx) {
        let Some(zone) = self.open.clone() else { return };
        let Some(Dialog::Form(editing)) = &mut self.dialog else { return };
        let (name, kind, answer) = match prepare(editing, &zone, &self.records) {
            Ok(p) => p,
            Err(e) => return editing.form.fail(e),
        };
        editing.busy = true;
        let aws = ctx.env().aws.clone();
        let old = editing.old.clone();
        let what = format!("{} {kind}", name.trim_end_matches('.'));
        let key = (name.clone(), kind.clone(), old.as_ref().and_then(|r| r.set_id.clone()));
        ctx.spawn(move || {
            let result = match &old {
                Some(old) => route53::edit_record(&aws, &zone.id, old, &answer),
                None => route53::create_record(&aws, &zone.id, &name, &kind, &answer),
            };
            Msg::Saved { zone: zone.id, key, what, created: old.is_none(), result }
        });
    }

    fn key_dialog(&mut self, key: KeyEvent, ctx: &mut Ctx) {
        let Some(dialog) = self.dialog.take() else { return };
        match dialog {
            Dialog::Kind(mut picker) => match picker.handle(key) {
                Outcome::Pending => self.dialog = Some(Dialog::Kind(picker)),
                Outcome::Cancel => {}
                Outcome::Done(picks) => match picks.first() {
                    Some(&i) if i < route53::TYPES.len() => {
                        self.dialog = Some(Dialog::Form(Box::new(self.values_form(route53::TYPES[i].0, None))));
                    }
                    Some(_) => self.open_targets(None, ctx),
                    None => self.dialog = Some(Dialog::Kind(picker)),
                },
            },
            Dialog::Target { mut picker, picks, edit } => match picker.handle(key) {
                Outcome::Pending => self.dialog = Some(Dialog::Target { picker, picks, edit }),
                Outcome::Cancel => {}
                Outcome::Done(chosen) => {
                    let pick = chosen.first().and_then(|&i| picks.get(i)).cloned().unwrap_or(Pick::Loading);
                    match self.alias_form(pick, edit.as_deref().cloned()) {
                        Some(editing) => self.dialog = Some(Dialog::Form(Box::new(editing))),
                        None => self.dialog = Some(Dialog::Target { picker, picks, edit }),
                    }
                }
            },
            Dialog::Form(mut editing) => {
                if editing.busy {
                    self.dialog = Some(Dialog::Form(editing));
                    return;
                }
                match editing.form.handle(key) {
                    FormEvent::Cancel => {}
                    FormEvent::Submit => {
                        self.dialog = Some(Dialog::Form(editing));
                        self.submit(ctx);
                    }
                    FormEvent::Pending => {
                        editing.refresh(self.zone_name());
                        self.dialog = Some(Dialog::Form(editing));
                    }
                }
            }
        }
    }

    // ---- changes in sync --------------------------------------------------------------------

    fn poll_changes(&mut self, ctx: &mut Ctx) {
        let now = Instant::now();
        for s in self.syncing.iter_mut().filter(|s| !s.in_flight && now.duration_since(s.checked) >= SYNC_EVERY) {
            s.in_flight = true;
            s.checked = now;
            let (aws, id) = (ctx.env().aws.clone(), s.id.clone());
            ctx.spawn(move || {
                let result = route53::change_status(&aws, &id);
                Msg::Synced { id, result }
            });
        }
    }

    // ---- rendering --------------------------------------------------------------------------

    fn render_zones(&mut self, f: &mut Frame, area: Rect, view: &View) {
        let [left, right] = if area.width < 90 {
            area.layout(&Layout::vertical([Constraint::Percentage(45), Constraint::Fill(1)]))
        } else {
            area.layout(&Layout::horizontal([Constraint::Percentage(40), Constraint::Fill(1)]))
        };
        self.list_area = left;
        let [bar, list_area] = left.layout(&Layout::vertical([Constraint::Length(1), Constraint::Fill(1)]));
        let (line, cursor) = filter_bar(&self.zone_filter, self.filtering, "type / to search hosted zones");
        f.render_widget(Paragraph::new(line), bar);
        if let Some(col) = cursor {
            f.set_cursor_position((bar.x + col, bar.y));
        }
        let title = match &self.zones_load {
            LoadState::Loading => format!(" {} Hosted zones ", theme::spinner(view.tick)),
            _ => format!(" Hosted zones · {} ", self.zone_visible.len()),
        };
        if self.zone_visible.is_empty() {
            let text = match &self.zones_load {
                LoadState::Failed(e) => Line::styled(e.short(), Style::new().fg(theme::ERR)),
                LoadState::Loading => Line::styled("Loading hosted zones…", theme::muted()),
                _ => Line::styled("No hosted zones", theme::muted()),
            };
            f.render_widget(widgets::placeholder(text).block(widgets::panel(title, true)), list_area);
        } else {
            let width = list_area.width.saturating_sub(4) as usize;
            let items: Vec<ListItem> = self
                .zone_visible
                .iter()
                .map(|&i| {
                    let z = &self.zones[i];
                    let count = format!("{} records", z.records);
                    let tag = if z.private { " private" } else { "" };
                    let name = ellipsize(z.domain(), width.saturating_sub(count.len() + tag.len() + 2));
                    let pad = width.saturating_sub(name.chars().count() + tag.len() + count.len());
                    ListItem::new(Line::from(vec![
                        Span::raw(name),
                        Span::styled(tag, Style::new().fg(theme::APPROVAL)),
                        Span::raw(" ".repeat(pad)),
                        Span::styled(count, theme::muted()),
                    ]))
                })
                .collect();
            let list = List::new(items).block(widgets::panel(title, true)).highlight_style(theme::selected());
            f.render_stateful_widget(list, list_area, &mut self.zone_state);
        }
        self.render_zone_detail(f, right, view);
    }

    fn render_zone_detail(&self, f: &mut Frame, area: Rect, view: &View) {
        let Some(z) = self.selected_zone() else {
            f.render_widget(widgets::placeholder(Line::styled("Select a hosted zone", theme::muted())).block(widgets::panel(" Zone ", false)), area);
            return;
        };
        let kw = 13;
        let mut lines = vec![
            widgets::kv("Id", z.id.clone(), kw),
            widgets::kv("Type", if z.private { "private (answers inside its VPCs)" } else { "public" }, kw),
            widgets::kv("Records", z.records.to_string(), kw),
        ];
        if let Some(c) = &z.comment {
            lines.push(widgets::kv("Comment", c.clone(), kw));
        }
        if let Some(m) = &z.managed_by {
            lines.push(widgets::kv("Managed by", m.clone(), kw));
        }
        lines.push(Line::raw(""));
        match self.details.get(&z.id) {
            None => lines.push(Line::from(vec![Span::styled(theme::spinner(view.tick), theme::accent()), Span::raw(" loading…")])),
            Some(Err(e)) => lines.push(Line::styled(e.short(), Style::new().fg(theme::ERR))),
            Some(Ok(d)) => {
                if !d.name_servers.is_empty() {
                    lines.push(widgets::section("Name servers"));
                    lines.extend(d.name_servers.iter().map(|n| Line::raw(n.clone())));
                }
                if !d.vpcs.is_empty() {
                    lines.push(widgets::section("VPCs"));
                    lines.extend(d.vpcs.iter().map(|(region, id)| Line::from(vec![Span::raw(id.clone()), Span::styled(format!("  {region}"), theme::muted())])));
                }
            }
        }
        lines.push(Line::raw(""));
        lines.push(Line::styled("Enter shows its records", theme::muted()));
        let block = widgets::panel(Line::from(format!(" {} ", z.domain())).bold(), false);
        f.render_widget(Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false }).scroll((self.detail_scroll, 0)).block(block), area);
    }

    fn render_records(&mut self, f: &mut Frame, area: Rect, view: &View) {
        let [top, bottom] = if area.width >= 150 {
            area.layout(&Layout::horizontal([Constraint::Percentage(62), Constraint::Fill(1)]))
        } else {
            area.layout(&Layout::vertical([Constraint::Percentage(60), Constraint::Fill(1)]))
        };
        self.list_area = top;
        let [bar, list_area] = top.layout(&Layout::vertical([Constraint::Length(1), Constraint::Fill(1)]));
        let (line, cursor) = filter_bar(&self.filter, self.filtering, "type / to search records: name, type, value");
        f.render_widget(Paragraph::new(line), bar);
        if let Some(col) = cursor {
            f.set_cursor_position((bar.x + col, bar.y));
        }
        let zone = self.open.clone().unwrap_or_default();
        let mut title = vec![Span::raw(format!(" {} ", zone.domain())).bold()];
        if zone.private {
            title.push(Span::styled("private ", Style::new().fg(theme::APPROVAL)));
        }
        title.push(Span::raw(match &self.records_load {
            LoadState::Loading => format!("· {} {} ", theme::spinner(view.tick), self.records.len()),
            _ if self.filter.value.is_empty() => format!("· {} records ", self.records.len()),
            _ => format!("· {} of {} ", self.visible.len(), self.records.len()),
        }));
        if !self.syncing.is_empty() {
            title.push(Span::styled(format!("· {} syncing {} ", theme::spinner(view.tick), self.syncing.len()), Style::new().fg(theme::RUNNING)));
        }
        let title = Line::from(title);
        if self.visible.is_empty() {
            let text = match &self.records_load {
                LoadState::Failed(e) => Line::styled(e.short(), Style::new().fg(theme::ERR)),
                LoadState::Loading => Line::styled("Loading records…", theme::muted()),
                _ if self.records.is_empty() => Line::styled("No records · n creates one", theme::muted()),
                _ => Line::styled("No record matches", theme::muted()),
            };
            f.render_widget(widgets::placeholder(text).block(widgets::panel(title, true)), list_area);
        } else {
            let width = list_area.width.saturating_sub(4) as usize;
            let rows: Vec<&Record> = self.visible.iter().map(|&i| &self.records[i]).collect();
            let name_w = rows.iter().map(|r| r.relative(&zone.name).chars().count()).max().unwrap_or(1).clamp(1, 36);
            let routing_w = rows.iter().filter_map(|r| r.routing.as_ref().map(|x| x.chars().count())).max().unwrap_or(0).min(22);
            let items: Vec<ListItem> = rows
                .iter()
                .map(|r| {
                    let locked = r.locked(&zone.name).is_some();
                    let ttl = r.ttl.map_or_else(|| "alias".to_string(), |t| t.to_string());
                    let routing = r.routing.clone().unwrap_or_default();
                    let fixed = name_w + 2 + 5 + 2 + 6 + 2 + if routing_w > 0 { routing_w + 2 } else { 0 };
                    let mut spans = vec![
                        Span::styled(format!("{:<name_w$}  ", ellipsize(&r.relative(&zone.name), name_w)), if locked { theme::muted() } else { Style::new() }),
                        Span::styled(format!("{:<5}  ", r.kind), theme::accent()),
                        Span::styled(format!("{ttl:>6}  "), if r.alias.is_some() { Style::new().fg(theme::APPROVAL) } else { theme::muted() }),
                    ];
                    if routing_w > 0 {
                        spans.push(Span::styled(format!("{:<routing_w$}  ", ellipsize(&routing, routing_w)), Style::new().fg(theme::WARN)));
                    }
                    let value = match &r.alias {
                        Some(a) => a.dns_name.trim_end_matches('.').to_string(),
                        None => r.values.join(", "),
                    };
                    spans.push(Span::styled(ellipsize(&value, width.saturating_sub(fixed)), if locked { theme::muted() } else { Style::new() }));
                    ListItem::new(Line::from(spans))
                })
                .collect();
            let list = List::new(items).block(widgets::panel(title, true)).highlight_style(theme::selected());
            f.render_stateful_widget(list, list_area, &mut self.state);
        }
        self.render_record(f, bottom, &zone);
    }

    fn render_record(&self, f: &mut Frame, area: Rect, zone: &Zone) {
        let Some(r) = self.selected() else {
            f.render_widget(widgets::placeholder(Line::styled("Select a record", theme::muted())).block(widgets::panel(" Record ", false)), area);
            return;
        };
        let kw = 15;
        let mut lines = vec![widgets::kv("Name", r.name.clone(), kw), widgets::kv("Type", r.kind.clone(), kw)];
        if let Some(t) = r.ttl {
            lines.push(widgets::kv("TTL", format!("{t} s"), kw));
        }
        if let Some(routing) = routing(r) {
            lines.push(widgets::kv("Routing", routing, kw));
        }
        if let Some(h) = &r.health_check {
            lines.push(widgets::kv("Health check", h.clone(), kw));
        }
        if let Some(t) = &r.traffic_policy {
            lines.push(widgets::kv("Traffic policy", t.clone(), kw));
        }
        lines.push(Line::raw(""));
        match &r.alias {
            Some(a) => {
                lines.push(widgets::section("Alias to"));
                lines.push(Line::styled(a.dns_name.clone(), Style::new().fg(theme::APPROVAL)));
                lines.push(widgets::kv("What", route53::alias_kind(a, &zone.id), kw));
                lines.push(widgets::kv("Hosted zone", a.zone_id.clone(), kw));
                lines.push(widgets::kv("Evaluate health", if a.evaluate_health { "yes" } else { "no" }, kw));
            }
            None => {
                lines.push(widgets::section(if r.values.len() == 1 { "Value" } else { "Values" }));
                lines.extend(r.values.iter().map(|v| Line::raw(v.clone())));
            }
        }
        lines.push(Line::raw(""));
        match r.locked(&zone.name) {
            Some(why) => lines.push(Line::styled(why, theme::muted())),
            None => lines.push(Line::styled("Enter edits what it answers", theme::muted())),
        }
        let block = widgets::panel(Line::from(format!(" {} {} ", r.relative(&zone.name), r.kind)).bold(), false);
        f.render_widget(Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false }).scroll((self.detail_scroll, 0)).block(block), area);
    }
}

impl Editing {
    /// Shows where the record will be and what to mind, as the form is filled.
    fn refresh(&mut self, zone: &str) {
        let name = match &self.old {
            Some(r) => Ok(r.name.clone()),
            None => route53::full_name(self.form.get("Name"), zone),
        };
        let mut note = match (&self.old, &name) {
            (Some(r), _) => format!("Now: {}. Route 53 swaps it in one change; if it changed since it was read, nothing changes.", r.answer().describe()),
            (None, Ok(n)) => format!("→ {n}"),
            (None, Err(e)) => format!("✗ {e}"),
        };
        match &self.mode {
            Mode::Values => {
                let kind = self.kind.as_deref().unwrap_or("");
                note.push_str(if kind == "TXT" { " · separate values with commas; quotes are added" } else { " · separate values with commas" });
            }
            Mode::Alias(c) => {
                if let Some(w) = name.as_ref().ok().and_then(|n| c.warning(n)) {
                    note.push_str(&format!(" · ⚠ {w}"));
                }
            }
            Mode::Other => {}
        }
        self.form.note = Some(note);
    }
}

/// The record's routing policy and set identifier, if it has one.
fn routing(r: &Record) -> Option<String> {
    let routing = r.routing.clone()?;
    Some(match &r.set_id {
        Some(id) => format!("{routing} · {id}"),
        None => routing,
    })
}

/// The record a filled form describes: (full name, type, what it answers).
fn prepare(editing: &Editing, zone: &Zone, records: &[Record]) -> Result<(String, String, Answer), String> {
    let form = &editing.form;
    let name = match &editing.old {
        Some(r) => r.name.clone(),
        None => route53::full_name(form.get("Name"), &zone.name)?,
    };
    let kind = editing.kind.clone().unwrap_or_else(|| form.get("Type").to_string());
    let evaluate = form.get("Evaluate health") == "yes";
    let answer = match &editing.mode {
        Mode::Values => Answer::Values {
            ttl: route53::check_ttl(form.get("TTL"))?,
            values: route53::check_values(&kind, &name, &zone.name, &route53::split_values(form.get("Values")))?,
        },
        Mode::Alias(c) => {
            if !c.kinds.contains(&kind) {
                return Err(format!("{} answers {} records, not {kind}", c.label, c.kinds.join(" and ")));
            }
            if c.alias.dns_name.eq_ignore_ascii_case(&name) && c.alias.zone_id == zone.id {
                return Err("A record cannot be an alias to itself".into());
            }
            Answer::Alias(Alias { evaluate_health: evaluate, ..c.alias.clone() })
        }
        Mode::Other => {
            let dns = form.get("DNS name").trim().trim_end_matches('.').to_lowercase();
            let target_zone = form.get("Hosted zone ID").trim().to_string();
            if dns.is_empty() || dns.contains(char::is_whitespace) {
                return Err("Write the target's DNS name".into());
            }
            if target_zone.is_empty() || !target_zone.chars().all(|c| c.is_ascii_alphanumeric()) {
                return Err("Write the target's hosted zone ID (letters and digits, like Z3AQBSTGFYJSTF)".into());
            }
            Answer::Alias(Alias { zone_id: target_zone, dns_name: format!("{dns}."), evaluate_health: evaluate })
        }
    };
    match &editing.old {
        Some(old) if old.answer() == answer => return Err("Nothing changed".into()),
        Some(_) => {}
        None => {
            if route53::find_record(records, &name, &kind, None).is_some() {
                return Err(format!("{name} already has a {kind} record: Esc, then Enter on it edits it"));
            }
        }
    }
    Ok((name, kind, answer))
}

/// A Route 53 refusal, said for what can be done about it.
fn refusal(e: &AwsError) -> String {
    let text = e.short();
    if text.contains("do not match the current values") || text.contains("but it was not found") {
        "The record changed since it was read: nothing was changed. Esc, r reloads, then edit it again".into()
    } else {
        text
    }
}

fn copy(ctx: &mut Ctx, text: &str, done: &str) {
    let ok = crate::platform::copy_to_clipboard(text);
    ctx.toast(if ok { Level::Success } else { Level::Warn }, if ok { done } else { "Clipboard not available" });
}

impl Module for Route53 {
    fn title(&self) -> &'static str {
        "Route 53"
    }

    fn description(&self) -> &'static str {
        "Route 53 · hosted zones and their records; create records and aliases, edit them"
    }

    fn reset(&mut self, ctx: &mut Ctx) {
        *self = Route53::new();
        self.env_name = ctx.env().name.clone();
    }

    fn activate(&mut self, ctx: &mut Ctx) {
        if matches!(self.zones_load, LoadState::Idle) {
            self.load_zones(ctx);
        }
    }

    /// Opens hosted zone `target` (its id, or its name), now or once the zones are listed.
    fn open(&mut self, target: &str, ctx: &mut Ctx) {
        self.dialog = None;
        if !matches!(self.zones_load, LoadState::Loaded) {
            self.pending_open = Some(target.to_string());
            return;
        }
        self.zone_filter.clear();
        self.filtering = false;
        self.refilter_zones();
        let found = route53::find_zone(&self.zones, target).ok().map(|z| z.id.clone());
        if let Some(pos) = found.and_then(|id| self.zone_visible.iter().position(|&i| self.zones[i].id == id)) {
            self.select_zone(pos, ctx);
            self.open_zone(ctx);
        }
    }

    fn key(&mut self, key: KeyEvent, ctx: &mut Ctx) -> Handled {
        if self.dialog.is_some() {
            self.key_dialog(key, ctx);
            return Handled::Yes;
        }
        let in_records = self.open.is_some();
        if self.filtering {
            let input = if in_records { &mut self.filter } else { &mut self.zone_filter };
            match input.handle(key) {
                InputEvent::Changed => self.refilter_current(ctx),
                InputEvent::Submit => self.filtering = false,
                InputEvent::Cancel => {
                    input.clear();
                    self.filtering = false;
                    self.refilter_current(ctx);
                }
                InputEvent::Ignored => match key.code {
                    KeyCode::Up => self.step(-1, ctx),
                    KeyCode::Down => self.step(1, ctx),
                    _ => {}
                },
                InputEvent::Moved => {}
            }
            return Handled::Yes;
        }
        if in_records {
            let selected = self.state.selected().unwrap_or(0);
            match key.code {
                KeyCode::Up | KeyCode::Char('k') => self.select(selected.saturating_sub(1)),
                KeyCode::Down | KeyCode::Char('j') => self.select(selected + 1),
                KeyCode::PageUp => self.detail_scroll = self.detail_scroll.saturating_sub(10),
                KeyCode::PageDown => self.detail_scroll = self.detail_scroll.saturating_add(10),
                KeyCode::Home => self.select(0),
                KeyCode::End => self.select(usize::MAX),
                KeyCode::Char('/') => self.filtering = true,
                KeyCode::Esc if !self.filter.value.is_empty() => {
                    self.filter.clear();
                    self.refilter();
                }
                KeyCode::Esc | KeyCode::Left | KeyCode::Backspace => {
                    self.open = None;
                    self.records.clear();
                    self.visible.clear();
                    self.records_load = LoadState::Idle;
                    self.detail_scroll = 0;
                }
                KeyCode::Char('n') => self.new_record(),
                KeyCode::Enter => self.edit_record(ctx),
                KeyCode::Char('y') => {
                    if let Some(name) = self.selected().map(|r| r.name.trim_end_matches('.').to_string()) {
                        copy(ctx, &name, "Name copied");
                    }
                }
                KeyCode::Char('c') => {
                    if let Some(r) = self.selected() {
                        let text = match &r.alias {
                            Some(a) => a.dns_name.trim_end_matches('.').to_string(),
                            None => r.values.join("\n"),
                        };
                        copy(ctx, &text, if r.values.len() > 1 { "Values copied" } else { "Value copied" });
                    }
                }
                KeyCode::Char('o') => {
                    let url = route53::console_url(&self.zone_id());
                    if !crate::platform::open_url(&url) {
                        ctx.toast(Level::Warn, format!("Could not open a browser: {url}"));
                    }
                }
                KeyCode::Char('r') => self.load_records(ctx),
                _ => return Handled::No,
            }
            return Handled::Yes;
        }
        let selected = self.zone_state.selected().unwrap_or(0);
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => self.select_zone(selected.saturating_sub(1), ctx),
            KeyCode::Down | KeyCode::Char('j') => self.select_zone(selected + 1, ctx),
            KeyCode::PageUp => self.detail_scroll = self.detail_scroll.saturating_sub(10),
            KeyCode::PageDown => self.detail_scroll = self.detail_scroll.saturating_add(10),
            KeyCode::Char('/') => self.filtering = true,
            KeyCode::Esc if !self.zone_filter.value.is_empty() => {
                self.zone_filter.clear();
                self.refilter_zones();
            }
            KeyCode::Enter | KeyCode::Right => self.open_zone(ctx),
            KeyCode::Char('y') => {
                if let Some(id) = self.selected_zone().map(|z| z.id.clone()) {
                    copy(ctx, &id, "Zone id copied");
                }
            }
            KeyCode::Char('o') => {
                if let Some(z) = self.selected_zone() {
                    let url = route53::console_url(&z.id);
                    if !crate::platform::open_url(&url) {
                        ctx.toast(Level::Warn, format!("Could not open a browser: {url}"));
                    }
                }
            }
            KeyCode::Char('r') => {
                self.details.clear();
                self.load_zones(ctx);
            }
            _ => return Handled::No,
        }
        Handled::Yes
    }

    fn mouse(&mut self, event: MouseEvent, ctx: &mut Ctx) -> Handled {
        if self.dialog.is_some() || !self.list_area.contains((event.column, event.row).into()) {
            return Handled::No;
        }
        let in_records = self.open.is_some();
        let (selected, offset) = if in_records {
            (self.state.selected().unwrap_or(0), self.state.offset())
        } else {
            (self.zone_state.selected().unwrap_or(0), self.zone_state.offset())
        };
        let target = match event.kind {
            MouseEventKind::ScrollUp => selected.saturating_sub(1),
            MouseEventKind::ScrollDown => selected + 1,
            MouseEventKind::Down(MouseButton::Left) => offset + event.row.saturating_sub(self.list_area.y + 2) as usize,
            _ => return Handled::No,
        };
        if in_records { self.select(target) } else { self.select_zone(target, ctx) }
        Handled::Yes
    }

    fn task(&mut self, payload: Payload, ctx: &mut Ctx) {
        let Ok(msg) = payload.downcast::<Msg>() else { return };
        match *msg {
            Msg::Zones(Ok(zones)) => {
                self.zones = zones;
                self.zones_load = LoadState::Loaded;
                self.refilter_zones();
                self.load_detail(ctx);
                if let Some(target) = self.pending_open.take() {
                    self.open(&target, ctx);
                }
                // A reload keeps the open zone's numbers current.
                if let Some(open) = &mut self.open
                    && let Some(z) = self.zones.iter().find(|z| z.id == open.id)
                {
                    *open = z.clone();
                }
            }
            Msg::Zones(Err(e)) => self.zones_load = LoadState::Failed(e),
            Msg::Detail { zone, result } => {
                self.details_in_flight.remove(&zone);
                self.details.insert(zone, result);
            }
            Msg::Records { zone, page } if self.zone_id() == zone => {
                self.records.extend(page);
                self.refilter();
                if let Some(pos) = self.reselect.as_ref().and_then(|k| self.visible.iter().position(|&i| self.records[i].key() == *k)) {
                    self.reselect = None;
                    self.select(pos);
                }
            }
            Msg::RecordsDone { zone, result } if self.zone_id() == zone => {
                self.reselect = None;
                self.records_load = match result {
                    Ok(()) => LoadState::Loaded,
                    Err(e) => LoadState::Failed(e),
                };
            }
            Msg::Candidates { zone, list, failed } if self.zone_id() == zone => {
                self.candidates_loading = false;
                self.candidates = Some((list, failed));
                // The list may be open, waiting for them.
                let edit = match &self.dialog {
                    Some(Dialog::Target { edit, .. }) => Some(edit.clone()),
                    _ => None,
                };
                if let Some(edit) = edit {
                    let (items, picks) = self.target_items(edit.as_deref());
                    if let Some(Dialog::Target { picker, picks: old, .. }) = &mut self.dialog {
                        picker.set_items(items);
                        *old = picks;
                    }
                }
            }
            Msg::Saved { zone, key, what, created, result } => match result {
                Ok(change) => {
                    self.dialog = None;
                    ctx.toast(Level::Success, format!("{} {what} · syncing to Route 53's name servers", if created { "Created" } else { "Changed" }));
                    let now = Instant::now();
                    self.syncing.push(Sync { id: change.id, what, since: now, checked: now, in_flight: false });
                    self.candidates = None;
                    if self.zone_id() == zone {
                        self.reselect = Some(key);
                        self.load_records(ctx);
                        self.load_zones(ctx);
                    }
                }
                Err(e) => {
                    if let Some(Dialog::Form(editing)) = &mut self.dialog {
                        editing.busy = false;
                        editing.form.fail(refusal(&e));
                    } else {
                        ctx.toast(Level::Error, refusal(&e));
                    }
                }
            },
            Msg::Synced { id, result } => {
                let Some(pos) = self.syncing.iter().position(|s| s.id == id) else { return };
                self.syncing[pos].in_flight = false;
                match result {
                    Ok(c) if c.in_sync() => {
                        let s = self.syncing.remove(pos);
                        ctx.notify(Level::Success, "Route 53", &format!("{} answers on every Route 53 name server", s.what));
                    }
                    Ok(_) if self.syncing[pos].since.elapsed() > SYNC_FOR => {
                        let s = self.syncing.remove(pos);
                        ctx.toast(Level::Warn, format!("{} still syncing after 5 minutes; Route 53 keeps at it", s.what));
                    }
                    Ok(_) => {}
                    Err(e) => {
                        let s = self.syncing.remove(pos);
                        ctx.toast(Level::Warn, format!("Could not follow {}: {}", s.what, e.short()));
                    }
                }
            }
            _ => {}
        }
    }

    fn tick(&mut self, ctx: &mut Ctx, _active: bool) {
        if !self.syncing.is_empty() {
            self.poll_changes(ctx);
        }
    }

    fn render(&mut self, f: &mut Frame, area: Rect, view: &View) {
        if self.open.is_some() {
            self.render_records(f, area, view);
        } else {
            self.render_zones(f, area, view);
        }
        let color = if is_production(&self.env_name) { theme::ERR } else { theme::ACCENT };
        match &mut self.dialog {
            Some(Dialog::Kind(picker)) | Some(Dialog::Target { picker, .. }) => {
                let rect = widgets::popup_area(area, 96, picker.height(area.height.saturating_sub(4)));
                picker.render(f, rect, color);
            }
            Some(Dialog::Form(editing)) => editing.form.render(f, area, color),
            None => {}
        }
    }

    fn hints(&self) -> Vec<(&'static str, &'static str)> {
        match &self.dialog {
            Some(Dialog::Form(_)) => return vec![("Tab", "next field"), ("Enter", "save"), ("Esc", "cancel")],
            Some(_) => return vec![("type", "filter"), ("↑↓", "move"), ("Enter", "choose"), ("Esc", "cancel")],
            None => {}
        }
        if self.filtering {
            return vec![("Enter", "keep filter"), ("Esc", "clear"), ("↑↓", "move")];
        }
        if self.open.is_some() {
            return vec![
                ("↑↓", "move"),
                ("/", "search"),
                ("n", "new record"),
                ("Enter", "edit"),
                ("y", "copy name"),
                ("c", "copy value"),
                ("o", "console"),
                ("r", "reload"),
                ("Esc", "zones"),
            ];
        }
        vec![("↑↓", "move"), ("Enter", "records"), ("/", "search"), ("y", "copy id"), ("o", "console"), ("r", "reload")]
    }

    fn animating(&self) -> bool {
        matches!(self.zones_load, LoadState::Loading)
            || matches!(self.records_load, LoadState::Loading)
            || !self.syncing.is_empty()
            || !self.details_in_flight.is_empty()
            || matches!(&self.dialog, Some(Dialog::Form(e)) if e.busy)
            || matches!(self.dialog, Some(Dialog::Target { .. })) && self.candidates_loading
    }

    fn capturing(&self) -> bool {
        self.filtering || self.dialog.is_some()
    }

    fn paste(&mut self, text: &str, _ctx: &mut Ctx) {
        match &mut self.dialog {
            Some(Dialog::Form(editing)) => {
                // Pasted lines become separate values.
                let text = if editing.form.focused_label() == Some("Values") {
                    route53::split_values(text).join(", ")
                } else {
                    text.replace(['\r', '\n'], " ")
                };
                editing.form.paste(&text);
                let zone = self.open.as_ref().map(|z| z.name.clone()).unwrap_or_default();
                editing.refresh(&zone);
            }
            Some(Dialog::Kind(picker)) | Some(Dialog::Target { picker, .. }) => picker.paste(text),
            None if self.filtering => {
                if self.open.is_some() {
                    self.filter.insert_str(text, false);
                    self.refilter();
                } else {
                    self.zone_filter.insert_str(text, false);
                    self.refilter_zones();
                }
            }
            None => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn zone() -> Zone {
        Zone { id: "Z1".into(), name: "acme.dev.".into(), ..Zone::default() }
    }

    fn editing(fields: Vec<Field>, old: Option<Record>, kind: Option<&str>, mode: Mode) -> Editing {
        Editing { form: Form::new("t", fields), old, kind: kind.map(str::to_string), mode, busy: false }
    }

    #[test]
    fn a_new_record_is_checked_before_it_is_sent() {
        let records = vec![Record::sample("www.acme.dev.", "A", 300, &["192.0.2.1"])];
        let new = |name: &str, values: &str| {
            editing(
                vec![Field::text("Name", name), Field::fixed("Type", "A"), Field::text("TTL", "60"), Field::text("Values", values)],
                None,
                Some("A"),
                Mode::Values,
            )
        };
        let (name, kind, answer) = prepare(&new("api", "192.0.2.7, 192.0.2.8"), &zone(), &records).unwrap();
        assert_eq!((name.as_str(), kind.as_str()), ("api.acme.dev.", "A"));
        assert_eq!(answer, Answer::Values { ttl: 60, values: vec!["192.0.2.7".into(), "192.0.2.8".into()] });
        assert!(prepare(&new("www", "192.0.2.9"), &zone(), &records).unwrap_err().contains("already has"));
        assert!(prepare(&new("api", "nope"), &zone(), &records).unwrap_err().contains("not an IPv4"));
    }

    #[test]
    fn an_edit_must_change_something() {
        let old = Record::sample("www.acme.dev.", "TXT", 300, &["\"hello\""]);
        let edit = |ttl: &str, values: &str| {
            editing(
                vec![Field::fixed("Record", "www.acme.dev."), Field::text("TTL", ttl), Field::text("Values", values)],
                Some(old.clone()),
                Some("TXT"),
                Mode::Values,
            )
        };
        assert_eq!(prepare(&edit("300", "hello"), &zone(), &[]).unwrap_err(), "Nothing changed", "quotes are added as stored");
        let (_, _, answer) = prepare(&edit("60", "\"hello\""), &zone(), &[]).unwrap();
        assert_eq!(answer, Answer::Values { ttl: 60, values: vec!["\"hello\"".into()] });
    }

    #[test]
    fn an_alias_takes_a_type_its_target_answers() {
        let target = Candidate {
            source: "CloudFront",
            label: "d1.cloudfront.net".into(),
            detail: String::new(),
            alias: Alias { zone_id: route53::CLOUDFRONT_ZONE.into(), dns_name: "d1.cloudfront.net.".into(), evaluate_health: false },
            kinds: vec!["A".into()],
            serves: vec!["www.acme.dev".into()],
        };
        let form = |name: &str, kind: Option<&str>| {
            editing(
                vec![Field::text("Name", name), Field::choice("Type", &["A", "AAAA"], "AAAA"), Field::choice("Evaluate health", &["no", "yes"], "yes")],
                None,
                kind,
                Mode::Alias(target.clone()),
            )
        };
        assert!(prepare(&form("www", None), &zone(), &[]).unwrap_err().contains("not AAAA"));
        let (_, kind, answer) = prepare(&form("www", Some("A")), &zone(), &[]).unwrap();
        assert_eq!(kind, "A");
        assert!(matches!(answer, Answer::Alias(a) if a.evaluate_health && a.zone_id == route53::CLOUDFRONT_ZONE));
        let mut e = form("api", Some("A"));
        e.refresh("acme.dev.");
        assert!(e.form.note.unwrap().contains("CloudFront answers 403"));
    }
}
