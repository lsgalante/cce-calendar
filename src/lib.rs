//! Shared between the `cce-calendar` app and the `cce-calendar-sync` helper:
//! the on-disk event record and its file. Both binaries read and rewrite the
//! same `events.json`, so the record shape lives in one place.

use std::path::PathBuf;

/// The on-disk shape: a flat list keeps the file trivially mergeable and
/// greppable. `uid`/`source` are set only on records mirrored from a remote
/// calendar by `cce-calendar-sync`; hand-entered events carry neither. The
/// app preserves them through its own saves, and the sync helper replaces
/// exactly the records whose `source` matches the account it is syncing.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct EventRecord {
    pub date: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time: Option<String>,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uid: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

pub fn data_path() -> PathBuf {
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/share")
        })
        .join("cce/calendar/events.json")
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
    let path = data_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(records).unwrap_or_default())?;
    std::fs::rename(&tmp, &path)
}
