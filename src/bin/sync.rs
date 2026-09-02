//! `cce-calendar-sync` — read-only CalDAV mirror of iCloud calendars into
//! cce-calendar's `events.json`.
//!
//! Accounts come from the same `accounts.json` cce-mail reads (owned by
//! cce-system-interface), passwords from the same `cce-mail` keyring service —
//! an Apple app-specific password is valid for CalDAV as well as IMAP, so the
//! credential that fetches mail fetches the calendar too. Only iCloud
//! accounts are synced; Google's CalDAV endpoint refuses app passwords and
//! waits on an OAuth scope change (step 3 of the sync plan).
//!
//! Each run replaces exactly the records whose `source` matches the account
//! being synced ("icloud:<email>"); hand-entered events (no `source`) and
//! other accounts' records pass through untouched. A record deleted in the
//! app therefore reappears on the next tick — this mirror is read-only by
//! design, and the server is the source of truth for what it owns.
//!
//! Recurring events are expanded server-side (`<C:expand>`), which also
//! normalizes times to UTC. If a calendar's REPORT rejects expansion, the
//! query is retried plain and RRULE-carrying events are skipped with a log
//! line rather than shown on the wrong day.
//!
//! Usage: `cce-calendar-sync [--dry-run]`. Driven by cce-calendar-sync.timer;
//! harmless to run by hand. Exits nonzero if any account failed (the timer
//! just tries again next tick); other accounts' results are still written.

use std::collections::BTreeSet;
use std::str::FromStr;

use chrono::{DateTime, Days, Local, NaiveDate, NaiveDateTime, TimeZone, Utc};
use cce_calendar::{load_records, save_records, EventRecord};

const CALDAV_ROOT: &str = "https://caldav.icloud.com/";
/// Sync window around today. Wide enough forward that "next spring" plans
/// show up; bounded so the file stays a glanceable flat list.
const PAST_DAYS: u64 = 60;
const FUTURE_DAYS: u64 = 400;
/// An all-day event spanning more than this is almost certainly bad data
/// (a botched DTEND); clamp rather than flood two months of cells.
const MAX_ALLDAY_SPAN: u64 = 62;

const CALDAV_NS: &str = "urn:ietf:params:xml:ns:caldav";

fn main() {
    env_logger::init();
    let dry_run = std::env::args().any(|a| a == "--dry-run");

    let accounts = match icloud_accounts() {
        Ok(a) => a,
        Err(e) => {
            log::error!("cannot read accounts: {e}");
            std::process::exit(1);
        }
    };
    if accounts.is_empty() {
        log::info!("no iCloud accounts in accounts.json; nothing to sync");
        return;
    }

    let today = Local::now().date_naive();
    let window = (
        today.checked_sub_days(Days::new(PAST_DAYS)).unwrap_or(today),
        today.checked_add_days(Days::new(FUTURE_DAYS)).unwrap_or(today),
    );

    let mut failed = false;
    let mut synced: Vec<(String, Vec<EventRecord>)> = Vec::new();
    for acc in &accounts {
        let source = format!("icloud:{}", acc.email);
        match sync_account(acc, window) {
            Ok(records) => {
                log::info!("{}: {} event records", acc.email, records.len());
                synced.push((source, records));
            }
            Err(e) => {
                log::error!("{}: sync failed, keeping existing records: {e}", acc.email);
                failed = true;
            }
        }
    }

    if !synced.is_empty() {
        let existing = match load_records() {
            Ok(r) => r,
            Err(e) => {
                // Refuse to rewrite a file we could not read — that would
                // silently drop every hand-entered event.
                log::error!("events.json unreadable, not writing: {e}");
                std::process::exit(1);
            }
        };
        let merged = merge(existing, &synced);
        if dry_run {
            for (source, records) in &synced {
                for r in records {
                    println!("{source}: {} {} {}", r.date, r.time.as_deref().unwrap_or("-----"), r.title);
                }
            }
            println!("dry run: {} records total after merge, not written", merged.len());
        } else if let Err(e) = save_records(&merged) {
            log::error!("saving events.json failed: {e}");
            failed = true;
        }
    }
    if failed {
        std::process::exit(1);
    }
}

