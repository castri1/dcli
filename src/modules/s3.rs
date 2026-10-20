//! S3 explorer: buckets, folders (prefixes) and objects with their metadata; text
//! previews, presigned download links and downloads to ~/Downloads. Read-only.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Style, Stylize};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{List, ListItem, ListState, Paragraph, Wrap};

use super::common::{LoadState, filter_bar, matches_filter};
use crate::aws::{self, s3};
use crate::tui::logview::{LogKey, LogView};
use crate::tui::module::{Ctx, Emitter, Handled, Level, Module, Payload, View};
use crate::tui::picker::{Item, Outcome, Picker};
use crate::tui::theme;
use crate::tui::widgets::{self, InputEvent, TextInput};
use crate::util::{display_path, ellipsize, fmt_ago, fmt_bytes, fmt_duration, fmt_utc, now_secs, pretty_json};

const DEBOUNCE: Duration = Duration::from_millis(200);
/// Entries read at most per folder.
const MAX_ENTRIES: usize = 5000;
/// Link lifetimes offered by `u`: (label, seconds).
const LINK_TIMES: [(&str, u64); 3] = [("15 minutes", 900), ("1 hour", 3600), ("12 hours", 43_200)];

type ObjectKey = (String, String);

enum Msg {
    Buckets(aws::Result<Vec<s3::Bucket>>),
    Region { seq: u64, region: String },
    Page { seq: u64, page: s3::Page },
    Listed { seq: u64, result: aws::Result<bool> },
    Info { object: ObjectKey, result: aws::Result<s3::ObjectInfo> },
    Preview { object: ObjectKey, result: aws::Result<s3::Preview> },
    Link { name: String, asked: u64, result: aws::Result<(String, Option<f64>)> },
    Downloaded { name: String, result: aws::Result<(PathBuf, u64)> },
}

enum Entry {
    Up,
    Bucket(usize),
    Folder(String),
    Object(usize),
}

/// The bucket and folder being browsed.
struct Place {
    bucket: String,
    region: Option<String>,
    prefix: String,
}

struct PreviewPane {
    object: ObjectKey,
    view: LogView,
    loading: bool,
    note: String,
}

pub struct S3 {
    buckets: Vec<s3::Bucket>,
    buckets_load: LoadState,
    place: Option<Place>,
    folders: Vec<String>,
    objects: Vec<s3::Object>,
    listing: LoadState,
    /// The folder has more entries than [`MAX_ENTRIES`].
    capped: bool,
    seq: u64,
    entries: Vec<Entry>,
    list_state: ListState,
    filter: TextInput,
    filtering: bool,
    info: HashMap<ObjectKey, aws::Result<s3::ObjectInfo>>,
    info_in_flight: HashSet<ObjectKey>,
    pending: Option<(ObjectKey, Instant)>,
    preview: Option<PreviewPane>,
    link: Option<(Picker, ObjectKey)>,
    /// A download or link in progress, for the detail panel.
    busy: Option<String>,
    /// Folder to select once its parent lists (going up).
    reselect: Option<String>,
    list_area: Rect,
}

impl S3 {
    pub fn new() -> S3 {
        S3 {
            buckets: Vec::new(),
            buckets_load: LoadState::Idle,
            place: None,
            folders: Vec::new(),
            objects: Vec::new(),
            listing: LoadState::Idle,
            capped: false,
            seq: 0,
            entries: Vec::new(),
            list_state: ListState::default(),
            filter: TextInput::default(),
            filtering: false,
            info: HashMap::new(),
            info_in_flight: HashSet::new(),
            pending: None,
            preview: None,
            link: None,
            busy: None,
            reselect: None,
            list_area: Rect::default(),
        }
    }

    fn load_buckets(&mut self, ctx: &mut Ctx) {
        self.buckets_load = LoadState::Loading;
        let aws = ctx.env().aws.clone();
        ctx.spawn(move || Msg::Buckets(s3::list_buckets(&aws)));
    }

