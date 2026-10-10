//! cce-calendar — month-view calendar with per-day events.
//!
//! A 6×7 month grid on the left, a day pane (a well in the root plate) on
//! the right. Events live in
//! `$XDG_DATA_HOME/cce/calendar/events.json` (one flat list of
//! date/time/title records) and are saved on every mutation. Records with a
//! `source` are mirrored from a remote calendar by `cce-calendar-sync`
//! (a dot coloured per account; local events are amber). Events typed here
//! are pushed to the default calendar on the next sync tick and come back
//! carrying their identity; deleting a synced
//! event here deletes it on the server — except instances of a recurring
//! event, which are read-only mirrors (the app refuses). The file is
//! re-read once a second when the sync rewrites it.
//!
//! Keys: arrows move the selected day · PageUp/PageDown month · [/] year ·
//! t/Home today · n/Enter new event (a leading `HH:MM` token sets the
//! time) · d/Delete remove the clicked event · q quit. The wheel flips
//! months over the grid and scrolls the event list over the pane.
//!
//! Config (`~/.config/cce/cce-calendar/config.kdl`): `week-start "sunday"`
//! (default monday); `push-to "icloud"` / `"google"` / `"none"`, optionally
//! `calendar="Name"`, picks where typed events are created (the sync's
//! default is iCloud when such an account exists);
//! `account-color "me@gmail.com" "#6ad08c"` pins an account's dot (a bare
//! email, or `"google:me@gmail.com"` to tell one address's two accounts
//! apart), otherwise accounts take a fixed palette in turn.
//!
//! The header's Accounts menu shows or hides each synced account's events
//! (`hide-account "<kind>:<email>"` in the config — display only: they stay
//! in events.json and keep syncing) and picks the account typed events go
//! to (it rewrites `push-to`).
//!
//! Dated tasks from the notes vault (any `- [ ] … 📅 2026-10-20` checkbox,
//! or `due 2026-10-20`, in any note — cce-list's lists included) show on
//! their day as a ring, and in the day pane as checkbox rows below the
//! events; a click ticks or unticks one in its note. They are read from
//! the vault and never enter events.json, where the sync would push them
//! to a calendar as events.
//!
//! Hovering a day-pane row shows a calendar glyph at its right end; it opens
//! a date picker that moves the row to another day — a task's date in its
//! note, an event's in events.json (which the sync then moves upstream). A
//! repeating event's instance cannot be moved, as it cannot be deleted.
//! The same moves work by drag and drop: an event or task, from its grid
//! chip or its day-pane row, dropped on another day (Escape cancels). A
//! task row ticks on release, so a press that becomes a drag does not.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use cce_calendar::{
    calendar_accounts, edit_config, load_records, push_target_config, save_push_target, save_records,
    CalendarAccount, EventRecord, PushTarget,
};
use chrono::{Datelike, Days, Local, NaiveDate, Weekday};

use cce_ui::engine::{Application, LogicalPosition, LogicalSize, WindowSettings};
use cce_ui::colors::{
    button_background_color, button_hover_color, button_press_color, control_label_color_u8,
    highlight_primary_color, list_font_color, textbox_background_color, textbox_placeholder_text_color,
    well_frame_color,
};
use cce_ui::layout::{
    align_text_y, bevel_width, button_corner_radius, button_font, button_height, carve_inside,
    control_label_font_detached, control_label_font_detached_parsed, control_relief, list_font,
    list_font_parsed, parse_font_string, plate_corner_radius, statusbar_font, statusbar_font_parsed,
    textbox_corner_radius, textbox_height, CONTROL_TEXT_INSET, plate_gap, plate_padding, root_plate_gap, root_plate_inset,
};
use cce_ui::scene::layout::Rect;
use cce_ui::scene::paint::{AlignH, AlignV, ControlPlate, DisplayList, PaintCtx, PlateStance, TextAttrs, TextLayout};
use cce_ui::scene::Material;
use cce_ui::widget::scroll_motion::{current_scroll_phase, Bounds, ScrollMotion, ScrollPhase};
use cce_ui::context::UiContext;
use cce_ui::widget::context_menu::{self, MARK_CHECK, MARK_OFF, MARK_ON};
use cce_ui::widget::Event as WidgetEvent;
use cce_ui::widget::input::date_picker::{DatePicker, Outcome as PickOutcome};
use cce_ui::widget::{
    Adapted, Button, Checkbox, Dropdown, ElementState, Handle, Key, KeyEvent, MouseButton, MouseScrollDelta, NamedKey, Paint,
    WidgetHost,
};

/// How often events.json is re-stat'd. The runner wakes an idle app once a
/// second anyway; `idle_poll_interval` pins that rather than inheriting it.
const WATCH_EVERY: std::time::Duration = std::time::Duration::from_secs(1);

const HEADER_H: f32 = 46.0;
const WEEKDAY_H: f32 = 24.0;
const SIDEBAR_W: f32 = 300.0;
const ROW_H: f32 = 36.0;
const INPUT_H: f32 = 40.0;
/// The sidebar's heading block: the day's name on one line, "today" under
/// it. Two text lines, so a size, not a spacing.
const SIDEBAR_HEAD_H: f32 = 40.0;
/// Trackpad travel per month step over the grid (a wheel notch is one step).
const MONTH_STEP_PX: f32 = 48.0;

const BG: [f32; 4] = [0.075, 0.08, 0.09, 1.0];
const BG_OTHER_MONTH: [f32; 4] = [0.06, 0.064, 0.072, 1.0];
const GRID_LINE: [f32; 4] = [1.0, 1.0, 1.0, 0.06];
/// The grid lines' drawn width. The lines are the grout between rounded
/// cells, so every crossing carries the cells' corners as concave fillets.
const GRID_LINE_W: f32 = 2.0;
const ACCENT: [f32; 4] = [0.22, 0.42, 0.85, 1.0];
const EVENT_DOT: [f32; 4] = [0.95, 0.72, 0.30, 1.0];
/// Dots for events mirrored from a remote calendar (`source` set), one per
/// account, dealt out in sorted `source` order and wrapping past the end.
/// The first is the blue a lone account always had; none is near the
/// local-event amber.
const ACCOUNT_DOTS: [[f32; 4]; 6] = [
    [0.42, 0.68, 0.95, 1.0], // blue
    [0.45, 0.82, 0.55, 1.0], // green
    [0.90, 0.48, 0.70, 1.0], // pink
    [0.68, 0.55, 0.95, 1.0], // violet
    [0.35, 0.80, 0.80, 1.0], // teal
    [0.95, 0.50, 0.42, 1.0], // coral
];
/// The ring a vault task wears in the grid (an event wears a dot).
const TASK_RING: [f32; 4] = [0.85, 0.86, 0.90, 1.0];
const TASK_RING_DONE: [f32; 4] = [0.45, 0.47, 0.52, 1.0];
const TEXT: [u8; 3] = [225, 228, 232];
const TEXT_DIM: [u8; 3] = [140, 145, 152];
const TEXT_FAINT: [u8; 3] = [95, 100, 108];
const TEXT_ACCENT: [u8; 3] = [120, 165, 255];

/// The month header's three buttons, for hover and press tracking.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HeaderBtn {
    Prev,
    Next,
    Today,
}

/// The "Today" button's width; the arrows are square at the button height.
const TODAY_BTN_W: f32 = 64.0;
/// The month title's width between the two arrows.
const TITLE_W: f32 = 190.0;
/// The Accounts menu button's width, left of "Today".
const ACCOUNTS_W: f32 = 104.0;
/// The Accounts menu's `selected` between picks: no row, so every pick —
/// the same row twice included — reads as a change.
const NO_ROW: usize = 999;
/// The calendar glyph's slot at a day-pane row's right end.
const CAL_S: f32 = 18.0;

/// What an open date picker, or a drag, moves.
#[derive(Clone, Debug)]
enum PickTarget {
    /// The event at `idx` in `events[day]`.
    Event { day: NaiveDate, idx: usize },
    /// The vault task on `line` of `path`.
    Task { path: String, line: usize },
}

/// What a grid chip stands for: the `i`th of the day's shown events, or of
/// its tasks.
#[derive(Clone, Copy, Debug)]
enum Chip {
    Event(usize),
    Task(usize),
}

/// How far the pointer travels before a press on an item becomes a drag.
const DRAG_SLOP: f32 = 5.0;

/// A press on an event or task that may become a drag.
struct Drag {
    /// `None` for a repeating instance, which cannot move but still ticks
    /// or selects as a press.
    target: Option<PickTarget>,
    title: String,
    task: bool,
    from: NaiveDate,
    start: (f32, f32),
    at: (f32, f32),
    /// Past `DRAG_SLOP`: the label follows the pointer.
    active: bool,
    /// The day-pane task to tick when the press ends without a drag.
    tick: Option<usize>,
}

#[derive(Debug, Clone)]
enum Message {
    Quit,
}

/// One event on a day. `time` is (hour, minute); untimed events sort after
/// timed ones. `uid`/`source` ride along from synced records so a save from
/// this app never strips what `cce-calendar-sync` wrote.
#[derive(Clone, Debug)]
struct Event {
    time: Option<(u32, u32)>,
    title: String,
    uid: Option<String>,
    source: Option<String>,
    recurring: bool,
}

fn load_events() -> BTreeMap<NaiveDate, Vec<Event>> {
    let mut map: BTreeMap<NaiveDate, Vec<Event>> = BTreeMap::new();
    let records = match load_records() {
        Ok(r) => r,
        Err(e) => {
            log::error!("events.json unreadable, starting empty: {e}");
            return map;
        }
    };
    for rec in records {
        let Ok(date) = rec.date.parse::<NaiveDate>() else {
            continue;
        };
        let time = rec.time.as_deref().and_then(parse_time);
        map.entry(date).or_default().push(Event {
            time,
            title: rec.title,
            uid: rec.uid,
            source: rec.source,
            recurring: rec.recurring,
        });
    }
    for events in map.values_mut() {
        sort_events(events);
    }
    map
}

/// A dated checkbox from the notes vault, on its due day. `text` is
/// without the date.
#[derive(Clone, Debug)]
struct DayTask {
    path: String,
    line: usize,
    text: String,
    done: bool,
}

/// The vault's dated tasks: cce-vault's index of the notes vault (the one
/// cce-notes and cce-list use), kept current by its watcher. The watcher
/// queues changed paths on its own thread; `poll` applies them on the
/// app's tick.
struct VaultTasks {
    index: cce_vault::Index,
    _watcher: Option<cce_vault::VaultWatcher>,
    pending: Arc<Mutex<Vec<PathBuf>>>,
    by_day: BTreeMap<NaiveDate, Vec<DayTask>>,
}