// ── Accounts ──────────────────────────────────────────────────────────────

struct Account {
    email: String,
    password: String,
}

/// The subset of cce-mail's AccountInfo this helper needs. Unknown fields
/// are ignored, so the two readers cannot drift apart.
#[derive(serde::Deserialize)]
struct AccountOnDisk {
    email: String,
    #[serde(default)]
    imap: String,
    #[serde(default)]
    password: String,
}

fn icloud_accounts() -> Result<Vec<Account>, String> {
    let path = cce_ui::config::cce_config_dir().join("accounts.json");
    let text = std::fs::read_to_string(&path)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    let on_disk: Vec<AccountOnDisk> =
        serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;

    let mut out = Vec::new();
    for acc in on_disk {
        if !is_icloud(&acc) {
            continue;
        }
        // Same resolution order as cce-mail: a plaintext on-disk password is
        // still valid pre-migration; an empty one lives in the keyring under
        // the "cce-mail" service. This helper only reads — migration into
        // the keyring stays cce-mail's job.
        let password = if !acc.password.is_empty() {
            acc.password.clone()
        } else {
            match keyring::Entry::new("cce-mail", &acc.email).and_then(|e| e.get_password()) {
                Ok(p) => p,
                Err(e) => {
                    log::warn!("{}: no password available ({e}); skipping", acc.email);
                    continue;
                }
            }
        };
        out.push(Account { email: acc.email, password });
    }
    Ok(out)
}

fn is_icloud(acc: &AccountOnDisk) -> bool {
    let host = acc.imap.split(':').next().unwrap_or("");
    host.ends_with(".mail.me.com")
        || ["@icloud.com", "@me.com", "@mac.com"].iter().any(|d| acc.email.ends_with(d))
}

// ── CalDAV ────────────────────────────────────────────────────────────────

fn sync_account(
    acc: &Account,
    window: (NaiveDate, NaiveDate),
) -> Result<Vec<EventRecord>, String> {
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .build()
        .map_err(|e| e.to_string())?;

    let root = reqwest::Url::parse(CALDAV_ROOT).expect("static url");
    let principal = discover_href(
        &client, acc, &root, "0",
        r#"<?xml version="1.0" encoding="utf-8"?>
<propfind xmlns="DAV:"><prop><current-user-principal/></prop></propfind>"#,
        "current-user-principal",
    )?;
    let home = discover_href(
        &client, acc, &principal, "0",
        r#"<?xml version="1.0" encoding="utf-8"?>
<propfind xmlns="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav"><prop><C:calendar-home-set/></prop></propfind>"#,
        "calendar-home-set",
    )?;
    let calendars = list_event_calendars(&client, acc, &home)?;
    log::info!("{}: {} calendar(s) with events", acc.email, calendars.len());

    let (start, end) = (
        format!("{}T000000Z", window.0.format("%Y%m%d")),
        format!("{}T000000Z", window.1.format("%Y%m%d")),
    );
    let source = format!("icloud:{}", acc.email);
    let mut seen = BTreeSet::new();
    let mut records = Vec::new();
    for (cal_url, cal_name) in &calendars {
        let events = fetch_events(&client, acc, cal_url, &start, &end)
            .map_err(|e| format!("calendar {cal_name}: {e}"))?;
        for ev in events {
            event_to_records(&ev, window, &source, &mut seen, &mut records);
        }
    }
    Ok(records)
}

fn dav_request(
    client: &reqwest::blocking::Client,
    acc: &Account,
    method: &str,
    url: &reqwest::Url,
    depth: &str,
    body: &str,
) -> Result<String, String> {
    let resp = client
        .request(
            reqwest::Method::from_bytes(method.as_bytes()).expect("static method"),
            url.clone(),
        )
        .basic_auth(&acc.email, Some(&acc.password))
        .header("Depth", depth)
        .header("Content-Type", "application/xml; charset=utf-8")
        .body(body.to_string())
        .send()
        .map_err(|e| format!("{method} {url}: {e}"))?;
    let status = resp.status();
    let text = resp.text().map_err(|e| e.to_string())?;
    if !status.is_success() {
        // 401 here usually means the app-specific password predates 2FA or
        // was revoked — generating a fresh one on appleid.apple.com fixes it.
        return Err(format!("{method} {url}: HTTP {status}"));
    }
    Ok(text)
}

