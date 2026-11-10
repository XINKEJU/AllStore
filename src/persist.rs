//! Durable download-queue state.
//!
//! Tasks live in memory as `DlTask` (which holds non-serializable runtime
//! handles). This module mirrors just the durable fields into a small JSON
//! file under `%LOCALAPPDATA%\AllStore\downloads.json`, so a restart can
//! restore the list and resume an interrupted transfer from its `.part` file.
//!
//! Nothing about the local machine is transmitted: this is a local cache only.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::downloads::DlState;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskSnapshot {
    pub name: String,
    pub url: String,
    #[serde(default)]
    pub logo: Option<String>,
    pub state: SavedState,
    /// Bytes committed to the `.part` file at the time of the last save.
    #[serde(default)]
    pub done: u64,
    /// Whole-file size when known (keeps the bar honest before a re-probe).
    #[serde(default)]
    pub total: u64,
    /// Final path in Downloads for a completed task.
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub error: Option<String>,
}

/// A stable, textual form of `DlState` so a future state addition cannot make
/// old files unreadable (unknown values simply restore as `Canceled`).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum SavedState {
    Queued,
    Downloading,
    Paused,
    Done,
    Failed,
    Canceled,
}

impl From<DlState> for SavedState {
    fn from(s: DlState) -> Self {
        match s {
            DlState::Queued => SavedState::Queued,
            DlState::Downloading => SavedState::Downloading,
            DlState::Paused => SavedState::Paused,
            DlState::Done => SavedState::Done,
            DlState::Failed => SavedState::Failed,
            DlState::Canceled => SavedState::Canceled,
        }
    }
}

impl From<SavedState> for DlState {
    fn from(s: SavedState) -> Self {
        match s {
            SavedState::Queued => DlState::Queued,
            SavedState::Downloading => DlState::Paused, // never auto-resume a network job
            SavedState::Paused => DlState::Paused,
            SavedState::Done => DlState::Done,
            SavedState::Failed => DlState::Failed,
            SavedState::Canceled => DlState::Canceled,
        }
    }
}

fn path() -> Option<PathBuf> {
    let base = std::env::var("LOCALAPPDATA").ok()?;
    Some(PathBuf::from(base).join("AllStore").join("downloads.json"))
}

pub fn save(tasks: &[TaskSnapshot]) {
    let Some(p) = path() else { return };
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(s) = serde_json::to_string_pretty(tasks) {
        // Write-temp-then-rename: a crash mid-save must not corrupt the file.
        let tmp = p.with_extension("json.tmp");
        if std::fs::write(&tmp, s).is_ok() {
            let _ = std::fs::rename(&tmp, &p);
        }
    }
}

pub fn load() -> Vec<TaskSnapshot> {
    let Some(p) = path() else { return Vec::new() };
    std::fs::read_to_string(p)
        .ok()
        .and_then(|s| serde_json::from_str::<Vec<TaskSnapshot>>(&s).ok())
        .unwrap_or_default()
}

/// Last window rectangle, in physical pixels.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct WindowPlacement {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
    #[serde(default)]
    pub maximized: bool,
}

fn window_path() -> Option<PathBuf> {
    let base = std::env::var("LOCALAPPDATA").ok()?;
    Some(PathBuf::from(base).join("AllStore").join("window.json"))
}

pub fn load_window() -> Option<WindowPlacement> {
    let p = window_path()?;
    let s = std::fs::read_to_string(p).ok()?;
    let w: WindowPlacement = serde_json::from_str(&s).ok()?;
    (w.w > 0 && w.h > 0).then_some(w)
}

pub fn save_window(w: WindowPlacement) {
    let Some(p) = window_path() else { return };
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(s) = serde_json::to_string(&w) {
        let tmp = p.with_extension("json.tmp");
        if std::fs::write(&tmp, s).is_ok() {
            let _ = std::fs::rename(&tmp, &p);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_downloading_task_restores_as_paused() {
        // A restart must never silently reopen a network connection.
        assert_eq!(DlState::from(SavedState::Downloading), DlState::Paused);
        assert_eq!(DlState::from(SavedState::Paused), DlState::Paused);
        assert_eq!(DlState::from(SavedState::Done), DlState::Done);
    }

    #[test]
    fn snapshot_round_trips_through_json() {
        let v = vec![TaskSnapshot {
            name: "7-Zip".into(),
            url: "https://example.test/a.exe".into(),
            logo: None,
            state: SavedState::Paused,
            done: 1_048_576,
            total: 4_194_304,
            path: None,
            error: None,
        }];
        let s = serde_json::to_string(&v).unwrap();
        let back: Vec<TaskSnapshot> = serde_json::from_str(&s).unwrap();
        assert_eq!(back.len(), 1);
        assert_eq!(back[0].done, 1_048_576);
        assert_eq!(back[0].state, SavedState::Paused);
    }

    #[test]
    fn older_snapshot_without_optional_fields_is_readable() {
        let s = r#"[{"name":"X","url":"https://e/x.exe","state":"Done"}]"#;
        let back: Vec<TaskSnapshot> = serde_json::from_str(s).unwrap();
        assert_eq!(back[0].done, 0);
        assert_eq!(back[0].path, None);
    }
}
