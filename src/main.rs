//! cce-calendar — month-view calendar with per-day events.
//!
//! A 6×7 month grid on the left, a day pane on the right. Events live in
//! `$XDG_DATA_HOME/cce/calendar/events.json` (one flat list of
//! date/time/title records) and are saved on every mutation. Records with a
//! `source` are mirrored from a remote calendar by `cce-calendar-sync`
//! (blue dot); they can be deleted here, but the next sync tick restores
//! them — the server owns them.
//!
//! Keys: arrows move the selected day · PageUp/PageDown month · [/] year ·
//! t/Home today · n/Enter new event (a leading `HH:MM` token sets the
//! time) · d/Delete remove the clicked event · q quit. The wheel flips
//! months over the grid and scrolls the event list over the pane.
//!
//! Config (`~/.config/cce/cce-calendar/config.kdl`): `week-start "sunday"`
//! (default monday).

use std::collections::BTreeMap;

use cce_calendar::{load_records, save_records, EventRecord};
use chrono::{Datelike, Days, Local, NaiveDate, Weekday};
use wayland_client::QueueHandle;

use cce_ui::engine::{Application, EngineState, LogicalPosition, LogicalSize, WindowSettings};
use cce_ui::scene::layout::Rect;
use cce_ui::scene::paint::{AlignH, AlignV, DisplayList, PaintCtx, TextAttrs, TextLayout};
use cce_ui::widget::scroll_motion::{current_scroll_phase, Bounds, ScrollMotion, ScrollPhase};
use cce_ui::widget::{ElementState, Key, KeyEvent, MouseButton, MouseScrollDelta, NamedKey};

const HEADER_H: f32 = 46.0;
const WEEKDAY_H: f32 = 24.0;
const SIDEBAR_W: f32 = 300.0;
const PAD: f32 = 12.0;
const ROW_H: f32 = 36.0;
const INPUT_H: f32 = 40.0;
/// Trackpad travel per month step over the grid (a wheel notch is one step).
const MONTH_STEP_PX: f32 = 48.0;

const BG: [f32; 4] = [0.075, 0.08, 0.09, 1.0];
const BG_OTHER_MONTH: [f32; 4] = [0.06, 0.064, 0.072, 1.0];
const SIDEBAR_BG: [f32; 4] = [0.10, 0.105, 0.12, 1.0];
const GRID_LINE: [f32; 4] = [1.0, 1.0, 1.0, 0.06];
const ACCENT: [f32; 4] = [0.22, 0.42, 0.85, 1.0];
const EVENT_DOT: [f32; 4] = [0.95, 0.72, 0.30, 1.0];
/// Dot for events mirrored from a remote calendar (`source` set).
const SYNC_DOT: [f32; 4] = [0.42, 0.68, 0.95, 1.0];
const ROW_SEL: [f32; 4] = [1.0, 1.0, 1.0, 0.08];
const TEXT: [u8; 3] = [225, 228, 232];
const TEXT_DIM: [u8; 3] = [140, 145, 152];
const TEXT_FAINT: [u8; 3] = [95, 100, 108];
const TEXT_ACCENT: [u8; 3] = [120, 165, 255];

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
    Some(Event { time, title, uid: None, source: None })
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
    prev_btn: Rect,
    next_btn: Rect,
    today_btn: Rect,
    grid: Rect,
    cell_w: f32,
    cell_h: f32,
    sidebar: Rect,
    /// Date of the grid's top-left cell (always a 42-day window).
    first_day: NaiveDate,
}

fn hit(r: &Rect, x: f32, y: f32) -> bool {
    x >= r.x && x < r.x + r.width && y >= r.y && y < r.y + r.height
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
}