/// PROPFIND for a single href-valued property (principal, calendar home).
fn discover_href(
    client: &reqwest::blocking::Client,
    acc: &Account,
    url: &reqwest::Url,
    depth: &str,
    body: &str,
    prop: &str,
) -> Result<reqwest::Url, String> {
    let xml = dav_request(client, acc, "PROPFIND", url, depth, body)?;
    let doc = roxmltree::Document::parse(&xml).map_err(|e| format!("bad multistatus: {e}"))?;
    let href = doc
        .descendants()
        .find(|n| n.tag_name().name() == prop)
        .and_then(|n| n.descendants().find(|c| c.tag_name().name() == "href"))
        .and_then(|n| n.text())
        .ok_or_else(|| format!("no {prop} in PROPFIND response"))?;
    url.join(href.trim()).map_err(|e| format!("bad {prop} href {href:?}: {e}"))
}

/// Depth-1 PROPFIND on the calendar home: the child collections that are
/// calendars and hold VEVENTs (Reminders lists are VTODO-only and excluded —
/// they are cce-list's, in a later step).
fn list_event_calendars(
    client: &reqwest::blocking::Client,
    acc: &Account,
    home: &reqwest::Url,
) -> Result<Vec<(reqwest::Url, String)>, String> {
    let body = r#"<?xml version="1.0" encoding="utf-8"?>
<propfind xmlns="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav">
  <prop><resourcetype/><displayname/><C:supported-calendar-component-set/></prop>
</propfind>"#;
    let xml = dav_request(client, acc, "PROPFIND", home, "1", body)?;
    let doc = roxmltree::Document::parse(&xml).map_err(|e| format!("bad multistatus: {e}"))?;

    let mut out = Vec::new();
    for resp in doc.descendants().filter(|n| n.tag_name().name() == "response") {
        let Some(href) = resp
            .children()
            .find(|c| c.tag_name().name() == "href")
            .and_then(|n| n.text())
        else {
            continue;
        };
        let is_calendar = resp.descendants().any(|n| {
            n.tag_name().name() == "calendar" && n.tag_name().namespace() == Some(CALDAV_NS)
        });
        if !is_calendar {
            continue;
        }
        // If the server states the component set, require VEVENT; if the
        // property is absent (404 propstat), assume events.
        let comps: Vec<_> = resp
            .descendants()
            .filter(|n| n.tag_name().name() == "comp")
            .filter_map(|n| n.attribute("name"))
            .collect();
        if !comps.is_empty() && !comps.contains(&"VEVENT") {
            continue;
        }
        let name = resp
            .descendants()
            .find(|n| n.tag_name().name() == "displayname")
            .and_then(|n| n.text())
            .unwrap_or(href)
            .to_string();
        let url = home
            .join(href.trim())
            .map_err(|e| format!("bad calendar href {href:?}: {e}"))?;
        if url.path().trim_end_matches('/') == home.path().trim_end_matches('/') {
            continue; // the home collection lists itself first
        }
        out.push((url, name));
    }
    Ok(out)
}

fn calendar_query(start: &str, end: &str, expand: bool) -> String {
    let data = if expand {
        format!(r#"<C:calendar-data><C:expand start="{start}" end="{end}"/></C:calendar-data>"#)
    } else {
        "<C:calendar-data/>".to_string()
    };
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<C:calendar-query xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav">
  <D:prop>{data}</D:prop>
  <C:filter><C:comp-filter name="VCALENDAR"><C:comp-filter name="VEVENT">
    <C:time-range start="{start}" end="{end}"/>
  </C:comp-filter></C:comp-filter></C:filter>
</C:calendar-query>"#
    )
}

