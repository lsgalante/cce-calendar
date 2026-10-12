//! MCP tools, so agents can read and change the calendar through the
//! running app (cce-ui's tools-only server, as cce-notes uses it):
//!
//! ```sh
//! claude mcp add --transport http cce-calendar http://127.0.0.1:3004/mcp
//! ```
//!
//! The first window serves it; a second finds the port taken and logs it.
//! `CCE_CALENDAR_MCP_PORT` moves the port, for a second instance in a shadow.
//! Calls run on the app's event loop and change the same `events` the
//! window draws, saved by the same `save`, so an agent's edit and the
//! user's cannot overwrite each other. The sync pushes them upstream: a
//! few seconds after the last write the app starts a sync run rather than
//! leaving it to the next timer tick.
//!
//! An event's id is its `uid`. The app mints one for every event it creates
//! (`new_uid`), and the sync reuses it as the iCalendar UID, so on iCloud
//! the id survives the push. Google assigns its own, so an event pushed
//! there changes id once. Instances of a repeating event, and the per-day
//! records of an all-day event spanning several days, share one uid and
//! are read-only here, as they are in the window.

use std::time::{Duration, Instant};

use cce_calendar::{new_uid, sync_window};
use cce_ui::mcp::{McpTool, McpToolCall};
use chrono::{Days, Local, NaiveDate};
use serde_json::{json, Value};

use crate::{file_mtime, parse_time, sort_events, CalendarApp, Event, Message};

const DEFAULT_PORT: u16 = 3004;
/// Quiet time after an agent's last write before the sync is started, so a
/// batch of edits goes up in one run.
const SYNC_DEBOUNCE: Duration = Duration::from_secs(3);
/// The most events `list_events` returns.
const LIST_CAP: usize = 500;

pub fn start(sender: calloop::channel::Sender<Message>) {
    let port = std::env::var("CCE_CALENDAR_MCP_PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(DEFAULT_PORT);
    cce_ui::mcp::start_mcp_server("cce-calendar", port, tools(), sender, Message::Mcp);
}

/// Start a sync run without waiting for it. A run already in progress
/// absorbs the request; edits it missed are merged into what it writes and
/// pushed on the next run.
pub fn start_sync() {
    std::thread::spawn(|| {
        let status = std::process::Command::new("systemctl")
            .args(["--user", "start", "--no-block", "cce-calendar-sync.service"])
            .status();
        match status {
            Ok(s) if s.success() => log::info!("started cce-calendar-sync for an agent's edits"),
            Ok(s) => log::warn!("systemctl start cce-calendar-sync: {s}"),
            Err(e) => log::warn!("systemctl start cce-calendar-sync: {e}"),
        }
    });
}

fn tools() -> Vec<McpTool> {
    let tool = |name: &str, description: &str, input_schema: Value| McpTool {
        name: name.into(),
        description: description.into(),
        input_schema,
    };
    let date = |what: &str| json!({ "type": "string", "description": format!("{what}, YYYY-MM-DD") });
    let time = json!({ "type": ["string", "null"], "description": "Start time, 24-hour HH:MM (local time). Events have no end time; the sync creates timed events an hour long." });
    let id = json!({ "type": "string", "description": "The event's id, from list_events or add_event" });
    vec![
        tool(
            "list_events",
            "Events from `from` to `to` inclusive (default: today through 30 days on), timed before untimed within a day. \
             Each has an id, date, time (null = all day), title, its calendar (`icloud:<email>` / `google:<email>`, or null \
             for an event not yet pushed by the sync) and `read_only` with the reason when it cannot be changed here.",
            json!({ "type": "object", "properties": {
                "from": date("First day"),
                "to": date("Last day"),
                "query": { "type": "string", "description": "Only events whose title contains this (case-insensitive)" }
            } }),
        ),
        tool(
            "add_event",
            "Create an event. It is pushed to the default calendar (see `calendars`) by the next sync run, which the app \
             starts a few seconds after the last edit.",
            json!({ "type": "object", "properties": {
                "date": date("Day"),
                "time": time,
                "title": { "type": "string" }
            }, "required": ["date", "title"] }),
        ),
        tool(
            "update_event",
            "Change an event's date, time or title; omitted fields stay as they are, and `time: null` makes it all-day. \
             A synced event is changed on its server by the next sync run. Repeating events are read-only.",
            json!({ "type": "object", "properties": {
                "id": id,
                "date": date("New day"),
                "time": time,
                "title": { "type": "string" }
            }, "required": ["id"] }),
        ),
        tool(
            "delete_event",
            "Delete an event. A synced event is deleted on its server by the next sync run. Repeating events are read-only.",
            json!({ "type": "object", "properties": { "id": id }, "required": ["id"] }),
        ),
        tool(
            "calendars",
            "The synced accounts, which one new events are pushed to, the accounts hidden in the window, and the date \
             range the sync mirrors (events outside it stay on the server but drop out of the app).",
            json!({ "type": "object", "properties": {} }),
        ),
        tool(
            "show_day",
            "Select a day in the calendar window, flipping to its month.",
            json!({ "type": "object", "properties": { "date": date("Day") }, "required": ["date"] }),
        ),
    ]
}

fn str_arg<'a>(args: &'a Value, key: &str) -> Result<&'a str, String> {
    args.get(key).and_then(Value::as_str).ok_or_else(|| format!("missing string argument `{key}`"))
}

