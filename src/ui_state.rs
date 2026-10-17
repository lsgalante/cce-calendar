//! Where the calendar was, so it comes back there: the selected day (and so
//! the month shown) is saved whenever it changes, and a launch opens on it —
//! after a restart (the remote's "Restart focused app", `ccectl
//! restart-app`), at login, or when reopened, however many days later.
//!
//! **A restart replays the same command line**, so the file also records what
//! the command line asked for (`launched`): the same request again, or none,
//! is this window reopening, and the saved day wins; a different one is a new
//! request, and it wins. The calendar takes no arguments today, so `launched`
//! is always null — the rule is here so one added later is not overridden by
//! a stale file.
//!
//! `cce_state_dir()/calendar/state.json`, written whole to a temporary file
//! and renamed over, so a crash mid-write leaves the last one.

use std::path::PathBuf;

use chrono::NaiveDate;
use serde::{Deserialize, Serialize};

const DATE: &str = "%Y-%m-%d";

#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
struct Saved {
    /// The selected day, `YYYY-MM-DD`; the month shown is its month.
    selected: String,
    /// What the command line asked for when this was saved, if anything.
    launched: Option<String>,
}

fn path() -> PathBuf {
    cce_ui::config::cce_state_dir().join("calendar").join("state.json")
}

/// The day to open on: the saved one unless this launch asks for something
/// other than what it was saved under; else none (the caller opens on today).
pub fn initial() -> Option<NaiveDate> {
    let saved = std::fs::read_to_string(path()).ok().and_then(|s| serde_json::from_str::<Saved>(&s).ok());
    choose(None, saved.as_ref())
}

fn choose(launched: Option<&str>, saved: Option<&Saved>) -> Option<NaiveDate> {
    saved
        .filter(|s| launched.is_none() || s.launched.as_deref() == launched)
        .and_then(|s| NaiveDate::parse_from_str(&s.selected, DATE).ok())
}

/// The selected day changed.
pub fn save(selected: NaiveDate) {
    let saved = Saved { selected: selected.format(DATE).to_string(), launched: None };
    let path = path();
    let Some(dir) = path.parent() else { return };
    let tmp = path.with_extension("json.tmp");
    let written = std::fs::create_dir_all(dir)
        .and_then(|()| std::fs::write(&tmp, serde_json::to_vec(&saved).unwrap_or_default()))
        .and_then(|()| std::fs::rename(&tmp, &path));
    if let Err(e) = written {
        log::warn!("cannot save the selected day to {}: {e}", path.display());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn saved(day: &str, launched: Option<&str>) -> Saved {
        Saved { selected: day.into(), launched: launched.map(Into::into) }
    }

    fn day(s: &str) -> NaiveDate {
        NaiveDate::parse_from_str(s, DATE).unwrap()
    }

    #[test]
    fn the_same_command_line_or_none_reopens_on_the_saved_day() {
        // A plain launch, then a plain relaunch.
        assert_eq!(choose(None, Some(&saved("2026-03-14", None))), Some(day("2026-03-14")));
        // Launched asking for something, restarted with the same request.
        assert_eq!(choose(Some("x"), Some(&saved("2026-03-14", Some("x")))), Some(day("2026-03-14")));
        // Launched asking for something, reopened plainly.
        assert_eq!(choose(None, Some(&saved("2026-03-14", Some("x")))), Some(day("2026-03-14")));
    }

    #[test]
    fn a_different_request_or_nothing_usable_opens_on_today() {
        // The command line asks for something else: it wins.
        assert_eq!(choose(Some("y"), Some(&saved("2026-03-14", Some("x")))), None);
        assert_eq!(choose(Some("y"), Some(&saved("2026-03-14", None))), None);
        // Nothing saved, or a day that does not exist.
        assert_eq!(choose(None, None), None);
        assert_eq!(choose(None, Some(&saved("2026-02-30", None))), None);
        assert_eq!(choose(None, Some(&saved("", None))), None);
    }
}