impl VaultTasks {
    /// `None` when no vault is configured or it cannot be read.
    fn open() -> Option<VaultTasks> {
        let root = cce_vault::config::vault_root(None).ok()?;
        let index = cce_vault::Index::open(&root, true).map_err(|e| log::warn!("vault tasks: {e}")).ok()?;
        let pending = Arc::new(Mutex::new(Vec::new()));
        let queue = pending.clone();
        let watcher = cce_vault::VaultWatcher::spawn(&root, move |paths| {
            queue.lock().unwrap_or_else(|e| e.into_inner()).extend(paths);
        })
        .map_err(|e| log::warn!("vault watcher: {e}"))
        .ok();
        let mut v = VaultTasks { index, _watcher: watcher, pending, by_day: BTreeMap::new() };
        v.rebuild();
        Some(v)
    }

    fn rebuild(&mut self) {
        let mut by_day: BTreeMap<NaiveDate, Vec<DayTask>> = BTreeMap::new();
        for (path, t) in self.index.tasks() {
            if let Some(due) = t.due {
                by_day.entry(due).or_default().push(DayTask {
                    path: path.to_string(),
                    line: t.line,
                    text: cce_vault::split_due(&t.text).0,
                    done: !t.is_open(),
                });
            }
        }
        self.by_day = by_day;
    }

    /// Apply queued vault changes; true when the tasks may have changed.
    fn poll(&mut self) -> bool {
        let paths: Vec<PathBuf> = std::mem::take(&mut *self.pending.lock().unwrap_or_else(|e| e.into_inner()));
        if paths.is_empty() {
            return false;
        }
        self.index.apply_changes(&paths);
        self.rebuild();
        true
    }

    /// Tick or untick a task in its note. cce-vault re-reads the note
    /// first, so an edit made elsewhere a moment ago is not overwritten.
    /// Set or clear a task's due date in its note (cce-vault re-reads it).
    fn set_due(&mut self, path: &str, line: usize, due: Option<NaiveDate>) -> Result<(), String> {
        self.index.set_task_due(path, line, due).map_err(|e| e.to_string())?;
        self.rebuild();
        Ok(())
    }

    fn set_done(&mut self, path: &str, line: usize, done: bool) -> Result<(), String> {
        self.index.set_task(path, line, if done { 'x' } else { ' ' }).map_err(|e| e.to_string())?;
        self.rebuild();
        Ok(())
    }
}

fn sort_events(events: &mut [Event]) {
    events.sort_by_key(|e| e.time.map_or((1, 0, 0), |(h, m)| (0, h, m)));
}

/// `"H:MM"` / `"HH:MM"` → (hour, minute).
fn parse_time(tok: &str) -> Option<(u32, u32)> {
    let (h, m) = tok.split_once(':')?;
    let (h, m) = (h.parse::<u32>().ok()?, m.parse::<u32>().ok()?);
    (h < 24 && m < 60 && !tok.starts_with('+')).then_some((h, m))
}

/// A leading `HH:MM` token becomes the event time; the rest is the title.
fn parse_event(raw: &str) -> Option<Event> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    let (time, title) = match raw.split_once(char::is_whitespace) {
        Some((tok, rest)) if parse_time(tok).is_some() => (parse_time(tok), rest.trim().to_string()),
        _ => match parse_time(raw) {
            Some(t) => (Some(t), String::new()),
            None => (None, raw.to_string()),
        },
    };
    let title = if title.is_empty() { "(untitled)".to_string() } else { title };
    Some(Event { time, title, uid: None, source: None, recurring: false })
}

fn week_start_config() -> Weekday {
    let path = cce_ui::config::get_app_config_path("cce-calendar");
    if let Ok(text) = std::fs::read_to_string(path) {
        if let Ok(doc) = text.parse::<kdl::KdlDocument>() {
            if let Some(v) = doc
                .get("week-start")
                .and_then(|n| n.entries().first())
                .and_then(|e| e.value().as_string())
            {
                return match v.to_ascii_lowercase().as_str() {
                    "sunday" | "sun" => Weekday::Sun,
                    "saturday" | "sat" => Weekday::Sat,
                    _ => Weekday::Mon,
                };
            }
        }
    }
    Weekday::Mon
}

/// `account-color "<key>" "#rrggbb"` nodes from the app config. The key is
/// a full source (`"google:me@gmail.com"`) or a bare email, which then
/// covers that address's iCloud and Google accounts alike.
fn account_colors_config() -> Vec<(String, [f32; 4])> {
    let path = cce_ui::config::get_app_config_path("cce-calendar");
    let Some(doc) = std::fs::read_to_string(path).ok().and_then(|t| t.parse::<kdl::KdlDocument>().ok())
    else {
        return Vec::new();
    };
    doc.nodes()
        .iter()
        .filter(|n| n.name().value() == "account-color")
        .filter_map(|n| {
            let mut args = n.entries().iter().filter(|e| e.name().is_none()).filter_map(|e| e.value().as_string());
            let key = args.next()?;
            let Some(color) = args.next().and_then(cce_ui::color::parse_hex_rgba) else {
                log::warn!("account-color {key:?}: expected a \"#rrggbb\" colour");
                return None;
            };
            Some((key.to_string(), color))
        })
        .collect()
}

/// The dot colour of every account that has events on file. A configured
/// colour wins (an exact source over a bare email); the rest take
/// `ACCOUNT_DOTS` in turn, so accounts stay distinct until the palette runs
/// out and keep their colours while the set of accounts does not change.
fn source_colors(
    events: &BTreeMap<NaiveDate, Vec<Event>>,
    configured: &[(String, [f32; 4])],
) -> BTreeMap<String, [f32; 4]> {
    let sources: BTreeSet<&str> = events.values().flatten().filter_map(|e| e.source.as_deref()).collect();
    let mut next = 0;
    sources
        .into_iter()
        .map(|source| {
            let email = source.split_once(':').map_or(source, |(_, e)| e);
            let find = |key: &str| configured.iter().find(|(k, _)| k.eq_ignore_ascii_case(key)).map(|(_, c)| *c);
            let color = find(source).or_else(|| find(email)).unwrap_or_else(|| {
                next += 1;
                ACCOUNT_DOTS[(next - 1) % ACCOUNT_DOTS.len()]
            });
            (source.to_string(), color)
        })
        .collect()
}

/// `hide-account "<kind>:<email>"` nodes: accounts whose events the app
/// does not draw.
fn hidden_accounts_config() -> BTreeSet<String> {
    let Some(doc) = std::fs::read_to_string(cce_calendar::config_path())
        .ok()
        .and_then(|t| t.parse::<kdl::KdlDocument>().ok())
    else {
        return BTreeSet::new();
    };
    doc.nodes()
        .iter()
        .filter(|n| n.name().value() == "hide-account")
        .filter_map(|n| n.entries().iter().find(|e| e.name().is_none())?.value().as_string())
        .map(String::from)
        .collect()
}

fn save_hidden_accounts(hidden: &BTreeSet<String>) -> std::io::Result<()> {
    edit_config(|doc| {
        let nodes = doc.nodes_mut();
        nodes.retain(|n| n.name().value() != "hide-account");
        for source in hidden {
            let mut node = kdl::KdlNode::new("hide-account");
            node.push(kdl::KdlEntry::new(source.clone()));
            nodes.push(node);
        }
    })
}

fn account_label(a: &CalendarAccount) -> String {
    let kind = if a.kind == "icloud" { "iCloud" } else { "Google" };
    format!("{kind} · {}", a.email)
}

fn add_months(year: i32, month: u32, delta: i32) -> (i32, u32) {
    let idx = year * 12 + month as i32 - 1 + delta;
    (idx.div_euclid(12), (idx.rem_euclid(12) + 1) as u32)
}

fn days_in_month(year: i32, month: u32) -> u32 {
    let (ny, nm) = add_months(year, month, 1);
    NaiveDate::from_ymd_opt(ny, nm, 1)
        .and_then(|d| d.pred_opt())
        .map_or(28, |d| d.day())
}

const MONTHS: [&str; 12] = [
    "January", "February", "March", "April", "May", "June", "July", "August", "September",
    "October", "November", "December",
];
const WEEKDAYS: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];

fn weekday_offset(day: Weekday, week_start: Weekday) -> u32 {
    (day.num_days_from_monday() + 7 - week_start.num_days_from_monday()) % 7
}

/// The window geometry every frame and every hit-test derive from.
struct Geom {
    /// The month header band across the top of the grid column.
    header: Rect,
    prev_btn: Rect,
    next_btn: Rect,
    today_btn: Rect,
    /// The Accounts menu trigger; `None` when the header is too narrow to
    /// fit it beside the month title.
    accounts_btn: Option<Rect>,
    /// The month grid's frame, outer edge of the outer lines.
    grid: Rect,
    /// The pitch: one line centre to the next.
    cell_w: f32,
    cell_h: f32,
    /// A cell face's corner radius — the fillet at every line crossing.
    cell_radius: f32,
    sidebar: Rect,
    /// The event list's clip inside the sidebar well: full well width,
    /// between the heading block and the bottom strip.
    list: Rect,
    /// The bottom strip inside the sidebar well — the input field while
    /// typing, else the key hints.
    strip: Rect,
    /// Date of the grid's top-left cell (always a 42-day window).
    first_day: NaiveDate,
}

impl Geom {
    /// Column `col`'s left line centre (`col` 7 is the right frame line).
    fn col_x(&self, col: u64) -> f32 {
        self.grid.x + GRID_LINE_W / 2.0 + col as f32 * self.cell_w
    }

    fn row_y(&self, row: u64) -> f32 {
        self.grid.y + GRID_LINE_W / 2.0 + row as f32 * self.cell_h
    }

    /// The face of `date`'s cell (it may lie outside the shown weeks).
    fn cell_of(&self, date: NaiveDate) -> Rect {
        let i = (date - self.first_day).num_days().clamp(0, 41) as u64;
        self.cell(i / 7, i % 7)
    }

    /// A cell's face: the pitch box less half a line on every side — the
    /// rounded pocket the grout leaves.
    fn cell(&self, row: u64, col: u64) -> Rect {
        Rect {
            x: self.col_x(col) + GRID_LINE_W / 2.0,
            y: self.row_y(row) + GRID_LINE_W / 2.0,
            width: (self.cell_w - GRID_LINE_W).max(0.0),
            height: (self.cell_h - GRID_LINE_W).max(0.0),
        }
    }
}

