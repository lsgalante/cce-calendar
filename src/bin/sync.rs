//! `cce-calendar-sync` — two-way sync between remote calendars and
//! cce-calendar's `events.json`.
//!
//! Accounts come from the same `accounts.json` cce-mail reads (owned by
//! cce-system-interface). Two kinds are synced:
//!
//! - **iCloud**, over CalDAV, with the password from the `cce-mail` keyring
//!   service — an Apple app-specific password is valid for CalDAV as well as
//!   IMAP, so the credential that fetches mail fetches the calendar too.
//! - **Google**, over the Calendar REST API, with the OAuth tokens the
//!   settings app's Google sign-in stores (`calendar.readonly` to read,
//!   `calendar.events` to write). Google's CalDAV endpoint refuses app
//!   passwords, hence the API. The access token is refreshed in memory each
//!   run and never written back: accounts.json has enough writers already.
//!
//! Reading is a mirror: each run replaces the records whose `source` matches
//! the account ("icloud:<email>" / "google:<email>"), with recurrences
//! expanded server-side (CalDAV `<C:expand>`, Google `singleEvents=true`)
//! and marked `recurring`. Writing is a three-way merge against
//! `sync-state.json`, the last-synced (date, time, title, etag) per
//! NON-recurring event: a record typed in the app (no `source`) is created
//! on the default calendar (`push-to` in the app's config.kdl; iCloud when
//! unset and such an account exists) and comes back carrying its identity;
//! a tracked event missing from the file is deleted on the server; one whose
//! date, time or title differs from the state is updated there (local wins
//! over a simultaneous remote edit; the next tick reconciles). Instances of
//! recurring events are never written: the mirror cannot say "just this
//! one", so a deleted instance simply reappears. Guards: a missing
//! events.json re-imports rather than deletes, and a run that would delete
//! most tracked events (>5 and >50%) refuses without `--force-deletes`.
//!
//! CalDAV updates PATCH the fetched iCalendar (SUMMARY, DTSTART, DTEND)
//! rather than rebuilding it, so alarms and Apple's own properties survive;
//! Google updates are field-level PATCHes with If-Match.
//!
//! Usage: `cce-calendar-sync [--dry-run] [--force-deletes]`. Driven by
//! cce-calendar-sync.timer; harmless to run by hand. Exits nonzero if any
//! account failed (the timer just tries again next tick); other accounts'
//! results are still written.

use std::collections::{BTreeMap, BTreeSet};
use std::str::FromStr;

use cce_calendar::{
    data_path, load_records, load_sync_state, push_target_config, save_records, save_sync_state,
    sort_records, EventRecord, PushTarget, SyncState, SyncedEvent,
};
use chrono::{DateTime, Days, Duration, Local, NaiveDate, NaiveDateTime, TimeZone, Utc};

const CALDAV_ROOT: &str = "https://caldav.icloud.com/";
/// Sync window around today. Wide enough forward that "next spring" plans
/// show up; bounded so the file stays a glanceable flat list.
const PAST_DAYS: u64 = 60;
const FUTURE_DAYS: u64 = 400;
/// An all-day event spanning more than this is almost certainly bad data
/// (a botched DTEND); clamp rather than flood two months of cells.
const MAX_ALLDAY_SPAN: u64 = 62;
/// A timed event typed in the app has no end; it is created an hour long.
const DEFAULT_DURATION_MIN: i64 = 60;

const CALDAV_NS: &str = "urn:ietf:params:xml:ns:caldav";

fn main() {
    env_logger::init();
    let args: Vec<String> = std::env::args().collect();
    let dry_run = args.iter().any(|a| a == "--dry-run");
    let force_deletes = args.iter().any(|a| a == "--force-deletes");

    let (icloud, google) = match (icloud_accounts(), google_accounts()) {
        (Ok(i), Ok(g)) => (i, g),
        (Err(e), _) | (_, Err(e)) => {
            log::error!("cannot read accounts: {e}");
            std::process::exit(1);
        }
    };
    if icloud.is_empty() && google.is_empty() {
        log::info!("no iCloud or Google (OAuth) accounts in accounts.json; nothing to sync");
        return;
    }

    let today = Local::now().date_naive();
    let window = (
        today.checked_sub_days(Days::new(PAST_DAYS)).unwrap_or(today),
        today.checked_add_days(Days::new(FUTURE_DAYS)).unwrap_or(today),
    );

    // Typed events go to one calendar; the account kind that owns it.
    let push = push_target_config().unwrap_or_else(|| PushTarget {
        kind: if !icloud.is_empty() { "icloud" } else { "google" }.to_string(),
        calendar: None,
    });

    let had_file = data_path().exists();
    let mut records = match load_records() {
        Ok(r) => r,
        Err(e) => {
            // Refuse to rewrite a file we could not read — that would
            // silently drop every hand-entered event.
            log::error!("events.json unreadable, not writing: {e}");
            std::process::exit(1);
        }
    };
    let mut state = match load_sync_state() {
        Ok(s) => s,
        Err(e) => {
            log::error!("sync-state.json unreadable: {e}");
            std::process::exit(1);
        }
    };
    if !had_file && !state.events.is_empty() {
        // The file is gone (fresh clone, deleted). Re-import rather than
        // reading absence as "delete everything on the server".
        log::warn!("events.json missing; discarding sync state and re-importing");
        state = SyncState::default();
    }

    let mut failed = false;
    let mut synced_any = false;
    let backends: Vec<Backend> = icloud
        .into_iter()
        .map(Backend::ICloud)
        .chain(google.into_iter().map(Backend::Google))
        .collect();
    for (i, backend) in backends.iter().enumerate() {
        let push_here = push.kind == backend.kind()
            && backends.iter().position(|b| b.kind() == push.kind) == Some(i);
        match sync_source(backend, window, &push, push_here, &mut records, &mut state, dry_run, force_deletes)
        {
            Ok(()) => synced_any = true,
            Err(e) => {
                log::error!("{}: sync failed, keeping existing records: {e}", backend.email());
                failed = true;
            }
        }
    }

    if synced_any && !dry_run {
        sort_records(&mut records);
        if let Err(e) = save_records(&records) {
            log::error!("saving events.json failed: {e}");
            failed = true;
        }
        if let Err(e) = save_sync_state(&state) {
            log::error!("saving sync-state.json failed: {e}");
            failed = true;
        }
    }
    if failed {
        std::process::exit(1);
    }
}

