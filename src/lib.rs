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
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    let tmp = path.with_extension(format!("{ext}.tmp"));
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

// ── Accounts ──────────────────────────────────────────────────────────────

/// One entry of the shared `accounts.json` (owned by cce-system-interface,
/// read by cce-mail), reduced to the fields the calendar uses. Unknown
/// fields are ignored, so the readers cannot drift apart.
#[derive(Clone, Debug, serde::Deserialize)]
pub struct AccountOnDisk {
    pub email: String,
    #[serde(default)]
    pub imap: String,
    #[serde(default)]
    pub password: String,
    #[serde(default)]
    pub is_oauth: bool,
    #[serde(default)]
    pub refresh_token: Option<String>,
    #[serde(default)]
    pub client_id: Option<String>,
    #[serde(default)]
    pub client_secret: Option<String>,
}

impl AccountOnDisk {
    pub fn is_icloud(&self) -> bool {
        let host = self.imap.split(':').next().unwrap_or("");
        host.ends_with(".mail.me.com")
            || ["@icloud.com", "@me.com", "@mac.com"].iter().any(|d| self.email.ends_with(d))
    }

    /// A Google sign-in the sync can use: OAuth with a refresh token.
    pub fn is_google(&self) -> bool {
        self.is_oauth && self.refresh_token.as_deref().is_some_and(|t| !t.is_empty())
    }
}

pub fn read_accounts() -> Result<Vec<AccountOnDisk>, String> {
    let path = cce_ui::config::cce_config_dir().join("accounts.json");
    let text = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))
}

/// An account the sync mirrors, named as its records' `source` is.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct CalendarAccount {
    /// "icloud" or "google".
    pub kind: &'static str,
    pub email: String,
}

impl CalendarAccount {
    pub fn source(&self) -> String {
        format!("{}:{}", self.kind, self.email)
    }
}

/// The accounts the sync would visit, in its order: iCloud, then Google,
/// each in accounts.json order. (An iCloud account whose password is
/// missing from the keyring is listed but skipped by the sync.)
pub fn calendar_accounts() -> Result<Vec<CalendarAccount>, String> {
    let on_disk = read_accounts()?;
    let icloud = on_disk.iter().filter(|a| a.is_icloud()).map(|a| ("icloud", a));
    let google = on_disk.iter().filter(|a| a.is_google()).map(|a| ("google", a));
    Ok(icloud.chain(google).map(|(kind, a)| CalendarAccount { kind, email: a.email.clone() }).collect())
}

// ── Config ────────────────────────────────────────────────────────────────

/// Where events typed into the app are created. From
/// `~/.config/cce/cce-calendar/config.kdl`:
///
/// ```kdl
/// push-to "icloud"                      // first iCloud account's first event calendar
/// push-to "icloud" calendar="Home"      // a named one
/// push-to "google"                      // the first Google account's primary calendar
/// push-to "google" account="me@gmail.com"   // a particular account
/// push-to "none"                        // keep typed events local
/// ```
///
/// Absent, the sync picks iCloud if such an account exists, else Google.
/// The app's account menu writes this node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushTarget {
    /// "icloud", "google", or "none".
    pub kind: String,
    /// The account's email; `None` is the first account of `kind`.
    pub account: Option<String>,
    pub calendar: Option<String>,
}

pub fn config_path() -> PathBuf {
    cce_ui::config::get_app_config_path("cce-calendar")
}

fn read_config() -> Option<kdl::KdlDocument> {
    std::fs::read_to_string(config_path()).ok()?.parse().ok()
}

fn prop<'a>(node: &'a kdl::KdlNode, name: &str) -> Option<&'a str> {
    node.entries()
        .iter()
        .find(|e| e.name().map(|n| n.value()) == Some(name))
        .and_then(|e| e.value().as_string())
}

pub fn push_target_config() -> Option<PushTarget> {
    let doc = read_config()?;
    let node = doc.get("push-to")?;
    let kind = node.entries().iter().find(|e| e.name().is_none())?.value().as_string()?;
    Some(PushTarget {
        kind: kind.to_ascii_lowercase(),
        account: prop(node, "account").map(String::from),
        calendar: prop(node, "calendar").map(String::from),
    })
}

/// Rewrite config.kdl through `edit`, keeping every node it does not touch
/// (comments and layout included). A file that does not parse is left
/// alone rather than replaced.
pub fn edit_config(edit: impl FnOnce(&mut kdl::KdlDocument)) -> std::io::Result<()> {
    let path = config_path();
    let mut doc: kdl::KdlDocument = match std::fs::read_to_string(&path) {
        Ok(text) => text.parse().map_err(std::io::Error::other)?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => kdl::KdlDocument::new(),
        Err(e) => return Err(e),
    };
    edit(&mut doc);
    let mut text = doc.to_string();
    if !text.ends_with('\n') {
        text.push('\n');
    }
    atomic_write(&path, &text)
}

/// Replace the `push-to` node (or add one).
pub fn save_push_target(target: &PushTarget) -> std::io::Result<()> {
    let mut node = kdl::KdlNode::new("push-to");
    node.push(kdl::KdlEntry::new(target.kind.clone()));
    if let Some(account) = &target.account {
        node.push(kdl::KdlEntry::new_prop("account", account.clone()));
    }
    if let Some(calendar) = &target.calendar {
        node.push(kdl::KdlEntry::new_prop("calendar", calendar.clone()));
    }
    edit_config(|doc| set_node(doc, node))
}

/// Put `node` where the first node of its name stood (dropping any later
/// duplicates), or at the end.
fn set_node(doc: &mut kdl::KdlDocument, node: kdl::KdlNode) {
    let name = node.name().value().to_string();
    let nodes = doc.nodes_mut();
    match nodes.iter().position(|n| n.name().value() == name) {
        Some(i) => {
            nodes[i] = node;
            let mut seen = 0;
            nodes.retain(|n| n.name().value() != name || { seen += 1; seen == 1 });
        }
        None => nodes.push(node),
    }
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

    #[test]
    fn set_node_replaces_in_place_and_keeps_the_rest() {
        let mut doc: kdl::KdlDocument =
            "// mine\nweek-start \"sunday\"\npush-to \"icloud\" calendar=\"Home\"\npush-to \"google\"\n".parse().unwrap();
        let mut node = kdl::KdlNode::new("push-to");
        node.push(kdl::KdlEntry::new("google"));
        node.push(kdl::KdlEntry::new_prop("account", "me@gmail.com"));
        set_node(&mut doc, node);
        let mut hide = kdl::KdlNode::new("hide-account");
        hide.push(kdl::KdlEntry::new("icloud:me@icloud.com"));
        doc.nodes_mut().push(hide);
        let text = doc.to_string();
        assert_eq!(
            text,
            "// mine\nweek-start \"sunday\"\npush-to \"google\" account=\"me@gmail.com\"\nhide-account \"icloud:me@icloud.com\"\n"
        );
        let back: kdl::KdlDocument = text.parse().unwrap();
        assert_eq!(prop(back.get("push-to").unwrap(), "account"), Some("me@gmail.com"));
    }
}