impl CalendarApp {
    fn geom(&self) -> Geom {
        let (w, h) = self.win;
        let sidebar_w = SIDEBAR_W.min(w * 0.4);
        let grid_w = w - sidebar_w;
        let grid = Rect {
            x: 0.0,
            y: HEADER_H + WEEKDAY_H,
            width: grid_w,
            height: h - HEADER_H - WEEKDAY_H,
        };
        let btn = |x: f32| Rect { x, y: (HEADER_H - 28.0) / 2.0, width: 28.0, height: 28.0 };
        let first_of_month = NaiveDate::from_ymd_opt(self.view.0, self.view.1, 1)
            .unwrap_or(self.today);
        let back = weekday_offset(first_of_month.weekday(), self.week_start);
        let first_day = first_of_month
            .checked_sub_days(Days::new(back as u64))
            .unwrap_or(first_of_month);
        Geom {
            prev_btn: btn(PAD),
            next_btn: btn(PAD + 28.0 + 190.0),
            today_btn: Rect {
                x: grid_w - PAD - 64.0,
                y: (HEADER_H - 24.0) / 2.0,
                width: 64.0,
                height: 24.0,
            },
            grid,
            cell_w: grid.width / 7.0,
            cell_h: grid.height / 6.0,
            sidebar: Rect { x: grid_w, y: 0.0, width: sidebar_w, height: h },
            first_day,
        }
    }

    fn day_at(&self, g: &Geom, x: f32, y: f32) -> Option<NaiveDate> {
        if !hit(&g.grid, x, y) {
            return None;
        }
        let col = ((x - g.grid.x) / g.cell_w) as u64;
        let row = ((y - g.grid.y) / g.cell_h) as u64;
        g.first_day.checked_add_days(Days::new(row.min(5) * 7 + col.min(6)))
    }

    fn select(&mut self, date: NaiveDate) {
        self.selected = date;
        self.sel_event = None;
        self.sidebar_scroll = 0.0;
        if (date.year(), date.month()) != self.view {
            self.view = (date.year(), date.month());
        }
    }

