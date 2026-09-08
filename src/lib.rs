//! Shared between the `cce-calendar` app and the `cce-calendar-sync` helper:
//! the on-disk event record and its file, the sync-state sidecar, and the
//! push-target config. Both binaries read and rewrite the same
//! `events.json`, so the record shape lives in one place.

use std::collections::BTreeMap;
use std::path::PathBuf;

/// The on-disk shape: a flat list keeps the file trivially mergeable and
/// greppable. `uid`/`source` are set only on records mirrored from a remote
/// calendar by `cce-calendar-sync`; hand-entered events carry neither until
/// the sync has pushed them to the default calendar and written the identity
/// back. `recurring` marks an expanded instance of a repeating event: those
/// are mirrored read-only (deleting one here would mean nothing the server
/// could express), and the app refuses to delete them.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct EventRecord {
    pub date: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time: Option<String>,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uid: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub recurring: bool,
}

fn data_dir() -> PathBuf {
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/share")
        })
        .join("cce/calendar")
}

pub fn data_path() -> PathBuf {
    data_dir().join("events.json")
}

pub fn sync_state_path() -> PathBuf {
    data_dir().join("sync-state.json")
}

pub fn load_records() -> std::io::Result<Vec<EventRecord>> {
    let text = match std::fs::read_to_string(data_path()) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    serde_json::from_str(&text).map_err(std::io::Error::other)
}

/// Write-temp-then-rename in the same directory, so a crash mid-write never
/// leaves a truncated file — two writers (app and sync timer) share this file.
pub fn save_records(records: &[EventRecord]) -> std::io::Result<()> {
    atomic_write(&data_path(), &serde_json::to_string_pretty(records).unwrap_or_default())
}

pub fn atomic_write(path: &std::path::Path, content: &str) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, content)?;
    std::fs::rename(&tmp, path)
}

/// Stable order for the file: by date, timed before untimed, then title.
pub fn sort_records(records: &mut [EventRecord]) {
    records.sort_by(|a, b| {
        (&a.date, a.time.is_none(), &a.time, &a.title).cmp(&(&b.date, b.time.is_none(), &b.time, &b.title))
    });
}

// ── Sync state (cce-calendar-sync's merge base; the app never touches it) ─

/// What the server held for one NON-recurring event at the end of the last
/// sync. Comparing the file and the server against this tells "the user
/// changed it here" apart from "it changed on the phone"; a uid in the state
/// but missing from the file is a local deletion to push.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SyncedEvent {
    /// "icloud:<email>" / "google:<email>".
    pub source: String,
    /// Absolute resource URL (PUT/PATCH/DELETE target).
    pub url: String,
    pub etag: String,
    pub date: String,
    #[serde(default)]
    pub time: Option<String>,
    pub title: String,
}

impl SyncedEvent {
    pub fn matches(&self, r: &EventRecord) -> bool {
        self.date == r.date && self.time == r.time && self.title == r.title
    }
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct SyncState {
    #[serde(default)]
    pub events: BTreeMap<String, SyncedEvent>,
}

pub fn load_sync_state() -> std::io::Result<SyncState> {
    let text = match std::fs::read_to_string(sync_state_path()) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(SyncState::default()),
        Err(e) => return Err(e),
    };
    serde_json::from_str(&text).map_err(std::io::Error::other)
}

pub fn save_sync_state(state: &SyncState) -> std::io::Result<()> {
    atomic_write(&sync_state_path(), &serde_json::to_string_pretty(state).unwrap_or_default())
}

// ── Config ────────────────────────────────────────────────────────────────

/// Where events typed into the app are created. From
/// `~/.config/cce/cce-calendar/config.kdl`:
///
/// ```kdl
/// push-to "icloud"                      // first iCloud event calendar
/// push-to "icloud" calendar="Home"      // a named one
/// push-to "google"                      // the Google primary calendar
/// push-to "none"                        // keep typed events local
/// ```
///
/// Absent, the sync picks iCloud if such an account exists, else Google.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushTarget {
    /// "icloud", "google", or "none".
    pub kind: String,
    pub calendar: Option<String>,
}

pub fn push_target_config() -> Option<PushTarget> {
    let path = cce_ui::config::get_app_config_path("cce-calendar");
    let text = std::fs::read_to_string(path).ok()?;
    let doc = text.parse::<kdl::KdlDocument>().ok()?;
    let node = doc.get("push-to")?;
    let kind = node.entries().iter().find(|e| e.name().is_none())?.value().as_string()?;
    let calendar = node
        .entries()
        .iter()
        .find(|e| e.name().map(|n| n.value()) == Some("calendar"))
        .and_then(|e| e.value().as_string())
        .map(String::from);
    Some(PushTarget { kind: kind.to_ascii_lowercase(), calendar })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recurring_flag_is_optional_on_disk() {
        let plain: EventRecord = serde_json::from_str(r#"{"date":"2026-09-10","title":"x"}"#).unwrap();
        assert!(!plain.recurring);
        let json = serde_json::to_string(&plain).unwrap();
        assert!(!json.contains("recurring"));
        let rec = EventRecord { recurring: true, ..plain };
        assert!(serde_json::to_string(&rec).unwrap().contains("\"recurring\":true"));
    }
}