    /// Lists the current folder, page by page, after finding the bucket's region.
    fn list(&mut self, ctx: &mut Ctx) {
        let Some((bucket, prefix)) = self.place.as_ref().map(|p| (p.bucket.clone(), p.prefix.clone())) else { return };
        self.seq += 1;
        // Entries index into the lists: drop them first.
        self.entries.clear();
        self.folders.clear();
        self.objects.clear();
        self.capped = false;
        self.listing = LoadState::Loading;
        self.rebuild();
        let (seq, aws) = (self.seq, ctx.env().aws.clone());
        ctx.spawn_stream(move |em: Emitter<Msg>| {
            let regional = match s3::bucket_region(&aws, &bucket) {
                Ok(region) => {
                    em.emit(Msg::Region { seq, region: region.clone() });
                    aws.in_region(&region)
                }
                Err(e) => {
                    em.emit(Msg::Listed { seq, result: Err(e) });
                    return;
                }
            };
            let (mut token, mut read) = (None::<String>, 0);
            loop {
                match s3::list_objects(&regional, &bucket, &prefix, token.as_deref()) {
                    Ok(page) => {
                        read += page.folders.len() + page.objects.len();
                        let next = page.next.clone();
                        if !em.emit(Msg::Page { seq, page }) {
                            return;
                        }
                        match next {
                            Some(_) if read >= MAX_ENTRIES => {
                                em.emit(Msg::Listed { seq, result: Ok(true) });
                                return;
                            }
                            Some(t) => token = Some(t),
                            None => break,
                        }
                    }
                    Err(e) => {
                        em.emit(Msg::Listed { seq, result: Err(e) });
                        return;
                    }
                }
            }
            em.emit(Msg::Listed { seq, result: Ok(false) });
        });
    }