enum Backend {
    ICloud(Account),
    Google(GoogleAccount),
}

impl Backend {
    fn email(&self) -> &str {
        match self {
            Backend::ICloud(a) => &a.email,
            Backend::Google(a) => &a.email,
        }
    }
    fn kind(&self) -> &'static str {
        match self {
            Backend::ICloud(_) => "icloud",
            Backend::Google(_) => "google",
        }
    }
    fn source(&self) -> String {
        format!("{}:{}", self.kind(), self.email())
    }
}

// ── Remote model (both backends produce it) ───────────────────────────────

struct RemoteCalendar {
    /// Where a new event is created (CalDAV collection URL / Google
    /// calendar id).
    target: String,
    name: String,
    primary: bool,
}

struct RemoteEvent {
    url: reqwest::Url,
    etag: String,
    recurring: bool,
    /// Every dated instance in the window, ready for the file.
    instances: Vec<EventRecord>,
    /// Unfolded logical lines of the resource (CalDAV only), for
    /// patch-and-PUT.
    lines: Vec<String>,
}

struct Remote {
    calendars: Vec<RemoteCalendar>,
    events: BTreeMap<String, RemoteEvent>,
}

// ── One account ───────────────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
fn sync_source(
    backend: &Backend,
    window: (NaiveDate, NaiveDate),
    push: &PushTarget,
    push_here: bool,
    records: &mut Vec<EventRecord>,
    state: &mut SyncState,
    dry_run: bool,
    force_deletes: bool,
) -> Result<(), String> {
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .build()
        .map_err(|e| e.to_string())?;
    let source = backend.source();
    let session = match backend {
        Backend::ICloud(_) => Session::ICloud,
        Backend::Google(acc) => Session::Google(google_access_token(&client, acc)?),
    };
    let ops = Ops { client: &client, backend, session: &session };
    let mut remote = ops.fetch(window, &source)?;
    log::info!(
        "{}: {} calendar(s), {} event(s) in window",
        backend.email(),
        remote.calendars.len(),
        remote.events.len()
    );

    // ── Plan ───────────────────────────────────────────────────────────
    let local_by_uid: BTreeMap<&str, &EventRecord> = records
        .iter()
        .filter(|r| r.source.as_deref() == Some(&source))
        .filter_map(|r| r.uid.as_deref().map(|u| (u, r)))
        .collect();
    let mut push_deletes: Vec<String> = Vec::new();
    let mut push_updates: Vec<(String, EventRecord)> = Vec::new();
    let mut forget: Vec<String> = Vec::new();
    for (uid, s) in state.events.iter().filter(|(_, s)| s.source == source) {
        match remote.events.get(uid) {
            None => forget.push(uid.clone()),
            Some(r) if r.recurring => forget.push(uid.clone()),
            Some(_) => match local_by_uid.get(uid.as_str()) {
                None => push_deletes.push(uid.clone()),
                Some(l) if !s.matches(l) => push_updates.push((uid.clone(), (*l).clone())),
                Some(_) => {}
            },
        }
    }
    let push_creates: Vec<usize> = if push_here {
        records
            .iter()
            .enumerate()
            .filter(|(_, r)| r.source.is_none() && r.uid.is_none())
            .map(|(i, _)| i)
            .collect()
    } else {
        Vec::new()
    };

    let tracked = state.events.values().filter(|s| s.source == source).count();
    if !force_deletes && push_deletes.len() > 5 && push_deletes.len() * 2 > tracked {
        return Err(format!(
            "refusing to delete {} of {tracked} tracked events on the server — if they were \
             really removed on purpose, run cce-calendar-sync --force-deletes",
            push_deletes.len()
        ));
    }
    log::info!(
        "{}: push {} new / {} changed / {} deleted",
        backend.email(),
        push_creates.len(),
        push_updates.len(),
        push_deletes.len()
    );
    let target = if push_here { Some(ops.pick_target(&remote.calendars, push)?) } else { None };
    if let Some(t) = &target {
        if !push_creates.is_empty() {
            log::info!("{}: new events go to {}", backend.email(), t.name);
        }
    }
    if dry_run {
        for i in &push_creates {
            let r = &records[*i];
            println!("push new:    {} {} {}", r.date, r.time.as_deref().unwrap_or("-----"), r.title);
        }
        for (uid, r) in &push_updates {
            println!("push change: {} {} {} ({uid})", r.date, r.time.as_deref().unwrap_or("-----"), r.title);
        }
        for uid in &push_deletes {
            println!("push delete: {} ({uid})", state.events[uid].title);
        }
        let mirrored: usize = remote.events.values().map(|e| e.instances.len()).sum();
        println!("{source}: {mirrored} record(s) mirrored");
        return Ok(());
    }

    // ── Server side ────────────────────────────────────────────────────
    for uid in &push_deletes {
        let s = &state.events[uid];
        let url = reqwest::Url::parse(&s.url).map_err(|e| e.to_string())?;
        match ops.delete(&url, &s.etag) {
            Ok(()) => {
                state.events.remove(uid);
                remote.events.remove(uid);
            }
            Err(e) => log::warn!("push delete {uid} failed (will retry next tick): {e}"),
        }
    }
    for (uid, local) in &push_updates {
        let Some(r) = remote.events.get_mut(uid) else { continue };
        match ops.update(r, local) {
            Ok(etag) => {
                r.etag = etag.clone();
                r.instances = vec![EventRecord {
                    uid: Some(uid.clone()),
                    source: Some(source.clone()),
                    recurring: false,
                    ..local.clone()
                }];
                state.events.insert(uid.clone(), synced(&source, &r.url, etag, local));
            }
            Err(e) => log::warn!("push update {uid} failed (will retry next tick): {e}"),
        }
    }
    let mut consumed: BTreeSet<usize> = BTreeSet::new();
    if let Some(t) = &target {
        for i in &push_creates {
            let local = &records[*i];
            match ops.create(t, local) {
                Ok((uid, url, etag)) => {
                    state.events.insert(uid.clone(), synced(&source, &url, etag.clone(), local));
                    remote.events.insert(
                        uid.clone(),
                        RemoteEvent {
                            url,
                            etag,
                            recurring: false,
                            instances: vec![EventRecord {
                                uid: Some(uid),
                                source: Some(source.clone()),
                                recurring: false,
                                ..local.clone()
                            }],
                            lines: Vec::new(),
                        },
                    );
                    consumed.insert(*i);
                }
                Err(e) => log::warn!("push create {:?} failed (will retry next tick): {e}", local.title),
            }
        }
    }

    // ── State: every non-recurring event the server now holds ─────────
    for uid in &forget {
        state.events.remove(uid);
    }
    for (uid, r) in &remote.events {
        if r.recurring {
            continue;
        }
        if let Some(first) = r.instances.first() {
            state.events.insert(uid.clone(), synced(&source, &r.url, r.etag.clone(), first));
        }
    }

    // ── File: this source's records are the fresh mirror ──────────────
    let mut idx = 0;
    records.retain(|r| {
        let keep = r.source.as_deref() != Some(&source) && !consumed.contains(&idx);
        idx += 1;
        keep
    });
    for r in remote.events.values() {
        records.extend(r.instances.iter().cloned());
    }
    Ok(())
}