fn fetch_events(
    client: &reqwest::blocking::Client,
    acc: &Account,
    cal: &reqwest::Url,
    start: &str,
    end: &str,
) -> Result<Vec<VEvent>, String> {
    // Server-side expansion first: recurrences come back as concrete
    // instances in UTC, so no RRULE or VTIMEZONE handling is needed here.
    let (xml, expanded) =
        match dav_request(client, acc, "REPORT", cal, "1", &calendar_query(start, end, true)) {
            Ok(xml) => (xml, true),
            Err(e) => {
                log::warn!("expand REPORT failed ({e}); retrying without expansion");
                (dav_request(client, acc, "REPORT", cal, "1", &calendar_query(start, end, false))?, false)
            }
        };
    let doc = roxmltree::Document::parse(&xml).map_err(|e| format!("bad multistatus: {e}"))?;
    let mut events = Vec::new();
    let mut skipped_rrule = 0usize;
    for node in doc.descendants().filter(|n| n.tag_name().name() == "calendar-data") {
        let Some(ics) = node.text() else { continue };
        for ev in parse_ics_events(ics) {
            if !expanded && ev.has_rrule {
                skipped_rrule += 1;
                continue;
            }
            events.push(ev);
        }
    }
    if skipped_rrule > 0 {
        log::warn!("{skipped_rrule} recurring event(s) skipped (server refused expansion)");
    }
    Ok(events)
}

// ── iCalendar parsing (the few fields this mirror needs) ──────────────────

#[derive(Debug, Default)]
struct VEvent {
    uid: String,
    summary: String,
    dtstart: Option<DtValue>,
    dtend: Option<DtValue>,
    cancelled: bool,
    has_rrule: bool,
}

#[derive(Debug, Clone)]
enum DtValue {
    /// All-day (VALUE=DATE).
    Date(NaiveDate),
    /// Zulu-suffixed date-time (what `<C:expand>` yields).
    Utc(DateTime<Utc>),
    /// Floating or TZID-qualified local time.
    Zoned(NaiveDateTime, Option<String>),
}

/// RFC 5545 line unfolding: a CRLF (or LF) followed by a space or tab
/// continues the previous line.
fn unfold(ics: &str) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    for raw in ics.split('\n') {
        let raw = raw.strip_suffix('\r').unwrap_or(raw);
        if let Some(rest) = raw.strip_prefix(' ').or_else(|| raw.strip_prefix('\t')) {
            if let Some(last) = lines.last_mut() {
                last.push_str(rest);
                continue;
            }
        }
        lines.push(raw.to_string());
    }
    lines
}

/// Split a content line at the first ':' outside double quotes.
fn split_content_line(line: &str) -> Option<(&str, &str)> {
    let mut in_quotes = false;
    for (i, c) in line.char_indices() {
        match c {
            '"' => in_quotes = !in_quotes,
            ':' if !in_quotes => return Some((&line[..i], &line[i + 1..])),
            _ => {}
        }
    }
    None
}

fn unescape_text(v: &str) -> String {
    let mut out = String::with_capacity(v.len());
    let mut chars = v.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') | Some('N') => out.push(' '),
            Some(other) => out.push(other),
            None => {}
        }
    }
    out
}

fn parse_dt(name_and_params: &str, value: &str) -> Option<DtValue> {
    let mut tzid = None;
    let mut is_date = false;
    for param in name_and_params.split(';').skip(1) {
        let (k, v) = param.split_once('=').unwrap_or((param, ""));
        match k.to_ascii_uppercase().as_str() {
            "TZID" => tzid = Some(v.trim_matches('"').to_string()),
            "VALUE" if v.eq_ignore_ascii_case("DATE") => is_date = true,
            _ => {}
        }
    }
    let value = value.trim();
    if is_date || value.len() == 8 {
        return NaiveDate::parse_from_str(value, "%Y%m%d").ok().map(DtValue::Date);
    }
    if let Some(stripped) = value.strip_suffix('Z') {
        let ndt = NaiveDateTime::parse_from_str(stripped, "%Y%m%dT%H%M%S").ok()?;
        return Some(DtValue::Utc(Utc.from_utc_datetime(&ndt)));
    }
    let ndt = NaiveDateTime::parse_from_str(value, "%Y%m%dT%H%M%S").ok()?;
    Some(DtValue::Zoned(ndt, tzid))
}