    /// Relative name shown for a key or prefix inside the current folder.
    fn label<'a>(&self, full: &'a str) -> &'a str {
        let prefix = self.place.as_ref().map_or("", |p| p.prefix.as_str());
        full.strip_prefix(prefix).unwrap_or(full)
    }

    fn rebuild(&mut self) {
        let keep = self.selected_name();
        let query = self.filter.value.trim().to_lowercase();
        self.entries.clear();
        match &self.place {
            None => {
                let mut hits: Vec<usize> =
                    (0..self.buckets.len()).filter(|&i| matches_filter(&self.buckets[i].name, &query)).collect();
                hits.sort_by(|&a, &b| self.buckets[a].name.cmp(&self.buckets[b].name));
                self.entries.extend(hits.into_iter().map(Entry::Bucket));
            }
            Some(_) => {
                self.entries.push(Entry::Up);
                let folders: Vec<String> =
                    self.folders.iter().filter(|f| matches_filter(self.label(f), &query)).cloned().collect();
                self.entries.extend(folders.into_iter().map(Entry::Folder));
                let objects: Vec<usize> =
                    (0..self.objects.len()).filter(|&i| matches_filter(self.label(&self.objects[i].key), &query)).collect();
                self.entries.extend(objects.into_iter().map(Entry::Object));
            }
        }
        let want = self.reselect.clone().or(keep);
        let index = want.and_then(|w| (0..self.entries.len()).find(|&i| self.entry_name(i).as_deref() == Some(w.as_str())));
        if index.is_some() {
            self.reselect = None;
        }
        let fallback = if self.place.is_some() && self.entries.len() > 1 { 1 } else { 0 };
        self.list_state.select(if self.entries.is_empty() { None } else { Some(index.unwrap_or(fallback)) });
    }

    fn entry_name(&self, i: usize) -> Option<String> {
        match self.entries.get(i)? {
            Entry::Up => None,
            Entry::Bucket(b) => Some(self.buckets.get(*b)?.name.clone()),
            Entry::Folder(f) => Some(f.clone()),
            Entry::Object(o) => Some(self.objects.get(*o)?.key.clone()),
        }
    }

    fn selected_name(&self) -> Option<String> {
        self.entry_name(self.list_state.selected()?)
    }

    fn selected_object(&self) -> Option<(&Place, &s3::Object)> {
        match self.entries.get(self.list_state.selected()?)? {
            Entry::Object(i) => Some((self.place.as_ref()?, self.objects.get(*i)?)),
            _ => None,
        }
    }

    fn select(&mut self, index: usize) {
        if self.entries.is_empty() {
            return;
        }
        self.list_state.select(Some(index.min(self.entries.len() - 1)));
        if let Some((place, o)) = self.selected_object() {
            let object = (place.bucket.clone(), o.key.clone());
            if !self.info.contains_key(&object) {
                self.pending = Some((object, Instant::now()));
            }
        }
    }

    fn enter(&mut self, ctx: &mut Ctx) {
        let Some(i) = self.list_state.selected() else { return };
        match self.entries.get(i) {
            Some(Entry::Up) => self.up(ctx),
            Some(Entry::Bucket(b)) => {
                let bucket = &self.buckets[*b];
                self.place = Some(Place { bucket: bucket.name.clone(), region: bucket.region.clone(), prefix: String::new() });
                self.filter.clear();
                self.list(ctx);
            }
            Some(Entry::Folder(f)) => {
                let folder = f.clone();
                if let Some(place) = &mut self.place {
                    place.prefix = folder;
                }
                self.filter.clear();
                self.list(ctx);
            }
            Some(Entry::Object(_)) => self.open_preview(ctx),
            None => {}
        }
    }

    fn up(&mut self, ctx: &mut Ctx) {
        let Some(place) = &mut self.place else { return };
        self.filter.clear();
        if place.prefix.is_empty() {
            self.reselect = Some(place.bucket.clone());
            self.place = None;
            self.seq += 1;
            self.listing = LoadState::Idle;
            self.rebuild();
            return;
        }
        let left = place.prefix.clone();
        let trimmed = &left[..left.len() - 1];
        place.prefix = trimmed.rfind('/').map_or(String::new(), |i| trimmed[..=i].to_string());
        self.reselect = Some(left);
        self.list(ctx);
    }

    fn fetch_info(&mut self, object: ObjectKey, ctx: &mut Ctx) {
        if !self.info_in_flight.insert(object.clone()) {
            return;
        }
        let aws = ctx.env().aws.clone();
        ctx.spawn(move || {
            let result = s3::regional(&aws, &object.0).and_then(|r| s3::head_object(&r, &object.0, &object.1));
            Msg::Info { object, result }
        });
    }

    fn open_preview(&mut self, ctx: &mut Ctx) {
        let Some((place, o)) = self.selected_object() else { return };
        let object = (place.bucket.clone(), o.key.clone());
        let mut view = LogView::default();
        view.zoomed = true;
        self.preview = Some(PreviewPane { object: object.clone(), view, loading: true, note: String::new() });
        let aws = ctx.env().aws.clone();
        ctx.spawn(move || {
            let result = s3::regional(&aws, &object.0).and_then(|r| s3::preview(&r, &object.0, &object.1));
            Msg::Preview { object, result }
        });
    }

    fn on_preview(&mut self, object: ObjectKey, result: aws::Result<s3::Preview>) {
        let Some(pane) = self.preview.as_mut().filter(|p| p.object == object) else { return };
        pane.loading = false;
        match result {
            Ok(preview) => match s3::preview_text(&preview) {
                Some(text) => {
                    let text = if preview.truncated { text } else { pretty_json(&text).unwrap_or(text) };
                    pane.view.extend(text.lines().map(str::to_string));
                    pane.view.scroll(-(pane.view.lines().len() as isize));
                    if preview.truncated {
                        pane.note = format!("first {} · d downloads it all", fmt_bytes(s3::PREVIEW_BYTES as i64));
                    }
                }
                None => {
                    pane.view.extend(["Binary file: not previewed. d downloads it.".to_string()]);
                }
            },
            Err(e) => pane.view.extend([format!("Could not read it: {}", e.short())]),
        }
    }

    fn open_link(&mut self) {
        let Some((place, o)) = self.selected_object() else { return };
        let object = (place.bucket.clone(), o.key.clone());
        let items = LINK_TIMES.iter().map(|(label, _)| Item::new(*label, "")).collect();
        let picker = Picker::new(format!("Download link · {}", s3::file_name(&o.key)), "create", items, false).with_cursor(1);
        self.link = Some((picker, object));
    }

    fn create_link(&mut self, object: ObjectKey, seconds: u64, ctx: &mut Ctx) {
        let name = s3::file_name(&object.1);
        self.busy = Some(format!("Creating a link for {name}…"));
        let aws = ctx.env().aws.clone();
        ctx.spawn(move || {
            let result = s3::regional(&aws, &object.0).and_then(|r| {
                let url = s3::presigned_url(&r, &object.0, &object.1, seconds)?;
                Ok((url, r.credentials_expiry()))
            });
            Msg::Link { name, asked: seconds, result }
        });
    }

    fn download(&mut self, ctx: &mut Ctx) {
        let Some((place, o)) = self.selected_object() else { return };
        if self.busy.is_some() {
            return ctx.toast(Level::Info, "Wait for the current download to finish");
        }
        let (bucket, key) = (place.bucket.clone(), o.key.clone());
        let name = s3::file_name(&key);
        self.busy = Some(format!("Downloading {name} ({})…", fmt_bytes(o.size)));
        let aws = ctx.env().aws.clone();
        ctx.spawn(move || {
            let result = s3::regional(&aws, &bucket).and_then(|r| {
                let path = s3::free_path(&s3::downloads_dir(), &name);
                let bytes = s3::download(&r, &bucket, &key, &path)?;
                Ok((path, bytes))
            });
            Msg::Downloaded { name, result }
        });
    }

    /// `s3://bucket/key` of the selection (bucket, folder or object).
    fn selected_uri(&self) -> Option<String> {
        match self.entries.get(self.list_state.selected()?)? {
            Entry::Up => None,
            Entry::Bucket(b) => Some(s3::uri(&self.buckets[*b].name, "")),
            Entry::Folder(f) => Some(s3::uri(&self.place.as_ref()?.bucket, f)),
            Entry::Object(o) => Some(s3::uri(&self.place.as_ref()?.bucket, &self.objects[*o].key)),
        }
    }

    fn open_console(&self, ctx: &mut Ctx) {
        let region = |bucket: &str| {
            self.buckets.iter().find(|b| b.name == bucket).and_then(|b| b.region.clone()).unwrap_or_else(|| ctx.env().region.clone())
        };
        let url = match self.entries.get(self.list_state.selected().unwrap_or(0)) {
            Some(Entry::Bucket(b)) => s3::console_url(&region(&self.buckets[*b].name), &self.buckets[*b].name, "", false),
            Some(entry) => {
                let Some(place) = &self.place else { return };
                let region = place.region.clone().unwrap_or_else(|| region(&place.bucket));
                match entry {
                    Entry::Object(o) => s3::console_url(&region, &place.bucket, &self.objects[*o].key, true),
                    Entry::Folder(f) => s3::console_url(&region, &place.bucket, f, false),
                    _ => s3::console_url(&region, &place.bucket, &place.prefix, false),
                }
            }
            None => return,
        };
        if !crate::platform::open_url(&url) {
            ctx.toast(Level::Warn, format!("Could not open a browser: {url}"));
        }
    }

    fn render_list(&mut self, f: &mut Frame, area: Rect, view: &View) {
        self.list_area = area;
        let [bar, list_area] = area.layout(&Layout::vertical([Constraint::Length(1), Constraint::Fill(1)]));
        let placeholder = if self.place.is_some() { "type / to filter this folder" } else { "type / to filter buckets" };
        let (bar_line, cursor) = filter_bar(&self.filter, self.filtering, placeholder);
        f.render_widget(Paragraph::new(bar_line), bar);
        if let Some(col) = cursor {
            f.set_cursor_position((bar.x + col, bar.y));
        }
        let (load, here) = match &self.place {
            None => (&self.buckets_load, "Buckets".to_string()),
            Some(p) => (&self.listing, format!("{}/{}", p.bucket, p.prefix)),
        };
        let count = self.entries.iter().filter(|e| !matches!(e, Entry::Up)).count();
        let title = match load {
            LoadState::Loading => format!(" {} {} · {count} ", theme::spinner(view.tick), ellipsize(&here, 50)),
            _ if self.capped => format!(" {} · first {MAX_ENTRIES} ", ellipsize(&here, 50)),
            _ => format!(" {} · {count} ", ellipsize(&here, 50)),
        };
        let only_up = self.place.is_some() && self.entries.len() == 1;
        if self.entries.is_empty() || only_up {
            let text = match load {
                LoadState::Failed(e) => Line::styled(e.short(), Style::new().fg(theme::ERR)),
                LoadState::Loading => Line::styled("Loading…", theme::muted()),
                _ if !self.filter.value.is_empty() => Line::styled("No matches · Esc clears the filter", theme::muted()),
                _ if self.place.is_some() => Line::styled("Empty folder · ← goes up", theme::muted()),
                _ => Line::styled("No buckets", theme::muted()),
            };
            f.render_widget(widgets::placeholder(text).block(widgets::panel(title, true)), list_area);
            return;
        }
        let width = list_area.width.saturating_sub(4) as usize;
        let items: Vec<ListItem> = self
            .entries
            .iter()
            .map(|e| match e {
                Entry::Up => ListItem::new(Line::styled("‹ ..", theme::muted())),
                Entry::Bucket(i) => {
                    let b = &self.buckets[*i];
                    let region = b.region.clone().unwrap_or_default();
                    let name = ellipsize(&b.name, width.saturating_sub(region.len() + 3));
                    let pad = width.saturating_sub(name.chars().count() + region.len() + 2);
                    ListItem::new(Line::from(vec![
                        Span::styled("◫ ", theme::accent()),
                        Span::raw(name),
                        Span::raw(" ".repeat(pad)),
                        Span::styled(region, theme::muted()),
                    ]))
                }
                Entry::Folder(p) => ListItem::new(Line::from(vec![
                    Span::styled("▸ ", theme::accent()),
                    Span::styled(ellipsize(self.label(p), width.saturating_sub(2)), theme::accent().bold()),
                ])),
                Entry::Object(i) => {
                    let o = &self.objects[*i];
                    let right = format!("{:>9}  {:>7}", fmt_bytes(o.size), o.modified.map(fmt_ago).unwrap_or_default().replace(" ago", ""));
                    let name = ellipsize(self.label(&o.key), width.saturating_sub(right.chars().count() + 3));
                    let pad = width.saturating_sub(name.chars().count() + right.chars().count() + 2);
                    ListItem::new(Line::from(vec![
                        Span::raw("  "),
                        Span::raw(name),
                        Span::raw(" ".repeat(pad)),
                        Span::styled(right, theme::muted()),
                    ]))
                }
            })
            .collect();
        let list = List::new(items).block(widgets::panel(title, true)).highlight_style(theme::selected());
        f.render_stateful_widget(list, list_area, &mut self.list_state);
    }

    fn render_detail(&self, f: &mut Frame, area: Rect, view: &View) {
        let kw = 13;
        let mut lines = Vec::new();
        if let Some(busy) = &self.busy {
            lines.push(Line::from(vec![Span::styled(format!("{} ", theme::spinner(view.tick)), theme::accent()), Span::raw(busy.clone())]));
            lines.push(Line::raw(""));
        }
        let selected = self.list_state.selected().and_then(|i| self.entries.get(i));
        let title = match selected {
            Some(Entry::Bucket(i)) => {
                let b = &self.buckets[*i];
                lines.push(widgets::kv("Bucket", b.name.clone(), kw));
                lines.push(widgets::kv("Region", b.region.clone().unwrap_or_else(|| "found when opened".into()), kw));
                if let Some(t) = b.created {
                    lines.push(widgets::kv("Created", format!("{} · {}", fmt_ago(t), fmt_utc(t)), kw));
                }
                lines.push(Line::raw(""));
                lines.push(Line::styled("Enter opens it · y copies its s3:// URI · o console", theme::muted()));
                format!(" {} ", b.name)
            }
            Some(Entry::Folder(p)) => {
                let bucket = self.place.as_ref().map_or("", |pl| pl.bucket.as_str());
                lines.push(widgets::kv("Folder", s3::uri(bucket, p), kw));
                lines.push(Line::raw(""));
                lines.push(Line::styled("Enter opens it · y copies its URI · o console", theme::muted()));
                format!(" {} ", self.label(p))
            }
            Some(Entry::Object(_)) => {
                let Some((place, o)) = self.selected_object() else { return };
                lines.push(widgets::kv("URI", s3::uri(&place.bucket, &o.key), kw));
                lines.push(widgets::kv("Size", format!("{} ({} bytes)", fmt_bytes(o.size), o.size), kw));
                if let Some(t) = o.modified {
                    lines.push(widgets::kv("Modified", format!("{} · {}", fmt_ago(t), fmt_utc(t)), kw));
                }
                match self.info.get(&(place.bucket.clone(), o.key.clone())) {
                    Some(Ok(info)) => {
                        let opt = |v: &Option<String>| v.clone().unwrap_or_else(|| "—".into());
                        lines.push(widgets::kv("Type", opt(&info.content_type), kw));
                        lines.push(widgets::kv("Storage", opt(&info.storage_class), kw));
                        let encryption = match (&info.encryption, &info.kms_key) {
                            (Some(e), Some(k)) => format!("{e} · {k}"),
                            (Some(e), None) => e.clone(),
                            _ => "none".into(),
                        };
                        lines.push(widgets::kv("Encryption", encryption, kw));
                        lines.push(widgets::kv("ETag", opt(&info.etag), kw));
                        if let Some(v) = &info.version_id {
                            lines.push(widgets::kv("Version", v.clone(), kw));
                        }
                        if !info.metadata.is_empty() {
                            lines.push(Line::raw(""));
                            lines.push(widgets::section("Metadata"));
                            let mw = info.metadata.iter().map(|(k, _)| k.chars().count()).max().unwrap_or(0).min(28);
                            for (k, v) in &info.metadata {
                                lines.push(widgets::kv(k, v.clone(), mw));
                            }
                        }
                    }
                    Some(Err(e)) => lines.push(Line::styled(e.short(), Style::new().fg(theme::ERR))),
                    None => lines.push(Line::from(vec![
                        Span::styled(theme::spinner(view.tick), theme::accent()),
                        Span::styled(" reading metadata…", theme::muted()),
                    ])),
                }
                lines.push(Line::raw(""));
                lines.push(widgets::hints_line(&[
                    ("p", "preview"),
                    ("u", "download link"),
                    ("d", "download"),
                    ("y", "copy URI"),
                    ("o", "console"),
                ]));
                format!(" {} ", s3::file_name(&o.key))
            }
            Some(Entry::Up) => {
                lines.push(Line::styled("Enter or ← goes up", theme::muted()));
                " S3 ".into()
            }
            None => {
                lines.push(Line::styled("Select a bucket", theme::muted()));
                " S3 ".into()
            }
        };
        let block = widgets::panel(Line::from(title).bold(), false);
        f.render_widget(Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false }).block(block), area);
    }
}