/// `text`'s advance in `font` at `size`, shaped as the renderer draws it —
/// what a caret or a column after the text must be placed by.
fn shaped_width(text: &str, size: f32, font: &str) -> f32 {
    let mut fs = cce_ui::geometry_font_system().lock().unwrap_or_else(|e| e.into_inner());
    cce_ui::backend::window_runner::shaped_cluster_offsets(&mut fs, text, size, Some(font))
        .last()
        .map_or(0.0, |&(_, x)| x)
}


struct CalendarApp {
    events: BTreeMap<NaiveDate, Vec<Event>>,
    /// `account-color` overrides from the config, read once at start.
    account_colors: Vec<(String, [f32; 4])>,
    /// Each synced account's dot, rebuilt whenever `events` is.
    source_colors: BTreeMap<String, [f32; 4]>,
    /// The accounts the sync mirrors (accounts.json), for the Accounts menu.
    accounts: Vec<CalendarAccount>,
    /// Sources (`kind:email`) whose events are not drawn. They stay in
    /// `events` and in the file: a save that dropped them would read to the
    /// sync as deletions, and it would delete them upstream.
    hidden: BTreeSet<String>,
    /// `push-to` as configured; `None` leaves the choice to the sync.
    push: Option<PushTarget>,
    ui_context: UiContext,
    accounts_menu: Handle<Adapted<Dropdown>>,
    /// Dated tasks from the notes vault; `None` without a vault.
    vault: Option<VaultTasks>,
    /// The open date picker, what it moves, and that thing's day-pane row.
    picker: Option<(DatePicker, PickTarget, usize)>,
    /// A press on an item, and the drag it may have become.
    drag: Option<Drag>,
    /// Displayed (year, month).
    view: (i32, u32),
    selected: NaiveDate,
    /// Index into the selected day's (sorted) events, for deletion.
    sel_event: Option<usize>,
    /// `Some(buffer)` while typing a new event.
    input: Option<String>,
    week_start: Weekday,
    today: NaiveDate,
    win: (f32, f32),
    sidebar_scroll: f32,
    /// Drives `sidebar_scroll` (the drawn value) from the wheel: notches
    /// glide, fingers track 1:1 and fling on the lift. `select` resets the
    /// offset directly; the motion adopts that through `reconcile`.
    sidebar_motion: ScrollMotion,
    /// Trackpad pixels accumulated toward the next month step over the grid,
    /// so a gesture flips one month per MONTH_STEP_PX rather than one per
    /// pixel event. Cleared by a wheel notch and by the finger lift.
    month_wheel_px: f32,
    status: Option<String>,
    /// The header button under the pointer, and the one held down.
    hover_btn: Option<HeaderBtn>,
    pressed_btn: Option<HeaderBtn>,
    /// The day view's event row under the pointer.
    hover_row: Option<usize>,
    /// events.json as last read: mtime, polled once a second so the sync's
    /// rewrites (pushed identities, phone-side changes) show up live.
    file_mtime: Option<std::time::SystemTime>,
    /// When the mtime check below may run again. A wall clock, not an
    /// accumulation of `tick`'s `dt`: `dt` is animation time, clamped to one
    /// frame after an idle sleep, and a calendar nobody is touching is idle —
    /// so the "once a second" watch actually ran about once a minute.
    watch_at: std::time::Instant,
}

fn file_mtime() -> Option<std::time::SystemTime> {
    std::fs::metadata(cce_calendar::data_path()).and_then(|m| m.modified()).ok()
}

impl CalendarApp {
    fn geom(&self) -> Geom {
        let (w, h) = self.win;
        // Everything stands on the root plate `root_plate_inset` in from
        // the window edge; the grid column and the sidebar well are
        // siblings on it, `root_plate_gap` apart:
        // [inset][header+grid][gap][sidebar][inset].
        let inset = root_plate_inset();
        let gap = root_plate_gap();
        let sidebar_w = SIDEBAR_W.min(w * 0.4);
        let grid_w = (w - 2.0 * inset - gap - sidebar_w).max(0.0);
        let header = Rect { x: inset, y: inset, width: grid_w, height: HEADER_H };
        let grid = Rect {
            x: inset,
            y: header.y + HEADER_H + WEEKDAY_H,
            width: grid_w,
            height: (h - inset - (header.y + HEADER_H + WEEKDAY_H)).max(0.0),
        };
        let btn_h = button_height().min(HEADER_H);
        let btn = |x: f32, w: f32| Rect { x, y: header.y + (HEADER_H - btn_h) / 2.0, width: w, height: btn_h };
        let sidebar = Rect {
            x: header.x + grid_w + gap,
            y: inset,
            width: sidebar_w,
            height: (h - 2.0 * inset).max(0.0),
        };
        // Inside the sidebar well: `plate_padding` off its rim, the
        // heading block, the list and the bottom strip `plate_gap` apart.
        let pad = plate_padding();
        let strip_h = INPUT_H - 8.0;
        let strip = Rect {
            x: sidebar.x + pad,
            y: sidebar.y + sidebar.height - pad - strip_h,
            width: (sidebar.width - 2.0 * pad).max(0.0),
            height: strip_h,
        };
        let list_y = sidebar.y + pad + SIDEBAR_HEAD_H + plate_gap();
        let list = Rect {
            x: sidebar.x,
            y: list_y,
            width: sidebar.width,
            height: (strip.y - plate_gap() - list_y).max(0.0),
        };
        // Seven pitches plus one line span the frame, so the outer lines
        // are as wide as the inner ones.
        let cell_w = ((grid.width - GRID_LINE_W) / 7.0).max(0.0);
        let cell_h = ((grid.height - GRID_LINE_W) / 6.0).max(0.0);
        let cell_radius = plate_corner_radius()
            .min((cell_w.min(cell_h) - GRID_LINE_W).max(0.0) / 4.0);
        let first_of_month = NaiveDate::from_ymd_opt(self.view.0, self.view.1, 1)
            .unwrap_or(self.today);
        let back = weekday_offset(first_of_month.weekday(), self.week_start);
        let first_day = first_of_month
            .checked_sub_days(Days::new(back as u64))
            .unwrap_or(first_of_month);
        let today_btn = btn(header.x + header.width - TODAY_BTN_W, TODAY_BTN_W);
        let next_btn = btn(header.x + btn_h + TITLE_W, btn_h);
        let accounts_x = today_btn.x - plate_gap() - ACCOUNTS_W;
        let accounts_btn = (accounts_x >= next_btn.x + next_btn.width + plate_gap()).then(|| {
            let h = cce_ui::layout::dropdown_height().min(HEADER_H);
            Rect { x: accounts_x, y: header.y + (HEADER_H - h) / 2.0, width: ACCOUNTS_W, height: h }
        });
        Geom {
            header,
            prev_btn: btn(header.x, btn_h),
            next_btn,
            today_btn,
            accounts_btn,
            grid,
            cell_w,
            cell_h,
            cell_radius,
            sidebar,
            list,
            strip,
            first_day,
        }
    }

    fn day_at(&self, g: &Geom, x: f32, y: f32) -> Option<NaiveDate> {
        if !g.grid.contains(x, y) {
            return None;
        }
        let col = ((x - g.col_x(0)).max(0.0) / g.cell_w) as u64;
        let row = ((y - g.row_y(0)).max(0.0) / g.cell_h) as u64;
        g.first_day.checked_add_days(Days::new(row.min(5) * 7 + col.min(6)))
    }

    fn header_btn_at(g: &Geom, x: f32, y: f32) -> Option<HeaderBtn> {
        [(HeaderBtn::Prev, g.prev_btn), (HeaderBtn::Next, g.next_btn), (HeaderBtn::Today, g.today_btn)]
            .into_iter()
            .find(|(_, r)| r.contains(x, y))
            .map(|(b, _)| b)
    }

    /// The selected day's event row at (x, y), if any — only inside the
    /// list's clip, so a row scrolled under the heading or strip is not hit.
    fn row_at(&self, g: &Geom, x: f32, y: f32) -> Option<usize> {
        if !g.list.contains(x, y) {
            return None;
        }
        let rows = self.day_rows(self.selected);
        (0..rows).find(|&i| self.sidebar_row(g, i).contains(x, y))
    }

    /// A row's state wash: the colour cce-ui's list-row Button wears in the
    /// same state (selected, hovered), asked of the toolkit rather than
    /// copied, so the rows follow it. Transparent at rest.
    fn row_wash(&self, i: usize) -> [f32; 4] {
        let mut row = Button::new_list_row(0.0, 0.0, 0.0, 0.0).with_selected(self.sel_event == Some(i));
        row.set_hovered(self.hover_row == Some(i));
        row.color()
    }

    fn select(&mut self, date: NaiveDate) {
        self.selected = date;
        self.sel_event = None;
        self.sidebar_scroll = 0.0;
        self.status = None;
        if (date.year(), date.month()) != self.view {
            self.view = (date.year(), date.month());
        }
    }

    /// How far the selected day's event rows overflow the sidebar's list area.
    fn sidebar_overflow(&self, g: &Geom) -> f32 {
        let rows = self.day_rows(self.selected);
        (rows as f32 * ROW_H - g.list.height).max(0.0)
    }

    /// Per-frame sidebar glide/coast; true while `sidebar_scroll` is still
    /// moving, so the frame loop keeps drawing.
    fn tick_sidebar_scroll(&mut self, dt: f32) -> bool {
        self.sidebar_motion.reconcile(0.0, self.sidebar_scroll);
        if !self.sidebar_motion.is_animating() {
            return false;
        }
        let g = self.geom();
        let overflow = self.sidebar_overflow(&g);
        let moved = self.sidebar_motion.tick(dt, Bounds::max(0.0), Bounds::max(overflow));
        self.sidebar_scroll = self.sidebar_motion.y.pos();
        moved || self.sidebar_motion.is_animating()
    }

    fn shift_months(&mut self, delta: i32) {
        let (y, m) = add_months(self.view.0, self.view.1, delta);
        self.view = (y, m);
        let day = self.selected.day().min(days_in_month(y, m));
        if let Some(d) = NaiveDate::from_ymd_opt(y, m, day) {
            self.selected = d;
            self.sel_event = None;
        }
    }

    fn shift_selected_days(&mut self, delta: i64) {
        let moved = if delta >= 0 {
            self.selected.checked_add_days(Days::new(delta as u64))
        } else {
            self.selected.checked_sub_days(Days::new((-delta) as u64))
        };
        if let Some(d) = moved {
            self.select(d);
        }
    }

    /// The day's events that are drawn — all but hidden accounts' — each
    /// with its index in `events[date]`. `sel_event`, `hover_row` and the
    /// sidebar rows index this list, not the day's full one.
    fn shown(&self, date: NaiveDate) -> Vec<(usize, &Event)> {
        self.events.get(&date).map_or_else(Vec::new, |day| {
            day.iter()
                .enumerate()
                .filter(|(_, e)| e.source.as_ref().is_none_or(|s| !self.hidden.contains(s)))
                .collect()
        })
    }