fn synced(source: &str, url: &reqwest::Url, etag: String, r: &EventRecord) -> SyncedEvent {
    SyncedEvent {
        source: source.to_string(),
        url: url.to_string(),
        etag,
        date: r.date.clone(),
        time: r.time.clone(),
        title: r.title.clone(),
    }
}

/// Per-run credentials the requests need beyond the account itself.
enum Session {
    ICloud,
    Google(String),
}

/// The backend operations, bundled so the pass reads the same either way.
struct Ops<'a> {
    client: &'a reqwest::blocking::Client,
    backend: &'a Backend,
    session: &'a Session,
}

impl Ops<'_> {
    fn token(&self) -> &str {
        match self.session {
            Session::Google(t) => t,
            Session::ICloud => "",
        }
    }

    fn fetch(&self, window: (NaiveDate, NaiveDate), source: &str) -> Result<Remote, String> {
        match self.backend {
            Backend::ICloud(acc) => fetch_icloud(self.client, acc, window, source),
            Backend::Google(acc) => fetch_google(self.client, self.token(), acc, window, source),
        }
    }

    /// The calendar typed events are created on: the configured name, else
    /// the account's primary (Google) or first (iCloud) event calendar.
    fn pick_target<'c>(
        &self,
        calendars: &'c [RemoteCalendar],
        push: &PushTarget,
    ) -> Result<&'c RemoteCalendar, String> {
        if let Some(name) = &push.calendar {
            return calendars
                .iter()
                .find(|c| c.name.eq_ignore_ascii_case(name))
                .ok_or_else(|| format!("push-to calendar {name:?} not found on this account"));
        }
        calendars
            .iter()
            .find(|c| c.primary)
            .or_else(|| calendars.first())
            .ok_or_else(|| "no writable calendar on this account".to_string())
    }

    fn create(
        &self,
        target: &RemoteCalendar,
        r: &EventRecord,
    ) -> Result<(String, reqwest::Url, String), String> {
        match self.backend {
            Backend::ICloud(acc) => {
                let uid = new_uid();
                let base = reqwest::Url::parse(&target.target).map_err(|e| e.to_string())?;
                let url = base.join(&format!("{uid}.ics")).map_err(|e| e.to_string())?;
                let etag = put_ics(self.client, acc, &url, &new_vevent(&uid, r)?, None)?;
                Ok((uid, url, etag))
            }
            Backend::Google(_) => google_create(self.client, self.token(), &target.target, r),
        }
    }

    fn update(&self, remote: &RemoteEvent, r: &EventRecord) -> Result<String, String> {
        match self.backend {
            Backend::ICloud(acc) => {
                let body = patch_vevent(&remote.lines, r)?;
                put_ics(self.client, acc, &remote.url, &body, Some(&remote.etag))
            }
            Backend::Google(_) => google_update(self.client, self.token(), &remote.url, &remote.etag, r),
        }
    }

    fn delete(&self, url: &reqwest::Url, etag: &str) -> Result<(), String> {
        match self.backend {
            Backend::ICloud(acc) => delete_ics(self.client, acc, url, etag),
            Backend::Google(_) => google_delete(self.client, self.token(), url),
        }
    }
}