fn parse_ics_events(ics: &str) -> Vec<VEvent> {
    let mut events = Vec::new();
    let mut current: Option<VEvent> = None;
    for line in unfold(ics) {
        let Some((head, value)) = split_content_line(&line) else { continue };
        let name = head.split(';').next().unwrap_or("").to_ascii_uppercase();
        match name.as_str() {
            "BEGIN" if value.eq_ignore_ascii_case("VEVENT") => {
                current = Some(VEvent::default());
            }
            "END" if value.eq_ignore_ascii_case("VEVENT") => {
                if let Some(ev) = current.take() {
                    events.push(ev);
                }
            }
            _ => {
                let Some(ev) = current.as_mut() else { continue };
                match name.as_str() {
                    "UID" => ev.uid = value.trim().to_string(),
                    "SUMMARY" => ev.summary = unescape_text(value.trim()),
                    "DTSTART" => ev.dtstart = parse_dt(head, value),
                    "DTEND" => ev.dtend = parse_dt(head, value),
                    "RRULE" | "RDATE" => ev.has_rrule = true,
                    "STATUS" => ev.cancelled = value.trim().eq_ignore_ascii_case("CANCELLED"),
                    _ => {}
                }
            }
        }
    }
    events
}

// ── VEvent → records ──────────────────────────────────────────────────────

fn to_local(dt: &DtValue) -> (NaiveDate, Option<(u32, u32)>) {
    use chrono::Timelike;
    match dt {
        DtValue::Date(d) => (*d, None),
        DtValue::Utc(dt) => {
            let local = dt.with_timezone(&Local);
            (local.date_naive(), Some((local.time().hour(), local.time().minute())))
        }
        DtValue::Zoned(ndt, tzid) => {
            let converted = tzid
                .as_deref()
                .and_then(|id| chrono_tz::Tz::from_str(id).ok())
                .and_then(|tz| tz.from_local_datetime(ndt).earliest())
                .map(|dt| dt.with_timezone(&Local).naive_local());
            if converted.is_none() && tzid.is_some() {
                log::warn!("unknown TZID {:?}; treating as local time", tzid.as_deref().unwrap());
            }
            let ndt = converted.unwrap_or(*ndt);
            (ndt.date(), Some((ndt.time().hour(), ndt.time().minute())))
        }
    }
}

fn event_to_records(
    ev: &VEvent,
    window: (NaiveDate, NaiveDate),
    source: &str,
    seen: &mut BTreeSet<(String, Option<String>, String)>,
    out: &mut Vec<EventRecord>,
) {
    if ev.cancelled {
        return;
    }
    let Some(dtstart) = &ev.dtstart else { return };
    let title = if ev.summary.is_empty() { "(untitled)".to_string() } else { ev.summary.clone() };
    let uid = (!ev.uid.is_empty()).then(|| ev.uid.clone());

    let mut push = |date: NaiveDate, time: Option<(u32, u32)>| {
        if date < window.0 || date > window.1 {
            return;
        }
        let date_s = date.to_string();
        let time_s = time.map(|(h, m)| format!("{h:02}:{m:02}"));
        // Expanded instances of one event share a UID; the (date, time,
        // title) key is what makes each day's mirror record unique.
        if seen.insert((date_s.clone(), time_s.clone(), title.clone())) {
            out.push(EventRecord {
                date: date_s,
                time: time_s,
                title: title.clone(),
                uid: uid.clone(),
                source: Some(source.to_string()),
            });
        }
    };

    match to_local(dtstart) {
        (date, Some(time)) => push(date, Some(time)),
        (start, None) => {
            // All-day: DTEND is exclusive per RFC 5545; a missing one means
            // a single day. One untimed record per covered day.
            let end = match ev.dtend.as_ref().map(to_local) {
                Some((d, _)) if d > start => d,
                _ => start.checked_add_days(Days::new(1)).unwrap_or(start),
            };
            let mut day = start;
            let mut span = 0;
            while day < end && span < MAX_ALLDAY_SPAN {
                push(day, None);
                let Some(next) = day.checked_add_days(Days::new(1)) else { break };
                day = next;
                span += 1;
            }
        }
    }
}

// ── Merge ─────────────────────────────────────────────────────────────────