    /// A day-pane row's calendar-glyph slot, at its right end.
    fn date_slot(row: Rect) -> Rect {
        Rect { x: row.x + row.width - 4.0 - CAL_S, y: row.y + (row.height - CAL_S) / 2.0, width: CAL_S, height: CAL_S }
    }

    /// What day-pane row `i` would move, its title, and whether it can lose
    /// its date: an event that is not a repeating instance (an event keeps
    /// a date), or a task (Clear undates it). `None` for anything else.
    fn row_target(&self, i: usize) -> Option<(PickTarget, String, bool)> {
        let day = self.selected;
        let shown = self.shown(day);
        if let Some((idx, e)) = shown.get(i) {
            return (!e.recurring).then(|| (PickTarget::Event { day, idx: *idx }, e.title.clone(), false));
        }
        let t = self.tasks(day).get(i - shown.len())?;
        Some((PickTarget::Task { path: t.path.clone(), line: t.line }, t.text.clone(), true))
    }

    fn open_picker(&mut self, i: usize, g: &Geom) {
        let Some((target, title, clearable)) = self.row_target(i) else { return };
        let row = self.sidebar_row(g, i);
        let picker = DatePicker::open(row, row.x + row.width, Some(self.selected), self.today, self.win)
            .with_title(title)
            .with_week_start(self.week_start);
        self.picker = Some((if clearable { picker } else { picker.without_clear() }, target, i));
        self.hover_row = None;
        self.sel_event = None;
    }

    /// Act on what the picker answered: move its row to the picked day (and
    /// follow it there), or for a task, Clear its date. True on a change.
    fn apply_picker(&mut self, outcome: PickOutcome) -> bool {
        let date = match outcome {
            PickOutcome::Ignored => return false,
            PickOutcome::Redraw => return true,
            PickOutcome::Close => {
                self.picker = None;
                return true;
            }
            PickOutcome::Set(d) => Some(d),
            PickOutcome::Clear => None,
        };
        let Some((_, target, _)) = self.picker.take() else { return true };
        self.move_target(target, date);
        true
    }

    /// Move an event or task to `to` and follow it there; `None` undates a
    /// task (an event keeps its day). The picker's and a drop's one path.
    fn move_target(&mut self, target: PickTarget, to: Option<NaiveDate>) {
        match target {
            PickTarget::Event { day, idx } => {
                let Some(to) = to.filter(|&d| d != day) else { return };
                let Some(list) = self.events.get_mut(&day).filter(|l| idx < l.len()) else { return };
                let event = list.remove(idx);
                if list.is_empty() {
                    self.events.remove(&day);
                }
                let dest = self.events.entry(to).or_default();
                dest.push(event);
                sort_events(dest);
                self.save();
            }
            PickTarget::Task { path, line } => {
                if let Some(v) = self.vault.as_mut() {
                    self.status = v.set_due(&path, line, to).err().map(|e| format!("{path}: {e}"));
                }
            }
        }
        if let Some(d) = to {
            self.select(d);
        }
    }

    /// The chips a grid cell shows, top to bottom, each with its line's
    /// rect, and how many did not fit (the "+N more" line). Shared by the
    /// paint and the drag's hit test.
    fn cell_chips(&self, date: NaiveDate, cell: Rect) -> (Vec<(Rect, Chip)>, usize) {
        // style: deliberate — a cell's chips are typographic lines under
        // the day number, not ladder spacings.
        let line_h = 15.0;
        let events = self.shown(date).len();
        let total = events + self.tasks(date).len();
        let avail = ((cell.height - 26.0) / line_h).max(0.0) as usize;
        let fit = if avail >= total { total } else { avail.saturating_sub(1) };
        let chips = (0..fit)
            .map(|k| {
                let rect = Rect { x: cell.x + 4.0, y: cell.y + 24.0 + k as f32 * line_h, width: (cell.width - 8.0).max(0.0), height: line_h };
                (rect, if k < events { Chip::Event(k) } else { Chip::Task(k - events) })
            })
            .collect();
        (chips, total - fit)
    }

    /// What chip `chip` of `date` would move, its title, and whether it is
    /// a task. A repeating instance has no target.
    fn chip_target(&self, date: NaiveDate, chip: Chip) -> Option<(Option<PickTarget>, String, bool)> {
        match chip {
            Chip::Event(i) => {
                let (idx, e) = self.shown(date).get(i).map(|(idx, e)| (*idx, *e))?;
                let target = (!e.recurring).then_some(PickTarget::Event { day: date, idx });
                Some((target, e.title.clone(), false))
            }
            Chip::Task(j) => {
                let t = self.tasks(date).get(j)?;
                Some((Some(PickTarget::Task { path: t.path.clone(), line: t.line }), t.text.clone(), true))
            }
        }
    }

    /// The day a drop at the drag's pointer would land on, while dragging.
    fn drop_day(&self, g: &Geom) -> Option<NaiveDate> {
        let d = self.drag.as_ref().filter(|d| d.active && d.target.is_some())?;
        self.day_at(g, d.at.0, d.at.1)
    }

    /// The dragged item's label at the pointer: its marker and title on a
    /// small plate of the cell face's colour.
    fn paint_drag(&self, pc: &mut PaintCtx) {
        let Some(d) = self.drag.as_ref().filter(|d| d.active && d.target.is_some()) else { return };
        // style: deliberate — a pointer-attached label's own insets.
        let w = (shaped_width(&d.title, 11.0, &list_font()) + 26.0).min(240.0);
        let r = Rect { x: d.at.0 + 10.0, y: d.at.1 + 6.0, width: w, height: 20.0 };
        pc.rounded_rect(r, 6.0, (true, true, true, true), [ACCENT[0], ACCENT[1], ACCENT[2], 0.85]);
        if d.task {
            pc.arc(r.x + 10.0, r.y + 10.0, 2.6, 1.2, 0.0, std::f32::consts::TAU, [1.0; 4]);
        } else {
            pc.circle(r.x + 10.0, r.y + 10.0, 2.5, [1.0; 4]);
        }
        pc.text_with(d.title.clone(), r.x + 18.0, r.y + 3.0, 11.0, [255, 255, 255], None,
            Some([r.x, r.y, r.x + r.width - 4.0, r.y + r.height]));
    }

    /// An event's dot: its account's colour, or amber for a local one.
    fn dot_color(&self, e: &Event) -> [f32; 4] {
        match &e.source {
            Some(s) => self.source_colors.get(s).copied().unwrap_or(ACCOUNT_DOTS[0]),
            None => EVENT_DOT,
        }
    }

    /// The day's vault tasks, in note and line order.
    fn tasks(&self, date: NaiveDate) -> &[DayTask] {
        self.vault.as_ref().and_then(|v| v.by_day.get(&date)).map_or(&[], Vec::as_slice)
    }

    /// The day pane's rows: the shown events, then the day's tasks.
    fn day_rows(&self, date: NaiveDate) -> usize {
        self.shown(date).len() + self.tasks(date).len()
    }

    /// Tick or untick the selected day's task `i`.
    fn toggle_task(&mut self, i: usize) {
        let Some(t) = self.tasks(self.selected).get(i).cloned() else { return };
        if let Some(v) = self.vault.as_mut() {
            self.status = v.set_done(&t.path, t.line, !t.done).err().map(|e| format!("{}: {e}", t.path));
        }
    }

    /// The account typed events go to, as the sync resolves `push-to`:
    /// the named account, else the first of the configured kind (iCloud
    /// when unconfigured and there is one). `None` keeps them local.
    fn default_account(&self) -> Option<usize> {
        let (kind, email) = match &self.push {
            Some(p) => (p.kind.as_str(), p.account.as_deref()),
            None if self.accounts.iter().any(|a| a.kind == "icloud") => ("icloud", None),
            None => ("google", None),
        };
        self.accounts
            .iter()
            .position(|a| a.kind == kind && email.is_none_or(|e| e.eq_ignore_ascii_case(&a.email)))
    }

    /// The Accounts menu's rows, dispatched by index in
    /// `accounts_menu_pick`: a show/hide switch per account, a separator,
    /// then the radio group for where typed events go.
    fn accounts_options(&self) -> Vec<String> {
        let mut rows: Vec<String> = self
            .accounts
            .iter()
            .map(|a| {
                let mark = if self.hidden.contains(&a.source()) { "" } else { MARK_CHECK };
                format!("{mark}Show {}", account_label(a))
            })
            .collect();
        rows.push("-".to_string());
        let default = self.default_account();
        let radio = |on: bool| if on { MARK_ON } else { MARK_OFF };
        for (i, a) in self.accounts.iter().enumerate() {
            rows.push(format!("{}New events → {}", radio(default == Some(i)), account_label(a)));
        }
        rows.push(format!("{}New events → this computer only", radio(default.is_none())));
        rows
    }

    fn accounts_menu_pick(&mut self, row: usize) {
        let n = self.accounts.len();
        if let Some(a) = self.accounts.get(row) {
            let source = a.source();
            if !self.hidden.remove(&source) {
                self.hidden.insert(source);
            }
            self.sel_event = None;
            self.hover_row = None;
            self.sidebar_scroll = 0.0;
            self.status = save_hidden_accounts(&self.hidden).err().map(|e| format!("saving config failed: {e}"));
        } else if (n + 1..=2 * n + 1).contains(&row) {
            let pick = row - n - 1;
            let mut target = match self.accounts.get(pick) {
                Some(a) => PushTarget { kind: a.kind.to_string(), account: Some(a.email.clone()), calendar: None },
                None => PushTarget { kind: "none".to_string(), account: None, calendar: None },
            };
            // A named calendar belongs to the account it was set for; keep
            // it only when the pick resolves to that same account.
            if self.default_account() == self.accounts.get(pick).map(|_| pick) {
                target.calendar = self.push.as_ref().and_then(|p| p.calendar.clone());
            }
            self.status = save_push_target(&target).err().map(|e| format!("saving config failed: {e}"));
            self.push = Some(target);
        }
        self.refresh_accounts_menu();
    }

    fn refresh_accounts_menu(&mut self) {
        let options = self.accounts_options();
        let menu = &mut self.ui_context[self.accounts_menu];
        menu.options = options;
        menu.selected = NO_ROW;
    }

