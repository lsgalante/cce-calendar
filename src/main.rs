//! cce-calendar — month-view calendar with per-day events.
//!
//! A 6×7 month grid on the left, a day pane (a well in the root plate) on
//! the right. Events live in
//! `$XDG_DATA_HOME/cce/calendar/events.json` (one flat list of
//! date/time/title records) and are saved on every mutation. Records with a
//! `source` are mirrored from a remote calendar by `cce-calendar-sync`
//! (blue dot). Events typed here are pushed to the default calendar on the
//! next sync tick and come back carrying their identity; deleting a synced
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
//! default is iCloud when such an account exists).

use std::collections::BTreeMap;

use cce_calendar::{load_records, save_records, EventRecord};
use chrono::{Datelike, Days, Local, NaiveDate, Weekday};
use wayland_client::QueueHandle;

use cce_ui::engine::{Application, EngineState, LogicalPosition, LogicalSize, WindowSettings};
use cce_ui::colors::{
    button_background_color, button_hover_color, button_press_color, control_label_color_u8,
    highlight_primary_color, list_font_color, textbox_background_color, textbox_placeholder_text_color,
    well_frame_color,
};
use cce_ui::layout::{
    align_text_y, bevel_width, button_corner_radius, button_font, button_height, carve_inside,
    control_label_font_detached, control_label_font_detached_parsed, control_relief, list_font,
    list_font_parsed, parse_font_string, plate_corner_radius, statusbar_font, statusbar_font_parsed,
    textbox_corner_radius, CONTROL_TEXT_INSET, plate_gap, plate_padding, root_plate_gap, root_plate_inset,
};
use cce_ui::scene::layout::Rect;
use cce_ui::scene::paint::{AlignH, AlignV, ControlPlate, DisplayList, PaintCtx, PlateStance, TextAttrs, TextLayout};
use cce_ui::scene::Material;
use cce_ui::widget::scroll_motion::{current_scroll_phase, Bounds, ScrollMotion, ScrollPhase};
use cce_ui::widget::{Button, ElementState, Key, KeyEvent, MouseButton, MouseScrollDelta, NamedKey, Paint};

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
/// Dot for events mirrored from a remote calendar (`source` set).
const SYNC_DOT: [f32; 4] = [0.42, 0.68, 0.95, 1.0];
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
        Geom {
            header,
            prev_btn: btn(header.x, btn_h),
            next_btn: btn(header.x + btn_h + TITLE_W, btn_h),
            today_btn: btn(header.x + header.width - TODAY_BTN_W, TODAY_BTN_W),
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
        let events = self.events.get(&self.selected).map_or(0, Vec::len);
        (0..events).find(|&i| self.sidebar_row(g, i).contains(x, y))
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
        let events = self.events.get(&self.selected).map_or(0, Vec::len);
        (events as f32 * ROW_H - g.list.height).max(0.0)
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
        self.events = load_events();
        self.file_mtime = file_mtime();
        let n = self.events.get(&self.selected).map_or(0, Vec::len);
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
        let Some(idx) = self.sel_event.take() else {
            return;
        };
        if let Some(day) = self.events.get_mut(&self.selected) {
            if day.get(idx).is_some_and(|e| e.recurring) {
                // One instance of a repeating event: the server has no
                // "just this one" the mirror could express, so it stays.
                self.sel_event = Some(idx);
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
    /// button font and the control label colour.
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
        let bold = TextAttrs { italic: false, weight: Some(700) };
        self.paint_header_btn(pc, g.prev_btn, HeaderBtn::Prev, Some("chevron-left"), "‹");
        self.paint_header_btn(pc, g.next_btn, HeaderBtn::Next, Some("chevron-right"), "›");
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
                TextAttrs { italic: false, weight: Some(600) }, Self::boxed(num, AlignH::Center));

            // Event chips: dot + clipped title, then a "+N" overflow line.
            if let Some(events) = self.events.get(&date) {
                let line_h = 15.0;
                let avail = ((cell.height - 26.0) / line_h).max(0.0) as usize;
                let shown = if avail >= events.len() { events.len() } else { avail.saturating_sub(1) };
                pc.clip(cell, |pc| {
                    let mut y = cell.y + 24.0;
                    for e in events.iter().take(shown) {
                        let dot = if e.source.is_some() { SYNC_DOT } else { EVENT_DOT };
                        pc.circle(cell.x + 9.0, y + 6.0, 2.5, dot);
                        let alpha = if in_month { TEXT } else { TEXT_DIM };
                        pc.text_with(e.title.clone(), cell.x + 15.0, y, 10.0, alpha, None,
                            Some([cell.x, cell.y, cell.x + cell.width - 4.0, cell.y + cell.height]));
                        y += line_h;
                    }
                    if events.len() > shown {
                        pc.text_with(format!("+{} more", events.len() - shown), cell.x + 15.0, y,
                            10.0, TEXT_FAINT, None, None);
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
        pc.text(heading, g.sidebar.x + pad, head_y, 14.0, color);
        if self.selected == self.today {
            // The heading block's second line (a line advance, not a gap).
            pc.text("today", g.sidebar.x + pad, head_y + 20.0, 10.5, TEXT_FAINT);
        }

        let events = self.events.get(&self.selected).map(Vec::as_slice).unwrap_or(&[]);
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
        pc.clip(g.list, |pc| {
            if events.is_empty() {
                pc.text_with("No events", g.sidebar.x + pad, g.list.y + 9.0, size, TEXT_FAINT,
                    Some(font.clone()), None);
            }
            for (i, e) in events.iter().enumerate() {
                let row = self.sidebar_row(g, i);
                let wash = self.row_wash(i);
                if wash[3] > 0.0 {
                    pc.rounded_rect(row, radius, (true, true, true, true), wash);
                }
                let ty = align_text_y(row.y, row.height, size, 0.0);
                let time = e.time.map_or("——".to_string(), |(h, m)| format!("{h:02}:{m:02}"));
                pc.text_with(time, row.x + 8.0, ty, size,
                    if e.time.is_some() { TEXT_ACCENT } else { TEXT_FAINT }, Some(font.clone()), None);
                pc.text_with(e.title.clone(), row.x + 8.0 + time_w + 12.0, ty, size, title_color,
                    Some(font.clone()), Some([row.x, row.y, row.x + row.width - 4.0, row.y + row.height]));
            }
        });

        // Bottom strip: the input field while typing, else the key hints.
        let strip = g.strip;
        if let Some(buffer) = &self.input {
            self.paint_input(pc, strip, buffer);
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
        pc.quad(Rect { x: x + advance, y: text_y + (size - caret_h) / 2.0, width: 1.5, height: caret_h },
            [0.80, 0.80, 0.85, 1.0]);
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

    fn new(_qh: &QueueHandle<EngineState<Self>>, _sender: calloop::channel::Sender<Self::Message>) -> Self {
        let today = Local::now().date_naive();
        Self {
            events: load_events(),
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
        }
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
        }
    }

    fn handle_resize(&mut self, width: f32, height: f32, _scale: f64) {
        self.win = (width, height);
    }

    fn handle_pointer_move(&mut self, pos: LogicalPosition, needs_rebuild: &mut bool) {
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
        if button != MouseButton::Left {
            return None;
        }
        if state != ElementState::Pressed {
            if self.pressed_btn.take().is_some() {
                *needs_rebuild = true;
            }
            return None;
        }
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
            self.select(date);
        } else if g.sidebar.contains(x, y) {
            self.sel_event = self.row_at(&g, x, y);
        } else {
            return None;
        }
        *needs_rebuild = true;
        None
    }

    fn handle_mouse_wheel(&mut self, delta: &MouseScrollDelta, pos: LogicalPosition, needs_rebuild: &mut bool) {
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
        let mut pc = PaintCtx::new();
        // The standard root plate (cce-ui PlateSpec::window).
        pc.root_plate(size.width, size.height);
        self.paint_header(&mut pc, &g);
        self.paint_grid(&mut pc, &g);
        self.paint_sidebar(&mut pc, &g);
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