// ── Time helpers ──────────────────────────────────────────────────────────

fn parse_record_time(r: &EventRecord) -> Result<(NaiveDate, Option<(u32, u32)>), String> {
    let date = r.date.parse::<NaiveDate>().map_err(|e| format!("bad date {:?}: {e}", r.date))?;
    let time = match &r.time {
        None => None,
        Some(t) => {
            let (h, m) = t.split_once(':').ok_or_else(|| format!("bad time {t:?}"))?;
            Some((h.parse().map_err(|_| format!("bad time {t:?}"))?, m.parse().map_err(|_| format!("bad time {t:?}"))?))
        }
    };
    Ok((date, time))
}

/// A record's start (and default end) as local wall-clock instants.
fn record_span(r: &EventRecord) -> Result<(DateTime<Local>, DateTime<Local>), String> {
    let (date, time) = parse_record_time(r)?;
    let (h, m) = time.unwrap_or((0, 0));
    let ndt = date.and_hms_opt(h, m, 0).ok_or("bad time")?;
    let start = Local
        .from_local_datetime(&ndt)
        .earliest()
        .ok_or_else(|| format!("{ndt} does not exist in the local timezone"))?;
    Ok((start, start + Duration::minutes(DEFAULT_DURATION_MIN)))
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

// ── Google (OAuth) ────────────────────────────────────────────────────────

struct GoogleAccount {
    email: String,
    refresh_token: String,
    client_id: String,
    client_secret: String,
}

/// The OAuth fields the settings app's Google sign-in writes.
#[derive(serde::Deserialize)]
struct OAuthOnDisk {
    email: String,
    #[serde(default)]
    is_oauth: bool,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    client_id: Option<String>,
    #[serde(default)]
    client_secret: Option<String>,
}

#[derive(serde::Deserialize, Default)]
struct GoogleClientConfig {
    #[serde(default)]
    client_id: String,
    #[serde(default)]
    client_secret: String,
}

fn google_accounts() -> Result<Vec<GoogleAccount>, String> {
    let dir = cce_ui::config::cce_config_dir();
    let path = dir.join("accounts.json");
    let text = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let on_disk: Vec<OAuthOnDisk> =
        serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
    // An account without its own pinned client credentials falls back to
    // the global template the settings app maintains.
    let template: GoogleClientConfig = std::fs::read_to_string(dir.join("google_client.json"))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default();
    let mut out = Vec::new();
    for acc in on_disk {
        if !acc.is_oauth {
            continue;
        }
        let Some(refresh_token) = acc.refresh_token.filter(|t| !t.is_empty()) else {
            log::warn!("{}: OAuth account without a refresh token; sign in again", acc.email);
            continue;
        };
        out.push(GoogleAccount {
            email: acc.email,
            refresh_token,
            client_id: acc.client_id.filter(|s| !s.is_empty()).unwrap_or(template.client_id.clone()),
            client_secret: acc
                .client_secret
                .filter(|s| !s.is_empty())
                .unwrap_or(template.client_secret.clone()),
        });
    }
    Ok(out)
}

/// A fresh access token from the refresh grant. Tokens last an hour and a
/// tick is one request burst, so refreshing every run is simpler than
/// tracking expiry — and keeps this helper from writing accounts.json.
fn google_access_token(
    client: &reqwest::blocking::Client,
    acc: &GoogleAccount,
) -> Result<String, String> {
    let resp = client
        .post("https://oauth2.googleapis.com/token")
        .form(&[
            ("client_id", acc.client_id.as_str()),
            ("client_secret", acc.client_secret.as_str()),
            ("refresh_token", acc.refresh_token.as_str()),
            ("grant_type", "refresh_token"),
        ])
        .send()
        .map_err(|e| format!("token refresh: {e}"))?;
    let status = resp.status();
    let body: serde_json::Value = resp.json().map_err(|e| format!("token refresh: {e}"))?;
    if !status.is_success() {
        // invalid_grant here means the refresh token was revoked or the
        // consent predates the calendar scope — a re-login fixes both.
        return Err(format!("token refresh: HTTP {status} {body}"));
    }
    body.get("access_token")
        .and_then(|v| v.as_str())
        .map(String::from)
        .ok_or_else(|| "token refresh: no access_token in response".to_string())
}

fn google_call(
    client: &reqwest::blocking::Client,
    token: &str,
    method: reqwest::Method,
    url: &str,
    query: &[(&str, &str)],
    body: Option<&serde_json::Value>,
    if_match: Option<&str>,
) -> Result<serde_json::Value, String> {
    let mut req = client.request(method.clone(), url).bearer_auth(token).query(query);
    if let Some(b) = body {
        req = req.json(b);
    }
    if let Some(e) = if_match.filter(|e| !e.is_empty()) {
        req = req.header("If-Match", e);
    }
    let resp = req.send().map_err(|e| format!("{method} {url}: {e}"))?;
    let status = resp.status();
    if status == reqwest::StatusCode::NO_CONTENT {
        return Ok(serde_json::Value::Null);
    }
    let text = resp.text().map_err(|e| format!("{method} {url}: {e}"))?;
    if !status.is_success() {
        return Err(format!("{method} {url}: HTTP {status} {text}"));
    }
    if text.trim().is_empty() {
        return Ok(serde_json::Value::Null);
    }
    serde_json::from_str(&text).map_err(|e| format!("{method} {url}: bad JSON: {e}"))
}

const CALENDAR_API: &str = "https://www.googleapis.com/calendar/v3";

fn google_events_url(cal_id: &str) -> String {
    format!("{CALENDAR_API}/calendars/{}/events", urlencode(cal_id))
}

fn fetch_google(
    client: &reqwest::blocking::Client,
    token: &str,
    acc: &GoogleAccount,
    window: (NaiveDate, NaiveDate),
    source: &str,
) -> Result<Remote, String> {
    let get = |url: &str, q: &[(&str, &str)]| {
        google_call(client, token, reqwest::Method::GET, url, q, None, None)
    };
    // Only calendars the user keeps visible in Google's own UI (`selected`);
    // subscribed-but-hidden ones stay hidden here too.
    let list = get(
        &format!("{CALENDAR_API}/users/me/calendarList"),
        &[("fields", "items(id,summary,selected,deleted,primary,accessRole)")],
    )?;
    let calendars: Vec<RemoteCalendar> = list["items"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|c| c["selected"].as_bool().unwrap_or(false) && !c["deleted"].as_bool().unwrap_or(false))
        .filter_map(|c| {
            Some(RemoteCalendar {
                target: c["id"].as_str()?.to_string(),
                name: c["summary"].as_str().unwrap_or("?").to_string(),
                primary: c["primary"].as_bool().unwrap_or(false),
            })
        })
        .collect();
    let _ = acc;

    let time_min = format!("{}T00:00:00Z", window.0);
    let time_max = format!("{}T00:00:00Z", window.1);
    let mut events: BTreeMap<String, RemoteEvent> = BTreeMap::new();
    let mut seen = BTreeSet::new();
    for cal in &calendars {
        let url = google_events_url(&cal.target);
        let mut page_token = String::new();
        loop {
            let mut query = vec![
                ("singleEvents", "true"),
                ("timeMin", time_min.as_str()),
                ("timeMax", time_max.as_str()),
                ("maxResults", "2500"),
                ("fields", "nextPageToken,items(id,iCalUID,recurringEventId,etag,summary,status,start,end)"),
            ];
            if !page_token.is_empty() {
                query.push(("pageToken", page_token.as_str()));
            }
            let page = get(&url, &query).map_err(|e| format!("calendar {}: {e}", cal.name))?;
            for item in page["items"].as_array().into_iter().flatten() {
                let Some(ev) = google_event(item) else { continue };
                let id = item["id"].as_str().unwrap_or("").to_string();
                let uid = item["iCalUID"].as_str().map(String::from).unwrap_or_else(|| id.clone());
                let recurring = item["recurringEventId"].is_string();
                let mut instances = Vec::new();
                event_to_records(&ev, window, source, recurring, &mut seen, &mut instances);
                let entry = events.entry(uid).or_insert_with(|| RemoteEvent {
                    url: reqwest::Url::parse(&format!("{url}/{id}")).expect("valid url"),
                    etag: item["etag"].as_str().unwrap_or("").to_string(),
                    recurring,
                    instances: Vec::new(),
                    lines: Vec::new(),
                });
                entry.recurring |= recurring;
                entry.instances.extend(instances);
            }
            match page["nextPageToken"].as_str() {
                Some(t) if !t.is_empty() => page_token = t.to_string(),
                _ => break,
            }
        }
    }
    // Cancelled instances leave an empty entry; drop those.
    events.retain(|_, e| !e.instances.is_empty());
    Ok(Remote { calendars, events })
}

/// Google's `{date}` / `{dateTime}` pair onto the same DtValue the iCal
/// path produces, so both feed one `event_to_records`.
fn google_event(item: &serde_json::Value) -> Option<VEvent> {
    let dt = |v: &serde_json::Value| -> Option<DtValue> {
        if let Some(d) = v["date"].as_str() {
            return NaiveDate::parse_from_str(d, "%Y-%m-%d").ok().map(DtValue::Date);
        }
        let s = v["dateTime"].as_str()?;
        DateTime::parse_from_rfc3339(s).ok().map(|t| DtValue::Utc(t.with_timezone(&Utc)))
    };
    Some(VEvent {
        uid: item["iCalUID"].as_str().unwrap_or("").to_string(),
        summary: item["summary"].as_str().unwrap_or("").to_string(),
        dtstart: Some(dt(&item["start"])?),
        dtend: dt(&item["end"]),
        cancelled: item["status"].as_str() == Some("cancelled"),
        has_rrule: false,
        has_recurrence_id: false,
    })
}

fn google_body(r: &EventRecord) -> Result<serde_json::Value, String> {
    let (date, time) = parse_record_time(r)?;
    let (start, end) = if time.is_some() {
        let (s, e) = record_span(r)?;
        (
            serde_json::json!({ "dateTime": s.to_rfc3339() }),
            serde_json::json!({ "dateTime": e.to_rfc3339() }),
        )
    } else {
        let next = date.checked_add_days(Days::new(1)).ok_or("date overflow")?;
        (
            serde_json::json!({ "date": date.to_string() }),
            serde_json::json!({ "date": next.to_string() }),
        )
    };
    Ok(serde_json::json!({ "summary": r.title, "start": start, "end": end }))
}

fn google_create(
    client: &reqwest::blocking::Client,
    token: &str,
    cal_id: &str,
    r: &EventRecord,
) -> Result<(String, reqwest::Url, String), String> {
    let body = google_body(r)?;
    let url = google_events_url(cal_id);
    let resp = google_call(client, token, reqwest::Method::POST, &url, &[], Some(&body), None)?;
    let id = resp["id"].as_str().ok_or("created event has no id")?.to_string();
    let uid = resp["iCalUID"].as_str().map(String::from).unwrap_or_else(|| id.clone());
    let event_url = reqwest::Url::parse(&format!("{url}/{id}")).map_err(|e| e.to_string())?;
    Ok((uid, event_url, resp["etag"].as_str().unwrap_or("").to_string()))
}

/// Field-level PATCH: summary, start, end — a description, attendees or
/// reminders set in Google's own apps ride through untouched.
fn google_update(
    client: &reqwest::blocking::Client,
    token: &str,
    url: &reqwest::Url,
    etag: &str,
    r: &EventRecord,
) -> Result<String, String> {
    let body = google_body(r)?;
    let resp = google_call(client, token, reqwest::Method::PATCH, url.as_str(), &[], Some(&body), Some(etag))?;
    Ok(resp["etag"].as_str().unwrap_or("").to_string())
}

fn google_delete(client: &reqwest::blocking::Client, token: &str, url: &reqwest::Url) -> Result<(), String> {
    match google_call(client, token, reqwest::Method::DELETE, url.as_str(), &[], None, None) {
        Ok(_) => Ok(()),
        // Already gone counts as done.
        Err(e) if e.contains("HTTP 404") || e.contains("HTTP 410") => Ok(()),
        Err(e) => Err(e),
    }
}

/// Calendar ids are email-like and go into the path; only the few
/// characters that could break it need escaping.
fn urlencode(s: &str) -> String {
    s.replace('%', "%25").replace('/', "%2F").replace('#', "%23").replace('?', "%3F").replace('@', "%40")
}

// ── CalDAV (iCloud) ───────────────────────────────────────────────────────

fn fetch_icloud(
    client: &reqwest::blocking::Client,
    acc: &Account,
    window: (NaiveDate, NaiveDate),
    source: &str,
) -> Result<Remote, String> {
    let root = reqwest::Url::parse(CALDAV_ROOT).expect("static url");
    let principal = discover_href(
        client, acc, &root, "0",
        r#"<?xml version="1.0" encoding="utf-8"?>
<propfind xmlns="DAV:"><prop><current-user-principal/></prop></propfind>"#,
        "current-user-principal",
    )?;
    let home = discover_href(
        client, acc, &principal, "0",
        r#"<?xml version="1.0" encoding="utf-8"?>
<propfind xmlns="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav"><prop><C:calendar-home-set/></prop></propfind>"#,
        "calendar-home-set",
    )?;
    let calendars = list_event_calendars(client, acc, &home)?;

    let (start, end) = (
        format!("{}T000000Z", window.0.format("%Y%m%d")),
        format!("{}T000000Z", window.1.format("%Y%m%d")),
    );
    let mut events = BTreeMap::new();
    let mut seen = BTreeSet::new();
    for cal in &calendars {
        let cal_url = reqwest::Url::parse(&cal.target).map_err(|e| e.to_string())?;
        fetch_events(client, acc, &cal_url, &start, &end, window, source, &mut seen, &mut events)
            .map_err(|e| format!("calendar {}: {e}", cal.name))?;
    }
    Ok(Remote { calendars, events })
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
/// they are cce-list's).
fn list_event_calendars(
    client: &reqwest::blocking::Client,
    acc: &Account,
    home: &reqwest::Url,
) -> Result<Vec<RemoteCalendar>, String> {
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
        out.push(RemoteCalendar { target: url.to_string(), name, primary: false });
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
  <D:prop><D:getetag/>{data}</D:prop>
  <C:filter><C:comp-filter name="VCALENDAR"><C:comp-filter name="VEVENT">
    <C:time-range start="{start}" end="{end}"/>
  </C:comp-filter></C:comp-filter></C:filter>
</C:calendar-query>"#
    )
}

#[allow(clippy::too_many_arguments)]
fn fetch_events(
    client: &reqwest::blocking::Client,
    acc: &Account,
    cal: &reqwest::Url,
    start: &str,
    end: &str,
    window: (NaiveDate, NaiveDate),
    source: &str,
    seen: &mut BTreeSet<(String, Option<String>, String)>,
    events: &mut BTreeMap<String, RemoteEvent>,
) -> Result<(), String> {
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
    let mut skipped_rrule = 0usize;
    for resp in doc.descendants().filter(|n| n.tag_name().name() == "response") {
        let href = resp
            .children()
            .find(|c| c.tag_name().name() == "href")
            .and_then(|n| n.text())
            .unwrap_or_default();
        let etag = resp
            .descendants()
            .find(|n| n.tag_name().name() == "getetag")
            .and_then(|n| n.text())
            .unwrap_or_default()
            .to_string();
        let Some(ics) = resp
            .descendants()
            .find(|n| n.tag_name().name() == "calendar-data")
            .and_then(|n| n.text())
        else {
            continue;
        };
        let url = cal.join(href.trim()).map_err(|e| format!("bad href {href:?}: {e}"))?;
        let vevents = parse_ics_events(ics);
        // A resource is recurring if it says so, or if expansion produced
        // more than one dated VEVENT of it.
        let recurring = vevents.len() > 1
            || vevents.iter().any(|v| v.has_rrule || v.has_recurrence_id);
        if !expanded && recurring {
            skipped_rrule += 1;
            continue;
        }
        let Some(uid) = vevents.iter().map(|v| v.uid.clone()).find(|u| !u.is_empty()) else {
            continue;
        };
        let mut instances = Vec::new();
        for ev in &vevents {
            event_to_records(ev, window, source, recurring, seen, &mut instances);
        }
        events.insert(
            uid,
            RemoteEvent { url, etag, recurring, instances, lines: unfold(ics) },
        );
    }
    if skipped_rrule > 0 {
        log::warn!("{skipped_rrule} recurring event(s) skipped (server refused expansion)");
    }
    Ok(())
}

fn put_ics(
    client: &reqwest::blocking::Client,
    acc: &Account,
    url: &reqwest::Url,
    body: &str,
    etag: Option<&str>,
) -> Result<String, String> {
    let mut req = client
        .put(url.clone())
        .basic_auth(&acc.email, Some(&acc.password))
        .header("Content-Type", "text/calendar; charset=utf-8")
        .body(body.to_string());
    req = match etag {
        Some(e) if !e.is_empty() => req.header("If-Match", e),
        Some(_) => req,
        None => req.header("If-None-Match", "*"),
    };
    let resp = req.send().map_err(|e| format!("PUT {url}: {e}"))?;
    let status = resp.status();
    if !status.is_success() {
        return Err(format!("PUT {url}: HTTP {status}"));
    }
    let etag = resp
        .headers()
        .get("etag")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    if !etag.is_empty() {
        return Ok(etag);
    }
    Ok(fetch_etag(client, acc, url).unwrap_or_default())
}

fn fetch_etag(
    client: &reqwest::blocking::Client,
    acc: &Account,
    url: &reqwest::Url,
) -> Option<String> {
    let body = r#"<?xml version="1.0" encoding="utf-8"?>
<propfind xmlns="DAV:"><prop><getetag/></prop></propfind>"#;
    let xml = dav_request(client, acc, "PROPFIND", url, "0", body).ok()?;
    let doc = roxmltree::Document::parse(&xml).ok()?;
    doc.descendants()
        .find(|n| n.tag_name().name() == "getetag")
        .and_then(|n| n.text())
        .map(|s| s.to_string())
}

fn delete_ics(
    client: &reqwest::blocking::Client,
    acc: &Account,
    url: &reqwest::Url,
    etag: &str,
) -> Result<(), String> {
    let mut req = client.delete(url.clone()).basic_auth(&acc.email, Some(&acc.password));
    if !etag.is_empty() {
        req = req.header("If-Match", etag);
    }
    let resp = req.send().map_err(|e| format!("DELETE {url}: {e}"))?;
    let status = resp.status();
    if status.is_success() || status == reqwest::StatusCode::NOT_FOUND {
        Ok(())
    } else {
        Err(format!("DELETE {url}: HTTP {status}"))
    }
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
    has_recurrence_id: bool,
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
    lines.retain(|l| !l.is_empty());
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

fn prop_name(line: &str) -> String {
    split_content_line(line)
        .map(|(h, _)| h.split(';').next().unwrap_or("").to_ascii_uppercase())
        .unwrap_or_default()
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

fn escape_text(v: &str) -> String {
    let mut out = String::with_capacity(v.len());
    for c in v.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            ',' => out.push_str("\\,"),
            ';' => out.push_str("\\;"),
            '\n' => out.push_str("\\n"),
            _ => out.push(c),
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
                    "RECURRENCE-ID" => ev.has_recurrence_id = true,
                    "STATUS" => ev.cancelled = value.trim().eq_ignore_ascii_case("CANCELLED"),
                    _ => {}
                }
            }
        }
    }
    events
}