    /// After the Accounts menu took an event: act on a pick, and give the
    /// keyboard back to the calendar once the menu is closed — by a pick,
    /// Escape or a click — so Enter and the arrows are the calendar's again
    /// rather than reopening the menu.
    fn drain_accounts_menu(&mut self) {
        if self.ui_context[self.accounts_menu].take_change() {
            let row = self.ui_context[self.accounts_menu].selected;
            self.accounts_menu_pick(row);
        }
        if !self.ui_context[self.accounts_menu].is_expanded() {
            self.ui_context.unfocus_id(self.accounts_menu.id());
        }
    }

    fn save(&mut self) {
        let records: Vec<EventRecord> = self
            .events
            .iter()
            .flat_map(|(date, events)| {
                events.iter().map(move |e| EventRecord {
                    date: date.to_string(),
                    time: e.time.map(|(h, m)| format!("{h:02}:{m:02}")),
                    title: e.title.clone(),
                    uid: e.uid.clone(),
                    source: e.source.clone(),
                    recurring: e.recurring,
                })
            })
            .collect();
        self.status = save_records(&records).err().map(|e| format!("save failed: {e}"));
        // Our own write must not read as an outside change next tick.
        self.file_mtime = file_mtime();
    }

    /// Re-read events.json after something else wrote it, keeping the
    /// selection where it still makes sense.
    fn reload(&mut self) {
        // The rows may have moved under it.
        self.picker = None;
        self.drag = None;
        self.events = load_events();
        self.source_colors = source_colors(&self.events, &self.account_colors);
        self.file_mtime = file_mtime();
        let n = self.shown(self.selected).len();
        if self.sel_event.is_some_and(|i| i >= n) {
            self.sel_event = None;
        }
    }

    fn commit_input(&mut self) {
        let Some(buffer) = self.input.take() else {
            return;
        };
        if let Some(event) = parse_event(&buffer) {
            let day = self.events.entry(self.selected).or_default();
            day.push(event);
            sort_events(day);
            self.save();
        }
    }

    fn delete_selected_event(&mut self) {
        let Some(shown) = self.sel_event.take() else {
            return;
        };
        let Some(idx) = self.shown(self.selected).get(shown).map(|(i, _)| *i) else {
            return;
        };
        if let Some(day) = self.events.get_mut(&self.selected) {
            if day.get(idx).is_some_and(|e| e.recurring) {
                // One instance of a repeating event: the server has no
                // "just this one" the mirror could express, so it stays.
                self.sel_event = Some(shown);
                self.status = Some("repeating event — change it on the phone".to_string());
                return;
            }
            if idx < day.len() {
                day.remove(idx);
                if day.is_empty() {
                    self.events.remove(&self.selected);
                }
                self.save();
            }
        }
    }

    // ── keyboard ──────────────────────────────────────────────────────────

    fn key_input_mode(&mut self, event: &KeyEvent, needs_rebuild: &mut bool) {
        let buffer = self.input.as_mut().expect("input mode");
        match &event.logical_key {
            Key::Named(NamedKey::Escape) => self.input = None,
            Key::Named(NamedKey::Enter) => self.commit_input(),
            Key::Named(NamedKey::Backspace) => {
                buffer.pop();
            }
            _ => {
                if let Some(text) = &event.text {
                    buffer.extend(text.chars().filter(|c| !c.is_control()));
                }
            }
        }
        *needs_rebuild = true;
    }

    fn key_browse_mode(&mut self, event: &KeyEvent) -> (bool, bool) {
        let mut quit = false;
        let mut handled = true;
        match &event.logical_key {
            Key::Named(NamedKey::ArrowLeft) => self.shift_selected_days(-1),
            Key::Named(NamedKey::ArrowRight) => self.shift_selected_days(1),
            Key::Named(NamedKey::ArrowUp) => self.shift_selected_days(-7),
            Key::Named(NamedKey::ArrowDown) => self.shift_selected_days(7),
            Key::Named(NamedKey::PageUp) => self.shift_months(-1),
            Key::Named(NamedKey::PageDown) => self.shift_months(1),
            Key::Named(NamedKey::Home) => self.select(self.today),
            Key::Named(NamedKey::Enter) => self.input = Some(String::new()),
            Key::Named(NamedKey::Delete) => self.delete_selected_event(),
            Key::Character(c) => match c.as_str() {
                "t" => self.select(self.today),
                "n" => self.input = Some(String::new()),
                "d" | "x" => self.delete_selected_event(),
                "[" => self.shift_months(-12),
                "]" => self.shift_months(12),
                "q" => quit = true,
                _ => handled = false,
            },
            _ => handled = false,
        }
        (handled, quit)
    }

    // ── painting ──────────────────────────────────────────────────────────

    fn boxed(rect: Rect, align_h: AlignH) -> TextLayout {
        TextLayout {
            wrap_width: Some(rect.width),
            box_height: rect.height,
            align_h,
            align_v: AlignV::Middle,
        }
    }

    /// A header button as cce-ui's Button draws one: a flush control plate
    /// at the button radius, its face the DE button colour for its state,
    /// then either a bundled cce-icons glyph (placed as Button's icon face:
    /// centred, the short side less 8) or, failing that, the label in the
    /// button font and the control label colour — a word, never a symbol
    /// character, since it shows only when the icon set is missing.
    fn paint_header_btn(&self, pc: &mut PaintCtx, rect: Rect, which: HeaderBtn, icon: Option<&str>, label: &str) {
        let face = if self.pressed_btn == Some(which) {
            button_press_color()
        } else if self.hover_btn == Some(which) {
            button_hover_color()
        } else {
            button_background_color()
        };
        let plate = ControlPlate::control(rect, button_corner_radius(), PlateStance::Flush, Material::face(face));
        pc.control_plate(&plate);
        if let Some((image, iw, ih)) = icon.and_then(|name| cce_ui::upload_icon(name, 32)) {
            let s = (rect.width.min(rect.height) - 8.0).max(4.0);
            let (iw, ih) = (iw as f32, ih as f32);
            let (dw, dh) = if iw >= ih { (s, s * ih / iw.max(1.0)) } else { (s * iw / ih.max(1.0), s) };
            let at = Rect { x: rect.x + (rect.width - dw) / 2.0, y: rect.y + (rect.height - dh) / 2.0, width: dw, height: dh };
            pc.image(image, at, 1.0);
            return;
        }
        let font = button_font();
        let size = parse_font_string(&font).1.unwrap_or(14.0);
        pc.text_boxed(label, rect.x, rect.y, size, control_label_color_u8(), Some(font), None,
            TextAttrs::default(), Self::boxed(rect, AlignH::Center));
    }

    fn paint_header(&self, pc: &mut PaintCtx, g: &Geom) {
        let bold = TextAttrs { italic: false, weight: Some(700), ..Default::default() };
        self.paint_header_btn(pc, g.prev_btn, HeaderBtn::Prev, Some("chevron-left"), "Prev");
        self.paint_header_btn(pc, g.next_btn, HeaderBtn::Next, Some("chevron-right"), "Next");
        let title = Rect {
            x: g.prev_btn.x + g.prev_btn.width,
            y: g.header.y,
            width: g.next_btn.x - (g.prev_btn.x + g.prev_btn.width),
            height: HEADER_H,
        };
        let label = format!("{} {}", MONTHS[self.view.1 as usize - 1], self.view.0);
        pc.text_boxed(label, title.x, title.y, 16.0, TEXT, None, None, bold,
            Self::boxed(title, AlignH::Center));
        self.paint_header_btn(pc, g.today_btn, HeaderBtn::Today, None, "Today");
    }

    fn paint_grid(&self, pc: &mut PaintCtx, g: &Geom) {
        let drop = self.drop_day(g);
        for (i, name) in WEEKDAYS.iter().cycle()
            .skip(self.week_start.num_days_from_monday() as usize)
            .take(7)
            .enumerate()
        {
            let rect = Rect {
                x: g.col_x(i as u64),
                y: g.header.y + g.header.height,
                width: g.cell_w,
                height: WEEKDAY_H,
            };
            pc.text_boxed(*name, rect.x, rect.y, 11.0, TEXT_FAINT, None, None,
                TextAttrs::default(), Self::boxed(rect, AlignH::Center));
        }

        for i in 0..42u64 {
            let Some(date) = g.first_day.checked_add_days(Days::new(i)) else { continue };
            let (row, col) = (i / 7, i % 7);
            let cell = g.cell(row, col);
            let radius = g.cell_radius;
            let all = (true, true, true, true);
            let in_month = (date.year(), date.month()) == self.view;
            if !in_month {
                pc.rounded_rect(cell, radius, all, BG_OTHER_MONTH);
            }
            // style: deliberate — the cells are grout-separated grid cells,
            // not plates: the selection ring is a 2px band traced on the
            // face's own outline (unfilled, so the face shows through), and
            // the day number, dots and chips are tight typographic offsets,
            // not ladder spacings.
            if date == self.selected {
                pc.border(cell, (radius, radius, radius, radius), [0.0; 4],
                    [ACCENT[0], ACCENT[1], ACCENT[2], 0.55], 2.0);
            }

            // Day number, top-left; today gets an accent pill.
            let num = Rect { x: cell.x + 6.0, y: cell.y + 4.0, width: 24.0, height: 17.0 };
            if date == self.today {
                pc.rounded_rect(num, 8.0, (true, true, true, true), ACCENT);
            }
            let num_color = if date == self.today {
                [255, 255, 255]
            } else if in_month {
                TEXT
            } else {
                TEXT_FAINT
            };
            pc.text_boxed(date.day().to_string(), num.x, num.y, 11.5, num_color, None, None,
                TextAttrs { italic: false, weight: Some(600), ..Default::default() }, Self::boxed(num, AlignH::Center));

            // The day a drag would drop on: the selection ring, brighter.
            if drop == Some(date) {
                pc.border(cell, (radius, radius, radius, radius), [0.0; 4], ACCENT, 2.5);
            }

            // Chips — events (a dot), then vault tasks (a ring) — each a
            // marker and a clipped title, then a "+N" overflow line.
            let (chips, hidden) = self.cell_chips(date, cell);
            if !chips.is_empty() || hidden > 0 {
                let events = self.shown(date);
                let tasks = self.tasks(date);
                let clip = Some([cell.x, cell.y, cell.x + cell.width - 4.0, cell.y + cell.height]);
                let text = if in_month { TEXT } else { TEXT_DIM };
                pc.clip(cell, |pc| {
                    for (r, chip) in &chips {
                        let (mx, my) = (r.x + 5.0, r.y + 6.0);
                        match *chip {
                            Chip::Event(i) => {
                                let e = events[i].1;
                                pc.circle(mx, my, 2.5, self.dot_color(e));
                                pc.text_with(e.title.clone(), r.x + 11.0, r.y, 10.0, text, None, clip);
                            }
                            Chip::Task(j) => {
                                let t = &tasks[j];
                                let (ring, color) = if t.done { (TASK_RING_DONE, TEXT_FAINT) } else { (TASK_RING, text) };
                                pc.arc(mx, my, 2.6, 1.2, 0.0, std::f32::consts::TAU, ring);
                                pc.text_with(t.text.clone(), r.x + 11.0, r.y, 10.0, color, None, clip);
                            }
                        }
                    }
                    if hidden > 0 {
                        let y = chips.last().map_or(cell.y + 24.0, |(r, _)| r.y + r.height);
                        pc.text_with(format!("+{hidden} more"), cell.x + 15.0, y, 10.0, TEXT_FAINT, None, None);
                    }
                });
            }
        }

        // The lines are grout: one draw paints everything in the frame
        // outside the rounded cell faces, so each crossing is filleted by
        // the four corners meeting at it. The frame's own outer corners are
        // clipped concentric with the corner cells'.
        let origin = (g.col_x(0) + g.cell_w / 2.0, g.row_y(0) + g.cell_h / 2.0);
        let face = ((g.cell_w - GRID_LINE_W).max(0.0), (g.cell_h - GRID_LINE_W).max(0.0));
        pc.clip_rounded(g.grid, g.cell_radius + GRID_LINE_W, |pc| {
            pc.grout(g.grid, (g.cell_w, g.cell_h), origin, face, g.cell_radius, GRID_LINE);
        });
    }