/// Replace each synced source's records wholesale; everything else — local
/// events and sources not synced this run — passes through untouched.
fn merge(existing: Vec<EventRecord>, synced: &[(String, Vec<EventRecord>)]) -> Vec<EventRecord> {
    let replaced: Vec<&str> = synced.iter().map(|(s, _)| s.as_str()).collect();
    let mut merged: Vec<EventRecord> = existing
        .into_iter()
        .filter(|r| !r.source.as_deref().is_some_and(|s| replaced.contains(&s)))
        .collect();
    for (_, records) in synced {
        merged.extend(records.iter().cloned());
    }
    merged.sort_by(|a, b| {
        (&a.date, a.time.is_none(), &a.time, &a.title).cmp(&(&b.date, b.time.is_none(), &b.time, &b.title))
    });
    merged
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(date: &str, source: Option<&str>, title: &str) -> EventRecord {
        EventRecord {
            date: date.into(),
            time: None,
            title: title.into(),
            uid: None,
            source: source.map(String::from),
        }
    }

    #[test]
    fn unfolds_continuation_lines() {
        let lines = unfold("SUMMARY:split\r\n  over\r\nUID:x\n\tmore");
        assert_eq!(lines, vec!["SUMMARY:split over", "UID:xmore"]);
    }

    #[test]
    fn parses_expanded_utc_event() {
        let ics = "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:abc\r\nDTSTART:20260901T140000Z\r\nDTEND:20260901T150000Z\r\nSUMMARY:Dentist\\, checkup\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let events = parse_ics_events(ics);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].uid, "abc");
        assert_eq!(events[0].summary, "Dentist, checkup");
        assert!(matches!(events[0].dtstart, Some(DtValue::Utc(_))));
        assert!(!events[0].has_rrule);
    }

    #[test]
    fn all_day_span_yields_one_record_per_day_dtend_exclusive() {
        let ev = VEvent {
            uid: "trip".into(),
            summary: "Trip".into(),
            dtstart: Some(DtValue::Date(NaiveDate::from_ymd_opt(2026, 9, 10).unwrap())),
            dtend: Some(DtValue::Date(NaiveDate::from_ymd_opt(2026, 9, 13).unwrap())),
            ..Default::default()
        };
        let window = (
            NaiveDate::from_ymd_opt(2026, 9, 1).unwrap(),
            NaiveDate::from_ymd_opt(2026, 12, 1).unwrap(),
        );
        let (mut seen, mut out) = (BTreeSet::new(), Vec::new());
        event_to_records(&ev, window, "icloud:x", &mut seen, &mut out);
        assert_eq!(
            out.iter().map(|r| r.date.as_str()).collect::<Vec<_>>(),
            vec!["2026-09-10", "2026-09-11", "2026-09-12"]
        );
        assert!(out.iter().all(|r| r.time.is_none() && r.source.as_deref() == Some("icloud:x")));
    }

    #[test]
    fn cancelled_events_are_dropped() {
        let ics = "BEGIN:VEVENT\r\nUID:x\r\nSTATUS:CANCELLED\r\nDTSTART:20260901T140000Z\r\nEND:VEVENT\r\n";
        let ev = &parse_ics_events(ics)[0];
        let (mut seen, mut out) = (BTreeSet::new(), Vec::new());
        let window = (
            NaiveDate::from_ymd_opt(2026, 1, 1).unwrap(),
            NaiveDate::from_ymd_opt(2027, 1, 1).unwrap(),
        );
        event_to_records(ev, window, "s", &mut seen, &mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn merge_replaces_own_source_and_keeps_the_rest() {
        let existing = vec![
            record("2026-09-01", None, "hand-entered"),
            record("2026-09-02", Some("icloud:a@icloud.com"), "stale"),
            record("2026-09-03", Some("icloud:other@me.com"), "foreign"),
        ];
        let fresh = vec![record("2026-09-04", Some("icloud:a@icloud.com"), "fresh")];
        let merged = merge(existing, &[("icloud:a@icloud.com".to_string(), fresh)]);
        let titles: Vec<_> = merged.iter().map(|r| r.title.as_str()).collect();
        assert_eq!(titles, vec!["hand-entered", "foreign", "fresh"]);
    }
}