    /// How far the selected day's event rows overflow the sidebar's list area.
    fn sidebar_overflow(&self, g: &Geom) -> f32 {
        let events = self.events.get(&self.selected).map_or(0, Vec::len);
        (events as f32 * ROW_H - (g.sidebar.height - 58.0 - INPUT_H - 8.0)).max(0.0)
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
                })
            })
            .collect();
        self.status = save_records(&records).err().map(|e| format!("save failed: {e}"));
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

    fn paint_header(&self, pc: &mut PaintCtx, g: &Geom) {
        let bold = TextAttrs { italic: false, weight: Some(700) };
        for (rect, glyph) in [(&g.prev_btn, "‹"), (&g.next_btn, "›")] {
            pc.rounded_rect(*rect, 6.0, (true, true, true, true), [1.0, 1.0, 1.0, 0.07]);
            pc.text_boxed(glyph, rect.x, rect.y - 1.0, 18.0, TEXT, None, None, bold,
                Self::boxed(*rect, AlignH::Center));
        }
        let title = Rect {
            x: g.prev_btn.x + g.prev_btn.width,
            y: 0.0,
            width: g.next_btn.x - (g.prev_btn.x + g.prev_btn.width),
            height: HEADER_H,
        };
        let label = format!("{} {}", MONTHS[self.view.1 as usize - 1], self.view.0);
        pc.text_boxed(label, title.x, title.y, 16.0, TEXT, None, None, bold,
            Self::boxed(title, AlignH::Center));
        pc.rounded_rect(g.today_btn, 6.0, (true, true, true, true), [1.0, 1.0, 1.0, 0.07]);
        pc.text_boxed("Today", g.today_btn.x, g.today_btn.y, 12.0, TEXT_DIM, None, None,
            TextAttrs::default(), Self::boxed(g.today_btn, AlignH::Center));
    }

    fn paint_grid(&self, pc: &mut PaintCtx, g: &Geom) {
        for (i, name) in WEEKDAYS.iter().cycle()
            .skip(self.week_start.num_days_from_monday() as usize)
            .take(7)
            .enumerate()
        {
            let rect = Rect {
                x: g.grid.x + i as f32 * g.cell_w,
                y: HEADER_H,
                width: g.cell_w,
                height: WEEKDAY_H,
            };
            pc.text_boxed(*name, rect.x, rect.y, 11.0, TEXT_FAINT, None, None,
                TextAttrs::default(), Self::boxed(rect, AlignH::Center));
        }

        for i in 0..42u64 {
            let Some(date) = g.first_day.checked_add_days(Days::new(i)) else { continue };
            let (row, col) = (i / 7, i % 7);
            let cell = Rect {
                x: g.grid.x + col as f32 * g.cell_w,
                y: g.grid.y + row as f32 * g.cell_h,
                width: g.cell_w,
                height: g.cell_h,
            };
            let in_month = (date.year(), date.month()) == self.view;
            if !in_month {
                pc.quad(cell, BG_OTHER_MONTH);
            }
            if date == self.selected {
                let r = Rect {
                    x: cell.x + 1.0,
                    y: cell.y + 1.0,
                    width: cell.width - 2.0,
                    height: cell.height - 2.0,
                };
                pc.rounded_rect(r, 5.0, (true, true, true, true), [ACCENT[0], ACCENT[1], ACCENT[2], 0.55]);
                let inner = Rect { x: r.x + 2.0, y: r.y + 2.0, width: r.width - 4.0, height: r.height - 4.0 };
                pc.rounded_rect(inner, 4.0, (true, true, true, true), if in_month { BG } else { BG_OTHER_MONTH });
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

        for col in 1..7 {
            let x = g.grid.x + col as f32 * g.cell_w;
            pc.quad(Rect { x, y: g.grid.y, width: 1.0, height: g.grid.height }, GRID_LINE);
        }
        for row in 0..6 {
            let y = g.grid.y + row as f32 * g.cell_h;
            pc.quad(Rect { x: g.grid.x, y, width: g.grid.width, height: 1.0 }, GRID_LINE);
        }
    }

    /// Sidebar event-row rects, matching `paint_sidebar` (shared with hit-testing).
    fn sidebar_row(&self, g: &Geom, idx: usize) -> Rect {
        Rect {
            x: g.sidebar.x + PAD,
            y: 64.0 + idx as f32 * ROW_H - self.sidebar_scroll,
            width: g.sidebar.width - 2.0 * PAD,
            height: ROW_H - 4.0,
        }
    }

    fn paint_sidebar(&self, pc: &mut PaintCtx, g: &Geom) {
        pc.quad(g.sidebar, SIDEBAR_BG);
        pc.quad(Rect { x: g.sidebar.x, y: 0.0, width: 1.0, height: g.sidebar.height }, GRID_LINE);

        let heading = format!(
            "{}, {} {}",
            WEEKDAYS[self.selected.weekday().num_days_from_monday() as usize],
            MONTHS[self.selected.month() as usize - 1],
            self.selected.day()
        );
        let color = if self.selected == self.today { TEXT_ACCENT } else { TEXT };
        pc.text(heading, g.sidebar.x + PAD, 18.0, 14.0, color);
        if self.selected == self.today {
            pc.text("today", g.sidebar.x + PAD, 38.0, 10.5, TEXT_FAINT);
        }

        let events = self.events.get(&self.selected).map(Vec::as_slice).unwrap_or(&[]);
        let bottom_h = INPUT_H + 8.0;
        let list = Rect {
            x: g.sidebar.x,
            y: 58.0,
            width: g.sidebar.width,
            height: g.sidebar.height - 58.0 - bottom_h,
        };
        pc.clip(list, |pc| {
            if events.is_empty() {
                pc.text("No events", g.sidebar.x + PAD, 70.0, 12.0, TEXT_FAINT);
            }
            for (i, e) in events.iter().enumerate() {
                let row = self.sidebar_row(g, i);
                if self.sel_event == Some(i) {
                    pc.rounded_rect(row, 5.0, (true, true, true, true), ROW_SEL);
                }
                let time = e.time.map_or("——".to_string(), |(h, m)| format!("{h:02}:{m:02}"));
                pc.text(time, row.x + 8.0, row.y + 9.0, 11.0,
                    if e.time.is_some() { TEXT_ACCENT } else { TEXT_FAINT });
                pc.text_with(e.title.clone(), row.x + 54.0, row.y + 8.0, 12.5, TEXT, None,
                    Some([row.x, row.y, row.x + row.width - 4.0, row.y + row.height]));
            }
        });

        // Bottom strip: the input field while typing, else the key hints.
        let strip = Rect {
            x: g.sidebar.x + PAD,
            y: g.sidebar.height - bottom_h,
            width: g.sidebar.width - 2.0 * PAD,
            height: INPUT_H - 8.0,
        };
        if let Some(buffer) = &self.input {
            pc.rounded_rect(strip, 6.0, (true, true, true, true), [0.0, 0.0, 0.0, 0.35]);
            pc.rounded_rect(
                Rect { x: strip.x, y: strip.y + strip.height - 2.0, width: strip.width, height: 2.0 },
                1.0, (true, true, true, true), ACCENT);
            let shown = if buffer.is_empty() { "HH:MM title".to_string() } else { format!("{buffer}▏") };
            let color = if buffer.is_empty() { TEXT_FAINT } else { TEXT };
            pc.text_with(shown, strip.x + 8.0, strip.y + 9.0, 12.5, color, None,
                Some([strip.x, strip.y, strip.x + strip.width - 8.0, strip.y + strip.height]));
        } else {
            let hint = if let Some(err) = &self.status {
                err.clone()
            } else if self.sel_event.is_some() {
                "d delete · n new · t today".to_string()
            } else {
                "n new · t today · pgup/pgdn month".to_string()
            };
            let color = if self.status.is_some() { [230, 130, 120] } else { TEXT_FAINT };
            pc.text(hint, strip.x, strip.y + 12.0, 10.5, color);
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

    fn tick(&mut self, dt: f32, needs_rebuild: &mut bool) {
        let now = Local::now().date_naive();
        if now != self.today {
            self.today = now;
            *needs_rebuild = true;
        }
        if self.tick_sidebar_scroll(dt) {
            *needs_rebuild = true;
        }
    }

    fn handle_resize(&mut self, width: f32, height: f32, _scale: f64) {
        self.win = (width, height);
    }

    fn handle_pointer_move(&mut self, _pos: LogicalPosition, _needs_rebuild: &mut bool) {}

    fn handle_mouse_input(
        &mut self,
        button: MouseButton,
        state: ElementState,
        pos: LogicalPosition,
        needs_rebuild: &mut bool,
    ) -> Option<Self::Message> {
        if button != MouseButton::Left || state != ElementState::Pressed {
            return None;
        }
        let g = self.geom();
        let (x, y) = (pos.x, pos.y);
        if hit(&g.prev_btn, x, y) {
            self.shift_months(-1);
        } else if hit(&g.next_btn, x, y) {
            self.shift_months(1);
        } else if hit(&g.today_btn, x, y) {
            self.select(self.today);
        } else if let Some(date) = self.day_at(&g, x, y) {
            self.select(date);
        } else if hit(&g.sidebar, x, y) {
            let events = self.events.get(&self.selected).map_or(0, Vec::len);
            self.sel_event = (0..events).find(|&i| hit(&self.sidebar_row(&g, i), x, y));
        } else {
            return None;
        }
        *needs_rebuild = true;
        None
    }

    fn handle_mouse_wheel(&mut self, delta: &MouseScrollDelta, pos: LogicalPosition, needs_rebuild: &mut bool) {
        let g = self.geom();
        if hit(&g.sidebar, pos.x, pos.y) {
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
        pc.quad(Rect { x: 0.0, y: 0.0, width: size.width, height: size.height }, BG);
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
