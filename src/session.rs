//! Session persistence: a pragmatic subset of jterm4's tabs.state.
//!
//! jterm4 serializes a full nested split tree (`PaneLayout`) per tab. jterm1's
//! model keeps panes in a flat `Vec`, and nested-split restoration is deferred
//! (a known P10 parity gap), so here we persist only what restores cleanly: per
//! tab the title, whether it was user-renamed, the terminal mode and the active
//! pane's cwd, plus the active tab index. The snapshot is written as JSON and
//! consumed (deleted) on load, matching jterm4's consume-on-start semantics.

use gtk4::glib;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

use crate::config::TerminalMode;

#[derive(Serialize, Deserialize, Debug, Clone)]
pub(crate) struct SavedTab {
    pub title: String,
    pub custom_title: bool,
    /// "vte" or "block".
    pub mode: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub(crate) struct SavedSession {
    pub active: usize,
    pub tabs: Vec<SavedTab>,
}

impl SavedTab {
    pub(crate) fn terminal_mode(&self) -> TerminalMode {
        match self.mode.as_str() {
            "vte" => TerminalMode::Vte,
            _ => TerminalMode::Block,
        }
    }
}

pub(crate) fn state_file_path() -> PathBuf {
    glib::user_config_dir().join("jterm1").join("tabs.state")
}

pub(crate) fn save_session(session: &SavedSession) {
    if session.tabs.is_empty() {
        return;
    }
    let path = state_file_path();
    if let Some(parent) = path.parent() {
        if let Err(err) = fs::create_dir_all(parent) {
            log::error!("Failed to create state dir {}: {err}", parent.display());
            return;
        }
    }
    let payload = match serde_json::to_string(session) {
        Ok(p) => p,
        Err(err) => {
            log::error!("Failed to serialize session: {err}");
            return;
        }
    };
    // Write atomically to avoid a half-written snapshot on interruption.
    let tmp = path.with_extension("state.tmp");
    if let Err(err) = fs::write(&tmp, &payload) {
        log::error!("Failed to write temp state {}: {err}", tmp.display());
        return;
    }
    if fs::rename(&tmp, &path).is_err() {
        let _ = fs::remove_file(&path);
        if let Err(err) = fs::rename(&tmp, &path) {
            log::error!("Failed to move state into place {}: {err}", path.display());
            let _ = fs::remove_file(&tmp);
        }
    }
}

/// Load and consume (delete) the saved session, if any.
pub(crate) fn load_session() -> Option<SavedSession> {
    let path = state_file_path();
    let contents = fs::read_to_string(&path).ok()?;
    // Consume-on-start: each instance writes its own snapshot on change; the last
    // one to write wins, and the file is removed once read so it restores once.
    let _ = fs::remove_file(&path);
    match serde_json::from_str::<SavedSession>(&contents) {
        Ok(session) if !session.tabs.is_empty() => Some(session),
        Ok(_) => None,
        Err(err) => {
            log::warn!("Failed to parse session state: {err}");
            None
        }
    }
}