fn parse_date(s: &str) -> Result<NaiveDate, String> {
    NaiveDate::parse_from_str(s.trim(), "%Y-%m-%d").map_err(|_| format!("not a YYYY-MM-DD date: {s:?}"))
}

fn date_arg(args: &Value, key: &str) -> Result<Option<NaiveDate>, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => parse_date(s).map(Some),
        Some(v) => Err(format!("`{key}` must be a YYYY-MM-DD string, got {v}")),
    }
}

/// `time`: absent → `None` (leave it); null or "" → `Some(None)` (all day).
fn time_arg(args: &Value) -> Result<Option<Option<(u32, u32)>>, String> {
    match args.get("time") {
        None => Ok(None),
        Some(Value::Null) => Ok(Some(None)),
        Some(Value::String(s)) if s.trim().is_empty() => Ok(Some(None)),
        Some(Value::String(s)) => {
            parse_time(s.trim()).map(|t| Some(Some(t))).ok_or_else(|| format!("not a 24-hour HH:MM time: {s:?}"))
        }
        Some(v) => Err(format!("`time` must be an HH:MM string or null, got {v}")),
    }
}

fn title_arg(args: &Value) -> Result<Option<String>, String> {
    match args.get("title") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => {
            let t = s.split_whitespace().collect::<Vec<_>>().join(" ");
            if t.is_empty() {
                Err("the title is empty".into())
            } else {
                Ok(Some(t))
            }
        }
        Some(v) => Err(format!("`title` must be a string, got {v}")),
    }
}

fn event_json(date: NaiveDate, e: &Event, read_only: Option<&str>) -> Value {
    json!({
        "id": e.uid,
        "date": date.to_string(),
        "time": e.time.map(|(h, m)| format!("{h:02}:{m:02}")),
        "title": e.title,
        "calendar": e.source,
        "read_only": read_only,
    })
}

/// Where an event the dispatcher acts on lives.
struct Found {
    date: NaiveDate,
    idx: usize,
}

impl CalendarApp {
    pub(crate) fn mcp_call(&mut self, call: McpToolCall) {
        let result = self.mcp_dispatch(&call.name, &call.arguments);
        let _ = call.reply.send(result);
    }

    fn mcp_dispatch(&mut self, name: &str, args: &Value) -> Result<Value, String> {
        // The once-a-second watch may not have seen the sync's latest write
        // yet; an edit made on stale state would undo it at the next save.
        // (Not mid-typing, which `tick` also leaves alone.)
        if self.input.is_none() && file_mtime() != self.file_mtime {
            self.reload();
        }
        self.ensure_ids();
        match name {
            "list_events" => self.mcp_list(args),
            "add_event" => {
                let date = date_arg(args, "date")?.ok_or("missing `date`")?;
                let title = title_arg(args)?.ok_or("missing `title`")?;
                let time = time_arg(args)?.flatten();
                let event = Event { time, title, uid: Some(new_uid()), source: None, recurring: false };
                let out = event_json(date, &event, None);
                let day = self.events.entry(date).or_default();
                day.push(event);
                sort_events(day);
                self.mcp_saved(date, out)
            }
            "update_event" => {
                let found = self.find_editable(str_arg(args, "id")?)?;
                let (date, time, title) = (date_arg(args, "date")?, time_arg(args)?, title_arg(args)?);
                let day = self.events.get_mut(&found.date).expect("found");
                let mut event = day.remove(found.idx);
                if day.is_empty() {
                    self.events.remove(&found.date);
                }
                if let Some(t) = time {
                    event.time = t;
                }
                if let Some(t) = title {
                    event.title = t;
                }
                let to = date.unwrap_or(found.date);
                let out = event_json(to, &event, None);
                let dest = self.events.entry(to).or_default();
                dest.push(event);
                sort_events(dest);
                self.mcp_saved(to, out)
            }
            "delete_event" => {
                let found = self.find_editable(str_arg(args, "id")?)?;
                let day = self.events.get_mut(&found.date).expect("found");
                let event = day.remove(found.idx);
                if day.is_empty() {
                    self.events.remove(&found.date);
                }
                let out = json!({ "deleted": event_json(found.date, &event, None) });
                self.mcp_saved(found.date, out)
            }
            "calendars" => {
                let (from, to) = sync_window(Local::now().date_naive());
                Ok(json!({
                    "accounts": self.accounts.iter().map(|a| a.source()).collect::<Vec<_>>(),
                    "new_events_go_to": self.push.as_ref().map_or_else(
                        || json!("the sync's default: the first iCloud account, else the first Google one"),
                        |p| json!({ "kind": p.kind, "account": p.account, "calendar": p.calendar }),
                    ),
                    "hidden_in_window": self.hidden.iter().collect::<Vec<_>>(),
                    "sync_window": { "from": from.to_string(), "to": to.to_string() },
                }))
            }
            "show_day" => {
                let date = date_arg(args, "date")?.ok_or("missing `date`")?;
                self.select(date);
                Ok(json!({ "shown": date.to_string() }))
            }
            _ => Err(format!("unknown tool `{name}`")),
        }
    }