    /// Sidebar event-row rects, matching `paint_sidebar` (shared with hit-testing).
    fn sidebar_row(&self, g: &Geom, idx: usize) -> Rect {
        let pad = plate_padding();
        Rect {
            x: g.sidebar.x + pad,
            y: g.list.y + idx as f32 * ROW_H - self.sidebar_scroll,
            width: (g.sidebar.width - 2.0 * pad).max(0.0),
            // style: deliberate — a 4px hairline between rows in a list,
            // not a rung gap.
            height: ROW_H - 4.0,
        }
    }

    fn paint_sidebar(&self, pc: &mut PaintCtx, g: &Geom) {
        // The day view is a well carved into the root plate, not a plate
        // standing on it: the floor is the root plate showing through, the
        // wall kept inside the sidebar rect so the gap to the grid is the
        // gap. Drawn first so the rows and the strip lie on its floor.
        let r = plate_corner_radius();
        let depth = bevel_width().min(g.sidebar.height * 0.2);
        let (well, radii) = carve_inside(g.sidebar, (r, r, r, r), depth);
        pc.recess(well, radii, depth);

        let pad = plate_padding();
        let heading = format!(
            "{}, {} {}",
            WEEKDAYS[self.selected.weekday().num_days_from_monday() as usize],
            MONTHS[self.selected.month() as usize - 1],
            self.selected.day()
        );
        let color = if self.selected == self.today { TEXT_ACCENT } else { TEXT };
        let head_y = g.sidebar.y + pad;
        // Display-list text draws above all geometry, so the open picker's
        // plate cannot hide the text beneath it: what it covers draws none.
        let cover = self.picker.as_ref().map(|(p, _, _)| p.rect);
        let covered = |r: Rect| {
            cover.is_some_and(|c| r.x < c.x + c.width && c.x < r.x + r.width && r.y < c.y + c.height && c.y < r.y + r.height)
        };
        let head = Rect { x: g.sidebar.x + pad, y: head_y, width: g.sidebar.width - 2.0 * pad, height: SIDEBAR_HEAD_H };
        if !covered(head) {
            pc.text(heading, g.sidebar.x + pad, head_y, 14.0, color);
        }
        if self.selected == self.today && !covered(head) {
            // The heading block's second line (a line advance, not a gap).
            pc.text("today", g.sidebar.x + pad, head_y + 20.0, 10.5, TEXT_FAINT);
        }

        let events = self.shown(self.selected);
        // style: deliberate — the text offsets inside a row and inside the
        // strip (+8/+9/+12, the -4/-8 clip margins, the time column's 12px
        // gutter) are a control's own text insets, not ladder spacings.
        //
        // The rows are list rows, drawn as the toolkit's list-row Button
        // draws one: a state wash at the button radius (`row_wash`), the
        // text in the list font. The title wears the list font colour; the
        // time keeps the accent that says it is timed.
        let font = list_font();
        let (_, size) = list_font_parsed();
        let fc = list_font_color();
        let title_color = [(fc[0] * 255.0) as u8, (fc[1] * 255.0) as u8, (fc[2] * 255.0) as u8];
        let time_w = shaped_width("00:00", size, &font);
        let radius = button_corner_radius();
        let tasks = self.tasks(self.selected);
        // A leading marker column — an event's account dot, a task's
        // checkbox — then the time column, then the title.
        let mark_w = 2.0 * Checkbox::INLINE_HALF + 8.0;
        // A row's calendar glyph: on hover, or while its picker is open, and
        // never on a repeating instance (which cannot move).
        let glyph = |pc: &mut PaintCtx, row: Rect, i: usize| {
            let picking = self.picker.as_ref().is_some_and(|(_, _, row)| *row == i);
            if (self.hover_row == Some(i) || picking) && self.row_target(i).is_some() {
                let slot = Self::date_slot(row);
                let s = 13.0;
                let at = Rect { x: slot.x + (slot.width - s) / 2.0, y: slot.y + (slot.height - s) / 2.0, width: s, height: s };
                pc.icon("calendar", at, cce_ui::colors::to_srgb(cce_ui::colors::TEXT_DIM));
            }
        };
        pc.clip(g.list, |pc| {
            if events.is_empty() && tasks.is_empty() {
                pc.text_with("No events", g.sidebar.x + pad, g.list.y + 9.0, size, TEXT_FAINT,
                    Some(font.clone()), None);
            }
            for (i, (_, e)) in events.iter().enumerate() {
                let row = self.sidebar_row(g, i);
                let wash = self.row_wash(i);
                if wash[3] > 0.0 {
                    pc.rounded_rect(row, radius, (true, true, true, true), wash);
                }
                let ty = align_text_y(row.y, row.height, size, 0.0);
                let mark_cx = row.x + 8.0 + Checkbox::INLINE_HALF;
                pc.circle(mark_cx, row.y + row.height / 2.0, 3.5, self.dot_color(e));
                if covered(row) {
                    continue;
                }
                glyph(pc, row, i);
                let clip_r = Self::date_slot(row).x - 4.0;
                let time = e.time.map_or("——".to_string(), |(h, m)| format!("{h:02}:{m:02}"));
                pc.text_with(time, row.x + 8.0 + mark_w, ty, size,
                    if e.time.is_some() { TEXT_ACCENT } else { TEXT_FAINT }, Some(font.clone()), None);
                pc.text_with(e.title.clone(), row.x + 8.0 + mark_w + time_w + 12.0, ty, size, title_color,
                    Some(font.clone()), Some([row.x, row.y, clip_r, row.y + row.height]));
            }
            for (j, t) in tasks.iter().enumerate() {
                let i = events.len() + j;
                let row = self.sidebar_row(g, i);
                let wash = self.row_wash(i);
                if wash[3] > 0.0 {
                    pc.rounded_rect(row, radius, (true, true, true, true), wash);
                }
                let ty = align_text_y(row.y, row.height, size, 0.0);
                let cx = row.x + 8.0 + Checkbox::INLINE_HALF;
                let cy = row.y + row.height / 2.0;
                Checkbox::paint_inline(pc, cx, cy, Checkbox::INLINE_HALF, t.done);
                if covered(row) {
                    continue;
                }
                glyph(pc, row, i);
                let clip_r = Self::date_slot(row).x - 4.0;
                // A task has no time; its title starts at the time column,
                // so it reads as a to-do rather than a slot in the day.
                let tx = row.x + 8.0 + mark_w;
                let color = if t.done { TEXT_FAINT } else { title_color };
                pc.text_with(t.text.clone(), tx, ty, size, color, Some(font.clone()),
                    Some([row.x, row.y, clip_r, row.y + row.height]));
                if t.done {
                    // Struck through, as cce-list draws a done item.
                    let w = shaped_width(&t.text, size, &font).min(clip_r - tx);
                    pc.vector(tx, cy, tx + w, cy, 1.0, [TEXT_FAINT[0] as f32 / 255.0, TEXT_FAINT[1] as f32 / 255.0,
                        TEXT_FAINT[2] as f32 / 255.0, 1.0], cce_ui::scene::paint::Cap::Flat);
                }
            }
        });

        // Bottom strip: the input field while typing, else the key hints.
        let strip = g.strip;
        if covered(strip) {
            return;
        }
        if let Some(buffer) = &self.input {
            // The field at the toolkit's textbox height, centred in the strip.
            let field_h = textbox_height().min(strip.height);
            let field = Rect { y: strip.y + (strip.height - field_h) / 2.0, height: field_h, ..strip };
            self.paint_input(pc, field, buffer);
        } else {
            self.paint_hints(pc, strip);
        }
    }
}

impl CalendarApp {
    /// The new-event field, drawn as cce-ui's single-line TextBox draws
    /// itself while editing: the textbox background (skipped when
    /// transparent, so the floor shows through), a well carved inside the
    /// strip at the textbox radius with its rim lit in the highlight accent
    /// (a flat accent frame without relief), the value in the detached
    /// control-label font at the control text inset, the placeholder in the
    /// placeholder colour, and a 1.5px caret at the shaped end of the text.
    /// A value wider than the field scrolls so the caret stays in view.
    fn paint_input(&self, pc: &mut PaintCtx, strip: Rect, buffer: &str) {
        let radius = textbox_corner_radius();
        let radii = (radius, radius, radius, radius);
        let bg = textbox_background_color();
        if control_relief() {
            if bg[3] > 0.001 {
                pc.rounded_rect(strip, radius, (true, true, true, true), bg);
            }
            let depth = bevel_width().min(strip.height * 0.2);
            let (well, well_radii) = carve_inside(strip, radii, depth);
            let hc = highlight_primary_color();
            pc.recess_tinted(well, well_radii, depth, [hc[0], hc[1], hc[2]]);
        } else {
            pc.border(strip, radii, bg, well_frame_color(false, true), 1.0);
        }

        let font = control_label_font_detached();
        let (_, size) = control_label_font_detached_parsed();
        let inset = CONTROL_TEXT_INSET;
        let text_y = align_text_y(strip.y, strip.height, size, 0.0);
        let clip = [strip.x + inset, strip.y, strip.x + strip.width - inset, strip.y + strip.height];
        if buffer.is_empty() {
            pc.text_with("HH:MM title", strip.x + inset, text_y, size, textbox_placeholder_text_color(),
                Some(font.clone()), Some(clip));
        }
        let advance = shaped_width(buffer, size, &font);
        let scroll = (advance - (strip.width - 2.0 * inset)).max(0.0);
        let x = strip.x + inset - scroll;
        if !buffer.is_empty() {
            pc.text_with(buffer, x, text_y, size, [0xee, 0xee, 0xf5], Some(font), Some(clip));
        }
        let caret_h = size * 1.15;
        let caret = Rect { x: x + advance, y: text_y + (size - caret_h) / 2.0, width: 1.5, height: caret_h };
        // Open for typing: the on-screen keyboard follows this.
        cce_ui::text_input::claim(caret.x, caret.y, caret.width, caret.height);
        pc.quad(caret, [0.80, 0.80, 0.85, 1.0]);
    }
}