/// The DTSTART/DTEND pair for a record: UTC for timed events, DATE for
/// all-day ones (DTEND exclusive, one day).
fn ics_span(r: &EventRecord) -> Result<(String, String), String> {
    let (date, time) = parse_record_time(r)?;
    if time.is_some() {
        let (s, e) = record_span(r)?;
        let fmt = |t: DateTime<Local>| t.with_timezone(&Utc).format("%Y%m%dT%H%M%SZ").to_string();
        Ok((format!("DTSTART:{}", fmt(s)), format!("DTEND:{}", fmt(e))))
    } else {
        let next = date.checked_add_days(Days::new(1)).ok_or("date overflow")?;
        Ok((
            format!("DTSTART;VALUE=DATE:{}", date.format("%Y%m%d")),
            format!("DTEND;VALUE=DATE:{}", next.format("%Y%m%d")),
        ))
    }
}

fn new_vevent(uid: &str, r: &EventRecord) -> Result<String, String> {
    let now = Utc::now().format("%Y%m%dT%H%M%SZ");
    let (dtstart, dtend) = ics_span(r)?;
    Ok(format!(
        "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//cce//cce-calendar-sync//EN\r\n\
         BEGIN:VEVENT\r\nUID:{uid}\r\nDTSTAMP:{now}\r\nCREATED:{now}\r\n{dtstart}\r\n{dtend}\r\n\
         SUMMARY:{}\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n",
        escape_text(&r.title)
    ))
}