    fn mcp_list(&self, args: &Value) -> Result<Value, String> {
        let from = date_arg(args, "from")?.unwrap_or(self.today);
        let to = match date_arg(args, "to")? {
            Some(d) => d,
            None => from.checked_add_days(Days::new(30)).ok_or("date overflow")?,
        };
        if to < from {
            return Err(format!("`to` ({to}) is before `from` ({from})"));
        }
        let query = args.get("query").and_then(Value::as_str).map(str::to_lowercase);
        let mut out = Vec::new();
        for (&date, day) in self.events.range(from..=to) {
            for e in day {
                if query.as_ref().is_some_and(|q| !e.title.to_lowercase().contains(q)) {
                    continue;
                }
                out.push(event_json(date, e, self.read_only_reason(e)));
            }
        }
        let total = out.len();
        out.truncate(LIST_CAP);
        let mut result = json!({ "from": from.to_string(), "to": to.to_string(), "events": out });
        if total > LIST_CAP {
            result["truncated"] = json!(format!("{total} events; showing the first {LIST_CAP} — narrow the range"));
        }
        Ok(result)
    }

    /// Why an event cannot be changed here, if it cannot.
    fn read_only_reason(&self, e: &Event) -> Option<&'static str> {
        if e.recurring {
            return Some("an instance of a repeating event; change it in the calendar it comes from");
        }
        let uid = e.uid.as_deref()?;
        let records = self.events.values().flatten().filter(|o| o.uid.as_deref() == Some(uid)).count();
        (records > 1).then_some("spans several days; change it in the calendar it comes from")
    }

    fn find_editable(&self, id: &str) -> Result<Found, String> {
        let mut found = self.events.iter().flat_map(|(&date, day)| {
            day.iter().enumerate().filter(|(_, e)| e.uid.as_deref() == Some(id)).map(move |(idx, e)| (date, idx, e))
        });
        let Some((date, idx, e)) = found.next() else {
            return Err(format!(
                "no event with id {id:?}. An event pushed to Google takes Google's id at the push, and one \
                 moved outside the sync window drops out — list_events again"
            ));
        };
        if let Some(why) = self.read_only_reason(e) {
            return Err(format!("{:?} is read-only: {why}", e.title));
        }
        Ok(Found { date, idx })
    }

    /// Give every event that has none (typed before the app minted uids) a
    /// uid, so each one an agent sees can be named. Persisted at once:
    /// an id that a reload re-minted would point at nothing.
    fn ensure_ids(&mut self) {
        let mut minted = false;
        for e in self.events.values_mut().flatten().filter(|e| e.uid.is_none()) {
            e.uid = Some(new_uid());
            minted = true;
        }
        if minted {
            self.save();
        }
    }

    /// Persist an agent's edit, show its day, schedule the push, and answer.
    fn mcp_saved(&mut self, date: NaiveDate, mut out: Value) -> Result<Value, String> {
        // Indices into the day lists have moved.
        self.picker = None;
        self.drag = None;
        self.sel_event = None;
        self.save();
        if let Some(e) = self.status.clone() {
            return Err(e);
        }
        self.select(date);
        self.sync_kick_at = Some(Instant::now() + SYNC_DEBOUNCE);
        let (from, to) = sync_window(Local::now().date_naive());
        if !(from..=to).contains(&date) {
            out["note"] = json!(format!(
                "{date} is outside the range the sync mirrors ({from} to {to}): the event is kept on the server \
                 but will drop out of this app after the next sync"
            ));
        }
        Ok(out)
    }
}