/// A key hint: the keys (one keycap each) and what they do.
type Hint = (&'static [&'static str], &'static str);

/// The strip's hints, most useful first: what does not fit is dropped whole.
/// "today" comes last because the header's Today button already says it.
const HINTS: [Hint; 3] = [(&["n"], "new"), (&["pgup", "pgdn"], "month"), (&["t"], "today")];
const HINTS_SELECTED: [Hint; 3] = [(&["d"], "delete"), (&["n"], "new"), (&["t"], "today")];

impl CalendarApp {
    /// The key hints, or the status message in their place, in the status
    /// bar font. Each key is a keycap — a raised control plate with no face
    /// of its own, so the floor shows through between its lit edges — with
    /// the key in the foreground text colour and the action after it in the
    /// dim one. Hints run left to right, and one that would overflow the
    /// strip is skipped whole (a shorter one after it may still fit), so a
    /// narrow strip loses hints rather than clipping one mid-word.
    fn paint_hints(&self, pc: &mut PaintCtx, strip: Rect) {
        let font = statusbar_font();
        let (_, size) = statusbar_font_parsed();
        let to_u8 = |c: [f32; 4]| [0, 1, 2].map(|i| (c[i] * 255.0).round() as u8);
        let text_y = align_text_y(strip.y, strip.height, size, 0.0);
        if let Some(err) = &self.status {
            pc.text_with(err.clone(), strip.x, text_y, size, [0xee, 0x5c, 0x5c], Some(font),
                Some([strip.x, strip.y, strip.x + strip.width, strip.y + strip.height]));
            return;
        }
        // style: deliberate — a keycap's 6px label inset, the 3px between
        // two caps of one hint, 6px from cap to action and 14px between
        // hints are a control's own text spacings, not ladder rungs.
        let cap_h = (size + 8.0).min(strip.height);
        let cap_y = strip.y + (strip.height - cap_h) / 2.0;
        let radius = button_corner_radius().min(cap_h / 2.0);
        let right = strip.x + strip.width;
        let hints: &[Hint] = if self.sel_event.is_some() { &HINTS_SELECTED } else { &HINTS };
        let mut x = strip.x;
        for (keys, action) in hints {
            let caps: Vec<f32> = keys.iter().map(|k| (shaped_width(k, size, &font) + 12.0).max(cap_h)).collect();
            let action_w = shaped_width(action, size, &font);
            let w = caps.iter().sum::<f32>() + 3.0 * (caps.len() - 1) as f32 + 6.0 + action_w;
            if x + w > right {
                continue;
            }
            for (key, cap_w) in keys.iter().zip(&caps) {
                let cap = Rect { x, y: cap_y, width: *cap_w, height: cap_h };
                pc.control_plate(&ControlPlate::control(cap, radius, PlateStance::Raised, None));
                pc.text_boxed(*key, cap.x, cap.y, size, to_u8(cce_ui::colors::TEXT_FG), Some(font.clone()), None,
                    TextAttrs::default(), Self::boxed(cap, AlignH::Center));
                x += cap_w + 3.0;
            }
            x += 3.0;
            pc.text_with(*action, x, text_y, size, to_u8(cce_ui::colors::TEXT_DIM), Some(font.clone()), None);
            x += action_w + 14.0;
        }
    }
}

impl Application for CalendarApp {
    type Message = Message;

    fn ui_context(&self) -> Option<&UiContext> {
        Some(&self.ui_context)
    }

    fn ui_context_mut(&mut self) -> Option<&mut UiContext> {
        Some(&mut self.ui_context)
    }

    fn create(_sender: cce_ui::engine::AppSender<Self::Message>) -> Self {
        let today = Local::now().date_naive();
        let events = load_events();
        let account_colors = account_colors_config();
        let accounts = calendar_accounts().unwrap_or_else(|e| {
            log::warn!("no accounts for the Accounts menu: {e}");
            Vec::new()
        });
        let mut ui_context = UiContext::new();
        let accounts_menu = ui_context.insert(Dropdown::new(Vec::new(), NO_ROW).with_custom_display_text("Accounts"));
        let mut app = Self {
            source_colors: source_colors(&events, &account_colors),
            events,
            account_colors,
            accounts,
            hidden: hidden_accounts_config(),
            push: push_target_config(),
            ui_context,
            accounts_menu,
            vault: VaultTasks::open(),
            picker: None,
            drag: None,
            view: (today.year(), today.month()),
            selected: today,
            sel_event: None,
            input: None,
            week_start: week_start_config(),
            today,
            win: (1060.0, 720.0),
            sidebar_scroll: 0.0,
            sidebar_motion: ScrollMotion::new(),
            month_wheel_px: 0.0,
            status: None,
            hover_btn: None,
            pressed_btn: None,
            hover_row: None,
            file_mtime: file_mtime(),
            watch_at: std::time::Instant::now(),
        };
        app.refresh_accounts_menu();
        app
    }

    fn settings(&self) -> WindowSettings {
        WindowSettings {
            title: "Calendar".to_string(),
            app_id: "cce-calendar".to_string(),
            width: 1060,
            height: 720,
            fullscreen: false,
            min_size: Some((700, 480)),
        }
    }

    fn update(&mut self, msg: Self::Message, _needs_rebuild: &mut bool, exit: &mut bool) {
        match msg {
            Message::Quit => *exit = true,
        }
    }

    /// The mtime watch in `tick` is work the runner cannot see — nothing
    /// redraws until the file changes underneath us — so name the cadence the
    /// loop has to come back at.
    fn idle_poll_interval(&self) -> Option<std::time::Duration> {
        Some(WATCH_EVERY)
    }

    fn tick(&mut self, dt: f32, needs_rebuild: &mut bool) {
        let now = Local::now().date_naive();
        if now != self.today {
            self.today = now;
            *needs_rebuild = true;
        }
        if self.tick_sidebar_scroll(dt) {
            *needs_rebuild = true;
        }
        // The sync timer rewrites events.json; pick that up without a
        // relaunch — but not mid-typing, which a reload would clobber.
        let now_i = std::time::Instant::now();
        if now_i >= self.watch_at {
            self.watch_at = now_i + WATCH_EVERY;
            if self.input.is_none() && file_mtime() != self.file_mtime {
                self.reload();
                *needs_rebuild = true;
            }
            if self.vault.as_mut().is_some_and(VaultTasks::poll) {
                self.picker = None;
                self.drag = None;
                *needs_rebuild = true;
            }
        }
    }

    fn handle_resize(&mut self, width: f32, height: f32, _scale: f64) {
        self.win = (width, height);
    }

    fn handle_pointer_move(&mut self, pos: LogicalPosition, needs_rebuild: &mut bool) {
        let (px, py) = (pos.x, pos.y);
        // The shared context menu (a right-click on the Accounts menu opens
        // one) has the pointer to itself while open: its row highlight.
        if context_menu::is_visible() {
            if context_menu::cursor_moved(px, py) {
                *needs_rebuild = true;
            }
            return;
        }
        if let Some((p, _, _)) = self.picker.as_mut() {
            // The open picker has the pointer: its own hover, none beneath.
            let hover = p.hit(px, py);
            if hover != p.hover || self.hover_row.is_some() || self.hover_btn.is_some() {
                p.hover = hover;
                self.hover_row = None;
                self.hover_btn = None;
                *needs_rebuild = true;
            }
            return;
        }
        if let Some(d) = self.drag.as_mut() {
            // A held press on an item: past the slop it is a drag, and the
            // label and the drop day follow the pointer.
            d.at = (px, py);
            if !d.active && (px - d.start.0).hypot(py - d.start.1) > DRAG_SLOP {
                d.active = true;
                self.hover_row = None;
            }
            if d.active {
                *needs_rebuild = true;
                return;
            }
        }
        let ev = WidgetEvent::PointerMove { x: px, y: py, local_x: px, local_y: py };
        if self.ui_context.propagate_event(&ev, self.accounts_menu.id()) {
            *needs_rebuild = true;
        }
        if self.ui_context[self.accounts_menu].is_expanded() {
            // The open menu covers the grid: no hover underneath it.
            if self.hover_btn.take().is_some() | self.hover_row.take().is_some() {
                *needs_rebuild = true;
            }
            return;
        }
        let g = self.geom();
        let hover = Self::header_btn_at(&g, pos.x, pos.y);
        let row = self.row_at(&g, pos.x, pos.y);
        if hover != self.hover_btn || row != self.hover_row {
            self.hover_btn = hover;
            self.hover_row = row;
            *needs_rebuild = true;
        }
    }