/// Rewrite only SUMMARY, DTSTART and DTEND (and drop a DURATION, which
/// DTEND supersedes) inside the first VEVENT, leaving every other property
/// — VALARM, X-APPLE-*, DESCRIPTION — exactly as the server sent it.
fn patch_vevent(lines: &[String], r: &EventRecord) -> Result<String, String> {
    let (dtstart, dtend) = ics_span(r)?;
    let mut out: Vec<String> = Vec::with_capacity(lines.len() + 3);
    let mut in_event = false;
    let mut patched = false;
    for line in lines {
        let name = prop_name(line);
        let value = split_content_line(line).map(|(_, v)| v).unwrap_or_default();
        if name == "BEGIN" && value.eq_ignore_ascii_case("VEVENT") && !patched {
            in_event = true;
            out.push(line.clone());
            continue;
        }
        if in_event && name == "END" && value.eq_ignore_ascii_case("VEVENT") {
            out.push(format!("SUMMARY:{}", escape_text(&r.title)));
            out.push(dtstart.clone());
            out.push(dtend.clone());
            in_event = false;
            patched = true;
            out.push(line.clone());
            continue;
        }
        if in_event && matches!(name.as_str(), "SUMMARY" | "DTSTART" | "DTEND" | "DURATION") {
            continue;
        }
        out.push(line.clone());
    }
    if !patched {
        return Err("no VEVENT to patch".into());
    }
    let mut s = out.join("\r\n");
    s.push_str("\r\n");
    Ok(s)
}

