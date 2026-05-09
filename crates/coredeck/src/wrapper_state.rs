//! Per-wrapper persisted state.
//!
//! After a daemon restart the in-memory `SessionState` table is gone
//! and gets repopulated from hooks as the user interacts with each
//! wrapper. The mode LED therefore stays stuck at whatever the last
//! touched session's mode was until every other wrapper has had a
//! `statusLine` (or any other hook) fire — misleading even though
//! it's not catastrophic.
//!
//! Persisting `permission_mode` keyed by `wrapper_id` lets us seed
//! the LED correctly when the user switches active to a session the
//! daemon hasn't seen any hooks for yet. The file lives next to
//! soft-key presets in the platform's data dir; it's append-only in
//! practice (entries from dead local-mode wrappers — fresh UUID per
//! invocation — leak in but the file stays small enough that we
//! don't bother pruning).
//!
//! Disk I/O is best-effort: read/write failures log at warn and the
//! daemon falls back to "leave the LED alone", which is the
//! pre-persistence behaviour.
//!
//! All ops take `&self` so the store can live inside `DaemonState`
//! behind an Arc without an outer lock.
//!
//! Format: a flat `wrapper_id → permission_mode` JSON object.
//!
//! ```json
//! {
//!   "8f0c…": "acceptEdits",
//!   "1a3e…": "default"
//! }
//! ```

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

use tracing::warn;

const FILE_NAME: &str = "wrapper_state.json";

#[derive(Default)]
pub struct WrapperStateStore {
    inner: Mutex<HashMap<String, String>>,
}

impl WrapperStateStore {
    pub fn load() -> Self {
        let map = read_from_disk().unwrap_or_default();
        Self {
            inner: Mutex::new(map),
        }
    }

    /// Last-known `permission_mode` for `wrapper_id`, or `None` when
    /// we've never persisted one.
    pub fn permission_mode(&self, wrapper_id: &str) -> Option<String> {
        self.inner
            .lock()
            .ok()
            .and_then(|m| m.get(wrapper_id).cloned())
    }

    /// Update the persisted mode for `wrapper_id` and flush to disk.
    /// Skips the write when nothing changed. Disk failures log at
    /// warn — the in-memory copy still gets updated so the running
    /// daemon stays consistent.
    pub fn set_permission_mode(&self, wrapper_id: &str, mode: &str) {
        let snapshot = {
            let mut g = match self.inner.lock() {
                Ok(g) => g,
                Err(_) => return,
            };
            if g.get(wrapper_id).map(String::as_str) == Some(mode) {
                return;
            }
            g.insert(wrapper_id.to_string(), mode.to_string());
            g.clone()
        };
        if let Err(e) = write_to_disk(&snapshot) {
            warn!(error = %e, "failed to persist wrapper state");
        }
    }
}

fn data_path() -> Option<PathBuf> {
    let dirs = directories::ProjectDirs::from("com", "coredeck", "CoreDeck")?;
    Some(dirs.data_dir().join(FILE_NAME))
}

fn read_from_disk() -> Option<HashMap<String, String>> {
    let path = data_path()?;
    if !path.exists() {
        return None;
    }
    match std::fs::read_to_string(&path) {
        Ok(content) => match serde_json::from_str(&content) {
            Ok(map) => Some(map),
            Err(e) => {
                warn!(error = %e, path = ?path, "wrapper state file unparseable");
                None
            }
        },
        Err(e) => {
            warn!(error = %e, path = ?path, "wrapper state file unreadable");
            None
        }
    }
}

fn write_to_disk(map: &HashMap<String, String>) -> std::io::Result<()> {
    let Some(path) = data_path() else {
        return Ok(());
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let content = serde_json::to_string(map).map_err(std::io::Error::other)?;
    std::fs::write(&path, content)
}