impl Module for S3 {
    fn title(&self) -> &'static str {
        "S3"
    }

    fn description(&self) -> &'static str {
        "S3 · buckets, folders and objects, previews, presigned links, downloads"
    }

    fn reset(&mut self, _ctx: &mut Ctx) {
        *self = S3::new();
    }

    fn activate(&mut self, ctx: &mut Ctx) {
        if matches!(self.buckets_load, LoadState::Idle) {
            self.load_buckets(ctx);
        }
    }

    /// `s3://bucket/folder/` from another module.
    fn open(&mut self, target: &str, ctx: &mut Ctx) {
        let Some((bucket, key)) = s3::parse_uri(target) else { return };
        let prefix = match key.rfind('/') {
            Some(i) => key[..=i].to_string(),
            None => String::new(),
        };
        let region = self.buckets.iter().find(|b| b.name == bucket).and_then(|b| b.region.clone());
        self.place = Some(Place { bucket, region, prefix });
        self.filter.clear();
        if !key.is_empty() && !key.ends_with('/') {
            self.reselect = Some(key);
        }
        self.list(ctx);
    }

    fn key(&mut self, key: KeyEvent, ctx: &mut Ctx) -> Handled {
        if let Some(pane) = &mut self.preview {
            if let LogKey::Copy = pane.view.key(key) {
                let ok = crate::platform::copy_to_clipboard(&pane.view.lines().join("\n"));
                ctx.toast(if ok { Level::Success } else { Level::Warn }, if ok { "Preview copied" } else { "Clipboard not available" });
            }
            if !pane.view.zoomed {
                self.preview = None;
            }
            return Handled::Yes;
        }
        if let Some((picker, object)) = &mut self.link {
            match picker.handle(key) {
                Outcome::Pending => {}
                Outcome::Cancel => self.link = None,
                Outcome::Done(chosen) => {
                    let object = object.clone();
                    self.link = None;
                    let seconds = chosen.first().map_or(3600, |&i| LINK_TIMES[i].1);
                    self.create_link(object, seconds, ctx);
                }
            }
            return Handled::Yes;
        }
        let selected = self.list_state.selected().unwrap_or(0);
        if self.filtering {
            match self.filter.handle(key) {
                InputEvent::Changed => self.rebuild(),
                InputEvent::Submit => self.filtering = false,
                InputEvent::Cancel => {
                    self.filtering = false;
                    self.filter.clear();
                    self.rebuild();
                }
                InputEvent::Ignored => match key.code {
                    KeyCode::Up => self.select(selected.saturating_sub(1)),
                    KeyCode::Down => self.select(selected + 1),
                    _ => {}
                },
                InputEvent::Moved => {}
            }
            return Handled::Yes;
        }
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => self.select(selected.saturating_sub(1)),
            KeyCode::Down | KeyCode::Char('j') => self.select(selected + 1),
            KeyCode::PageUp => self.select(selected.saturating_sub(10)),
            KeyCode::PageDown => self.select(selected + 10),
            KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => self.enter(ctx),
            KeyCode::Left | KeyCode::Backspace | KeyCode::Char('h') => self.up(ctx),
            KeyCode::Char('/') => self.filtering = true,
            KeyCode::Esc if !self.filter.value.is_empty() => {
                self.filter.clear();
                self.rebuild();
            }
            KeyCode::Esc if self.place.is_some() => self.up(ctx),
            KeyCode::Char('p') => self.open_preview(ctx),
            KeyCode::Char('u') => self.open_link(),
            KeyCode::Char('d') => self.download(ctx),
            KeyCode::Char('y') => {
                if let Some(uri) = self.selected_uri() {
                    let ok = crate::platform::copy_to_clipboard(&uri);
                    ctx.toast(if ok { Level::Success } else { Level::Warn }, if ok { format!("Copied {uri}") } else { "Clipboard not available".into() });
                }
            }
            KeyCode::Char('o') => self.open_console(ctx),
            KeyCode::Char('r') => {
                if self.place.is_some() {
                    self.info.clear();
                    self.list(ctx);
                } else {
                    self.load_buckets(ctx);
                }
            }
            _ => return Handled::No,
        }
        Handled::Yes
    }

    fn mouse(&mut self, event: MouseEvent, ctx: &mut Ctx) -> Handled {
        if self.preview.is_some() || self.link.is_some() || !self.list_area.contains((event.column, event.row).into()) {
            return Handled::No;
        }
        let selected = self.list_state.selected().unwrap_or(0);
        match event.kind {
            MouseEventKind::ScrollUp => self.select(selected.saturating_sub(1)),
            MouseEventKind::ScrollDown => self.select(selected + 1),
            MouseEventKind::Down(MouseButton::Left) => {
                // Below the filter bar and the panel border.
                let row = event.row.saturating_sub(self.list_area.y + 2) as usize;
                let index = self.list_state.offset() + row;
                if index < self.entries.len() {
                    if self.list_state.selected() == Some(index) {
                        self.enter(ctx);
                    } else {
                        self.select(index);
                    }
                }
            }
            _ => return Handled::No,
        }
        Handled::Yes
    }

    fn task(&mut self, payload: Payload, ctx: &mut Ctx) {
        let Ok(msg) = payload.downcast::<Msg>() else { return };
        match *msg {
            Msg::Buckets(result) => match result {
                Ok(buckets) => {
                    self.buckets = buckets;
                    self.buckets_load = LoadState::Loaded;
                    if self.place.is_none() {
                        self.rebuild();
                    }
                }
                Err(e) => self.buckets_load = LoadState::Failed(e),
            },
            Msg::Region { seq, region } if seq == self.seq => {
                if let Some(place) = &mut self.place {
                    place.region = Some(region);
                }
            }
            Msg::Page { seq, page } if seq == self.seq => {
                let first = self.folders.is_empty() && self.objects.is_empty();
                self.folders.extend(page.folders);
                self.objects.extend(page.objects);
                self.rebuild();
                if first && self.reselect.is_none() {
                    let i = self.list_state.selected().unwrap_or(0);
                    self.select(i);
                }
            }
            Msg::Listed { seq, result } if seq == self.seq => match result {
                Ok(capped) => {
                    self.capped = capped;
                    self.listing = LoadState::Loaded;
                    self.reselect = None;
                    let i = self.list_state.selected().unwrap_or(0);
                    self.select(i);
                }
                Err(e) => self.listing = LoadState::Failed(e),
            },
            Msg::Region { .. } | Msg::Page { .. } | Msg::Listed { .. } => {}
            Msg::Info { object, result } => {
                self.info_in_flight.remove(&object);
                self.info.insert(object, result);
            }
            Msg::Preview { object, result } => self.on_preview(object, result),
            Msg::Link { name, asked, result } => {
                self.busy = None;
                match result {
                    Ok((url, session_ends)) => {
                        let now = now_secs();
                        let until = session_ends.map_or(now + asked as f64, |s| s.min(now + asked as f64));
                        let cut = until < now + asked as f64 - 1.0;
                        let when = format!("{} (in {})", fmt_utc(until), fmt_duration(until - now));
                        let text = if cut {
                            format!("Link for {name} copied. It works until {when}, when your AWS session ends.")
                        } else {
                            format!("Link for {name} copied. It works until {when}.")
                        };
                        if crate::platform::copy_to_clipboard(&url) {
                            ctx.toast(if cut { Level::Warn } else { Level::Success }, text);
                        } else {
                            ctx.toast(Level::Warn, format!("Clipboard not available. {url}"));
                        }
                    }
                    Err(e) => ctx.toast(Level::Error, format!("Could not create the link: {}", e.short())),
                }
            }
            Msg::Downloaded { name, result } => {
                self.busy = None;
                match result {
                    Ok((path, bytes)) => {
                        ctx.toast(Level::Success, format!("Downloaded {name} ({}) → {}", fmt_bytes(bytes as i64), display_path(&path)))
                    }
                    Err(e) => ctx.toast(Level::Error, format!("Could not download {name}: {}", e.short())),
                }
            }
        }
    }

    fn tick(&mut self, ctx: &mut Ctx, _active: bool) {
        if let Some((object, at)) = &self.pending
            && at.elapsed() >= DEBOUNCE
        {
            let object = object.clone();
            self.pending = None;
            let still = self.selected_object().is_some_and(|(p, o)| p.bucket == object.0 && o.key == object.1);
            if still && !self.info.contains_key(&object) {
                self.fetch_info(object, ctx);
            }
        }
    }

    fn render(&mut self, f: &mut Frame, area: Rect, view: &View) {
        if let Some(pane) = &mut self.preview {
            if pane.loading {
                let text = Line::from(vec![
                    Span::styled(format!("{} ", theme::spinner(view.tick)), theme::accent()),
                    Span::raw(format!("Reading {}…", s3::uri(&pane.object.0, &pane.object.1))),
                ]);
                f.render_widget(widgets::placeholder(text).block(widgets::panel(" Preview ", true)), area);
                return;
            }
            let mut title = s3::uri(&pane.object.0, &pane.object.1);
            if !pane.note.is_empty() {
                title.push_str(&format!(" · {}", pane.note));
            }
            pane.view.render_zoomed(f, area, &title);
            return;
        }
        let [left, right] = if area.width < 90 {
            area.layout(&Layout::vertical([Constraint::Percentage(50), Constraint::Fill(1)]))
        } else {
            area.layout(&Layout::horizontal([Constraint::Percentage(50), Constraint::Fill(1)]))
        };
        self.render_list(f, left, view);
        self.render_detail(f, right, view);
        if let Some((picker, _)) = &mut self.link {
            let rect = widgets::popup_area(area, 56, picker.height(area.height.saturating_sub(4)));
            picker.render(f, rect, theme::ACCENT);
        }
    }

    fn hints(&self) -> Vec<(&'static str, &'static str)> {
        if self.preview.is_some() {
            return vec![("/", "search"), ("y", "copy all"), ("Esc", "back")];
        }
        if self.link.is_some() {
            return vec![("↑↓", "how long"), ("Enter", "create"), ("Esc", "cancel")];
        }
        if self.filtering {
            return vec![("Enter", "keep filter"), ("Esc", "clear"), ("↑↓", "move")];
        }
        if self.place.is_none() {
            return vec![("↑↓", "move"), ("Enter", "open"), ("/", "filter"), ("y", "copy URI"), ("o", "console"), ("r", "reload")];
        }
        vec![
            ("Enter/←", "open/up"),
            ("/", "filter"),
            ("p", "preview"),
            ("u", "link"),
            ("d", "download"),
            ("y", "copy URI"),
            ("o", "console"),
            ("r", "reload"),
        ]
    }

    fn animating(&self) -> bool {
        matches!(self.buckets_load, LoadState::Loading)
            || matches!(self.listing, LoadState::Loading)
            || self.busy.is_some()
            || self.pending.is_some()
            || !self.info_in_flight.is_empty()
            || self.preview.as_ref().is_some_and(|p| p.loading)
    }

    fn capturing(&self) -> bool {
        self.filtering || self.preview.is_some() || self.link.is_some()
    }

    fn wants_mouse(&self) -> bool {
        self.preview.is_none()
    }

    fn paste(&mut self, text: &str, _ctx: &mut Ctx) {
        if let Some((picker, _)) = &mut self.link {
            picker.paste(text);
        } else if self.filtering {
            self.filter.insert_str(text, false);
            self.rebuild();
        }
    }
}