/// Random-enough UID from the kernel, no uuid dependency.
fn new_uid() -> String {
    let mut bytes = [0u8; 16];
    if std::fs::File::open("/dev/urandom")
        .and_then(|mut f| std::io::Read::read_exact(&mut f, &mut bytes))
        .is_err()
    {
        return format!("CCE-{}", Utc::now().format("%Y%m%dT%H%M%S%fZ"));
    }
    let hex: String = bytes.iter().map(|b| format!("{b:02X}")).collect();
    format!("CCE-{}-{}-{}", &hex[..8], &hex[8..16], &hex[16..])
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
    recurring: bool,
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
                recurring,
            });
        }
    };

    match to_local(dtstart) {
        (date, Some(time)) => push(date, Some(time)),
        (start, None) => {
            // All-day: DTEND is exclusive per RFC 5545; a missing one means
            // a single day. One untimed record per covered day. A multi-day
            // all-day event is several records of one uid, which the file
            // cannot edit as one thing — so it is mirrored read-only too.
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

#[cfg(test)]
mod tests {
    use super::*;

    fn record(date: &str, time: Option<&str>, title: &str) -> EventRecord {
        EventRecord {
            date: date.into(),
            time: time.map(String::from),
            title: title.into(),
            uid: None,
            source: None,
            recurring: false,
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
        assert!(!events[0].has_rrule && !events[0].has_recurrence_id);
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
        event_to_records(&ev, window, "icloud:x", false, &mut seen, &mut out);
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
        event_to_records(ev, window, "s", false, &mut seen, &mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn new_vevent_all_day_and_timed() {
        let all_day = new_vevent("U1", &record("2026-09-10", None, "Trip, day")).unwrap();
        assert!(all_day.contains("DTSTART;VALUE=DATE:20260910"));
        assert!(all_day.contains("DTEND;VALUE=DATE:20260911"));
        assert!(all_day.contains("SUMMARY:Trip\\, day"));
        let timed = new_vevent("U2", &record("2026-09-10", Some("09:30"), "Call")).unwrap();
        // UTC form, an hour long; the exact hour depends on the local zone.
        assert!(timed.contains("DTSTART:20260910T") || timed.contains("DTSTART:20260911T"));
        assert!(timed.contains("Z\r\nDTEND:"));
    }

    #[test]
    fn patch_keeps_foreign_properties_and_replaces_the_span() {
        let ics = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:u\r\nDTSTART;TZID=America/New_York:20261001T090000\r\nDURATION:PT30M\r\nSUMMARY:old\r\nBEGIN:VALARM\r\nTRIGGER:-PT10M\r\nEND:VALARM\r\nX-APPLE-TRAVEL:1\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let patched = patch_vevent(&unfold(ics), &record("2026-10-02", None, "new")).unwrap();
        assert!(patched.contains("BEGIN:VALARM"));
        assert!(patched.contains("X-APPLE-TRAVEL:1"));
        assert!(patched.contains("SUMMARY:new"));
        assert!(patched.contains("DTSTART;VALUE=DATE:20261002"));
        assert!(!patched.contains("SUMMARY:old"));
        assert!(!patched.contains("DURATION"));
        assert!(!patched.contains("TZID"));
    }

    #[test]
    fn google_body_shapes() {
        let all_day = google_body(&record("2026-09-10", None, "x")).unwrap();
        assert_eq!(all_day["start"]["date"], "2026-09-10");
        assert_eq!(all_day["end"]["date"], "2026-09-11");
        let timed = google_body(&record("2026-09-10", Some("09:30"), "x")).unwrap();
        assert!(timed["start"]["dateTime"].as_str().unwrap().starts_with("2026-09-10T09:30:00"));
    }

    #[test]
    fn synced_event_matches_on_the_editable_triple() {
        let s = SyncedEvent {
            source: "icloud:a".into(),
            url: "u".into(),
            etag: "e".into(),
            date: "2026-09-10".into(),
            time: Some("09:30".into()),
            title: "x".into(),
        };
        assert!(s.matches(&record("2026-09-10", Some("09:30"), "x")));
        assert!(!s.matches(&record("2026-09-10", Some("10:30"), "x")));
        assert!(!s.matches(&record("2026-09-10", Some("09:30"), "y")));
    }
}