    fn handle_mouse_input(
        &mut self,
        button: MouseButton,
        state: ElementState,
        pos: LogicalPosition,
        needs_rebuild: &mut bool,
    ) -> Option<Self::Message> {
        let (px, py) = (pos.x, pos.y);
        // The context menu takes every click while open: a row runs, a press
        // elsewhere dismisses it. The toolkit leaves this routing to the app.
        if context_menu::is_visible() {
            if context_menu::mouse_input(button, state, px, py, Some(&mut self.ui_context)) {
                *needs_rebuild = true;
            }
            return None;
        }
        // The open picker takes every press: on it, a pick; anywhere else,
        // it closes and the press goes no further.
        if self.picker.is_some() {
            if state == ElementState::Pressed {
                let hit = self.picker.as_ref().and_then(|(p, _, _)| p.hit(px, py));
                let today = self.today;
                let outcome = match hit {
                    Some(hit) if button == MouseButton::Left => {
                        self.picker.as_mut().map_or(PickOutcome::Ignored, |(p, _, _)| p.press(hit, today))
                    }
                    Some(_) => PickOutcome::Ignored,
                    None => PickOutcome::Close,
                };
                if self.apply_picker(outcome) {
                    *needs_rebuild = true;
                }
            }
            return None;
        }
        // The Accounts menu first: its open list lies over the grid.
        let ev = WidgetEvent::MouseButton { button, state, x: px, y: py, local_x: px, local_y: py };
        if self.ui_context.propagate_event(&ev, self.accounts_menu.id()) {
            self.drain_accounts_menu();
            *needs_rebuild = true;
            return None;
        }
        if state == ElementState::Pressed {
            self.ui_context.unfocus_id(self.accounts_menu.id());
        }
        if button != MouseButton::Left {
            return None;
        }
        if state != ElementState::Pressed {
            if self.pressed_btn.take().is_some() {
                *needs_rebuild = true;
            }
            if let Some(d) = self.drag.take() {
                let g = self.geom();
                if d.active {
                    // A drop on another day moves the item there.
                    if let (Some(target), Some(to)) = (d.target, self.day_at(&g, pos.x, pos.y)) {
                        if to != d.from {
                            self.move_target(target, Some(to));
                        }
                    }
                } else if let Some(j) = d.tick {
                    // A press that never moved: the task row's click.
                    self.toggle_task(j);
                }
                *needs_rebuild = true;
            }
            return None;
        }
        self.drag = None;
        let g = self.geom();
        let (x, y) = (pos.x, pos.y);
        let header_btn = Self::header_btn_at(&g, x, y);
        self.pressed_btn = header_btn;
        if let Some(b) = header_btn {
            match b {
                HeaderBtn::Prev => self.shift_months(-1),
                HeaderBtn::Next => self.shift_months(1),
                HeaderBtn::Today => self.select(self.today),
            }
        } else if let Some(date) = self.day_at(&g, x, y) {
            // A press on a chip may become a drag of that item.
            let chip = self.cell_chips(date, g.cell_of(date)).0.into_iter().find(|(r, _)| r.contains(x, y));
            let press = chip.and_then(|(_, c)| self.chip_target(date, c));
            self.select(date);
            if let Some((target, title, task)) = press {
                self.drag = Some(Drag { target, title, task, from: date, start: (x, y), at: (x, y), active: false, tick: None });
            }
        } else if g.sidebar.contains(x, y) {
            let events = self.shown(self.selected).len();
            let row = self.row_at(&g, x, y);
            if let Some(i) = row.filter(|&i| Self::date_slot(self.sidebar_row(&g, i)).contains(x, y)) {
                if self.row_target(i).is_some() {
                    self.open_picker(i, &g);
                    *needs_rebuild = true;
                    return None;
                }
            }
            // A press on a row may become a drag of it; a task row's tick
            // waits for the release, so a drag does not tick it.
            let from = self.selected;
            let press = row.and_then(|i| {
                let chip = if i < events { Chip::Event(i) } else { Chip::Task(i - events) };
                self.chip_target(from, chip).map(|(target, title, task)| (i, target, title, task))
            });
            match row {
                Some(i) if i >= events => self.sel_event = None,
                row => self.sel_event = row,
            }
            if let Some((i, target, title, task)) = press {
                let tick = (i >= events).then(|| i - events);
                self.drag = Some(Drag { target, title, task, from, start: (x, y), at: (x, y), active: false, tick });
            }
        } else {
            return None;
        }
        *needs_rebuild = true;
        None
    }

    fn handle_mouse_wheel(&mut self, delta: &MouseScrollDelta, pos: LogicalPosition, needs_rebuild: &mut bool) {
        if let Some((p, _, _)) = self.picker.as_mut() {
            // Over the picker the wheel flips its months; elsewhere it
            // closes it (the rows or the month are about to move).
            if p.contains(pos.x, pos.y) {
                let dy = match delta {
                    MouseScrollDelta::LineDelta(_, y) => *y,
                    MouseScrollDelta::PixelDelta(d) => d.y as f32,
                };
                if dy != 0.0 {
                    p.shift_month(if dy < 0.0 { 1 } else { -1 });
                    *needs_rebuild = true;
                }
                return;
            }
            self.picker = None;
            *needs_rebuild = true;
        }
        let g = self.geom();
        if g.sidebar.contains(pos.x, pos.y) {
            // A notch is one row, pixel deltas are 1:1. The motion glides
            // notches and coasts a flick; `tick_sidebar_scroll` carries the
            // drawn offset after it. A true return is the repaint signal.
            let overflow = self.sidebar_overflow(&g);
            self.sidebar_motion.reconcile(0.0, self.sidebar_scroll);
            if self.sidebar_motion.apply(delta, (ROW_H, ROW_H), Bounds::max(0.0), Bounds::max(overflow)) {
                self.sidebar_scroll = self.sidebar_motion.y.pos();
                *needs_rebuild = true;
            }
        } else {
            // Month stepping stays discrete: one month per wheel notch. A
            // trackpad gesture accumulates pixels and steps once per
            // MONTH_STEP_PX instead of once per pixel event, carrying the
            // remainder until the finger lifts.
            let step = match delta {
                MouseScrollDelta::LineDelta(_, y) => {
                    self.month_wheel_px = 0.0;
                    if *y < 0.0 {
                        1
                    } else if *y > 0.0 {
                        -1
                    } else {
                        0
                    }
                }
                MouseScrollDelta::PixelDelta(p) => {
                    if current_scroll_phase() == ScrollPhase::FingerEnd {
                        self.month_wheel_px = 0.0;
                        0
                    } else {
                        self.month_wheel_px += p.y as f32;
                        if self.month_wheel_px <= -MONTH_STEP_PX {
                            self.month_wheel_px += MONTH_STEP_PX;
                            1
                        } else if self.month_wheel_px >= MONTH_STEP_PX {
                            self.month_wheel_px -= MONTH_STEP_PX;
                            -1
                        } else {
                            0
                        }
                    }
                }
            };
            if step != 0 {
                self.shift_months(step);
                *needs_rebuild = true;
            }
        }
    }

    fn handle_key_input(&mut self, event: &KeyEvent, needs_rebuild: &mut bool) -> Option<Self::Message> {
        // Escape drops a drag where it started.
        if self.drag.as_ref().is_some_and(|d| d.active) {
            if event.state == ElementState::Pressed && event.logical_key == Key::Named(NamedKey::Escape) {
                self.drag = None;
                *needs_rebuild = true;
            }
            return None;
        }
        // The open picker takes the keyboard.
        if self.picker.is_some() {
            if event.state == ElementState::Pressed {
                let today = self.today;
                let outcome = self.picker.as_mut().map_or(PickOutcome::Ignored, |(p, _, _)| p.key(event, today));
                if self.apply_picker(outcome) {
                    *needs_rebuild = true;
                }
            }
            return None;
        }
        // The Accounts menu takes keys while open, or while Tab has focused
        // it (Enter / Space opens it) and nothing is being typed; otherwise
        // they are the calendar's.
        let menu = self.accounts_menu.id();
        let focused = self.ui_context.is_focused_id(menu) && self.input.is_none();
        if self.ui_context[self.accounts_menu].is_expanded() || focused {
            let ev = WidgetEvent::KeyInput(event.clone());
            if self.ui_context.propagate_event(&ev, menu) {
                self.drain_accounts_menu();
                *needs_rebuild = true;
                return None;
            }
        }
        if event.state != ElementState::Pressed {
            return None;
        }
        if self.input.is_some() {
            self.key_input_mode(event, needs_rebuild);
            return None;
        }
        let (handled, quit) = self.key_browse_mode(event);
        if handled {
            *needs_rebuild = true;
        }
        quit.then_some(Message::Quit)
    }

    fn display_list(&mut self, size: LogicalSize, _scale: f64) -> Option<DisplayList> {
        self.win = (size.width, size.height);
        let g = self.geom();
        // No accounts, or no room beside the title: no menu (a zero rect
        // also takes it out of hit-testing).
        let menu_rect = g.accounts_btn.filter(|_| !self.accounts.is_empty());
        let r = menu_rect.unwrap_or(Rect { x: 0.0, y: 0.0, width: 0.0, height: 0.0 });
        self.ui_context[self.accounts_menu].set_rect(r.x, r.y, r.width, r.height);
        self.ui_context.rebuild_spatial_grid();
        // Registration feeds the engine's text-occlusion clamp and its
        // close-on-outside-press; the list itself is painted last, below.
        self.ui_context.clear_popovers();
        let open = self.ui_context[self.accounts_menu].popover_rect().is_some();
        if open {
            self.ui_context.register_popover_id(self.accounts_menu.id());
        }
        let mut pc = PaintCtx::new();
        // The standard root plate (cce-ui PlateSpec::window).
        pc.root_plate(size.width, size.height);
        self.paint_header(&mut pc, &g);
        if menu_rect.is_some() {
            cce_ui::scene::painter::paint_root_into(&self.ui_context, &self.ui_context[self.accounts_menu], &mut pc);
        }
        self.paint_grid(&mut pc, &g);
        self.paint_sidebar(&mut pc, &g);
        if open {
            self.ui_context[self.accounts_menu].render_popover(&mut pc);
        }
        if let Some((p, _, _)) = &self.picker {
            p.paint(&mut pc, self.today);
        }
        self.paint_drag(&mut pc);
        Some(pc.finish())
    }

    fn display_list_text(&self) -> bool {
        true
    }

    fn clear_color(&self) -> [f32; 4] {
        BG
    }
}

fn main() {
    env_logger::init();
    cce_ui::engine::run::<CalendarApp>();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn synced(source: &str) -> Event {
        Event { time: None, title: "x".into(), uid: None, source: Some(source.into()), recurring: false }
    }

    #[test]
    fn accounts_get_distinct_dots_and_config_wins() {
        let day = NaiveDate::from_ymd_opt(2026, 10, 9).unwrap();
        let mut events = BTreeMap::new();
        events.insert(day, vec![
            synced("icloud:a@me.com"),
            synced("google:b@gmail.com"),
            synced("google:c@gmail.com"),
            parse_event("local").unwrap(),
        ]);
        let colors = source_colors(&events, &[]);
        assert_eq!(colors.len(), 3);
        // Sorted by source: google:b, google:c, icloud:a.
        assert_eq!(colors["google:b@gmail.com"], ACCOUNT_DOTS[0]);
        assert_eq!(colors["google:c@gmail.com"], ACCOUNT_DOTS[1]);
        assert_eq!(colors["icloud:a@me.com"], ACCOUNT_DOTS[2]);

        let red = [1.0, 0.0, 0.0, 1.0];
        let colors = source_colors(&events, &[("B@gmail.com".into(), red)]);
        assert_eq!(colors["google:b@gmail.com"], red);
        // A pinned account does not use up a palette slot.
        assert_eq!(colors["google:c@gmail.com"], ACCOUNT_DOTS[0]);
    }
}
