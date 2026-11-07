//! Per-pane (tmux) keyboard layout state and its JSON persistence.
//!
//! Mirrors `state.rs`'s per-window memory, keyed by the numeric part of a
//! tmux pane id (`%NN`). The on-disk pane state file format is:
//! `{"version": 1, "last_pane": <id|null>, "layouts": {"<id>": "<name>"}}`.

use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::state::{DEFAULT_LAYOUT, atomic_write};

/// Pane state-file schema version written by [`PaneLayouts::save_sync`].
const PANE_STATE_VERSION: u32 = 1;

/// Helper DTO mirroring the pane state-file JSON format (serde derive).
#[derive(Debug, Serialize, Deserialize)]
struct PaneStateFile {
    version: u32,
    last_pane: Option<u32>,
    /// `serde_json` writes integer map keys as JSON strings and parses them back.
    layouts: HashMap<u32, String>,
}

/// Per-pane keyboard layout state.
#[derive(Debug, Default, Clone)]
pub struct PaneLayouts {
    /// tmux pane id -> layout name.
    pub map: HashMap<u32, String>,
    /// Id of the last focused pane, if known.
    pub last_pane: Option<u32>,
}

impl PaneLayouts {
    /// Loads pane state from `path`.
    ///
    /// A missing, unreadable, or malformed pane state file yields an empty
    /// state (fresh start) instead of panicking.
    pub fn load(path: &Path) -> Self {
        let Ok(text) = fs::read_to_string(path) else {
            return Self::default();
        };
        match serde_json::from_str::<PaneStateFile>(&text) {
            Ok(file) => Self {
                map: file.layouts,
                last_pane: file.last_pane,
            },
            Err(_) => Self::default(),
        }
    }

    /// Saves pane state to `path` synchronously as JSON, atomically via
    /// [`atomic_write`] (tmp write + fsync + rename, so the previous pane
    /// state file always survives a failed save). Shares the debounced
    /// flush + shutdown flush cadence with the window state.
    pub fn save_sync(&self, path: &Path) -> io::Result<()> {
        let file = PaneStateFile {
            version: PANE_STATE_VERSION,
            last_pane: self.last_pane,
            layouts: self.map.clone(),
        };
        let text = serde_json::to_string(&file).map_err(io::Error::other)?;
        atomic_write(path, &text)
    }

    /// Returns the stored layout for `pane`, or [`DEFAULT_LAYOUT`].
    pub fn layout_for(&self, pane: u32) -> &str {
        self.map
            .get(&pane)
            .map(String::as_str)
            .unwrap_or(DEFAULT_LAYOUT)
    }
}

/// Save-at-switch bookkeeping for tmux panes (pane analogue of the
/// window-level decision core in [`crate::daemon`]).
pub struct PaneCore {
    /// Per-pane layout state, also used by the persistence layer.
    pub state: PaneLayouts,
    /// Pane focus is currently on; the layout save target when focus
    /// moves away. Private: mutated only through [`PaneCore::on_focus`].
    prev_pane: Option<u32>,
}

impl PaneCore {
    /// Wraps already-loaded state (e.g. restored from the pane state file).
    ///
    /// `state.last_pane` is trusted as-is; `prev_pane` starts `None`
    /// because no pane event has been seen yet in this session.
    pub fn new(state: PaneLayouts) -> Self {
        Self {
            state,
            prev_pane: None,
        }
    }

    /// Handles a pane-focus-in event for `pane`.
    ///
    /// Returns the layout name the caller must apply for the newly focused
    /// pane, or `None` when focus was already on `pane` (duplicate pane
    /// event, e.g. terminal refocus): nothing is applied and no state is
    /// mutated, so re-application cannot race the window layer's apply.
    ///
    /// Save-at-switch: the layout currently observed belongs to the pane
    /// focus is leaving, so it is recorded into the previous pane before
    /// switching away.
    pub fn on_focus(&mut self, pane: u32, current_layout: &str) -> Option<String> {
        if self.prev_pane == Some(pane) {
            return None;
        }
        if let Some(prev) = self.prev_pane {
            self.state.map.insert(prev, current_layout.to_string());
        }
        self.state.last_pane = Some(pane);
        self.prev_pane = Some(pane);
        Some(self.state.layout_for(pane).to_string())
    }

    /// Current save-at-switch anchor (test accessor).
    pub fn prev_pane(&self) -> Option<u32> {
        self.prev_pane
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use super::{DEFAULT_LAYOUT, PaneCore, PaneLayouts};

    /// Builds a unique temporary file path under the system temp directory.
    fn temp_state_path(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock is past the Unix epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "i3_kb_switch_panes_{tag}_{}_{}.json",
            std::process::id(),
            nanos,
        ));
        let _ = fs::remove_file(&path); // start from a clean slate
        path
    }

    #[test]
    fn roundtrip_persists_map_and_last_pane() {
        let path = temp_state_path("roundtrip");

        let mut state = PaneLayouts::default();
        state.map.insert(1, "ru".to_string());
        state.map.insert(2, "de".to_string());
        state.last_pane = Some(2);

        state.save_sync(&path).expect("save_sync must succeed");

        let loaded = PaneLayouts::load(&path);
        assert_eq!(loaded.map, state.map);
        assert_eq!(loaded.last_pane, Some(2));

        let _ = fs::remove_file(&path);
    }

    #[test]
    fn load_missing_file_returns_empty() {
        let path = temp_state_path("missing");

        let state = PaneLayouts::load(&path);

        assert!(state.map.is_empty());
        assert_eq!(state.last_pane, None);
    }

    /// A malformed pane state file (garbage instead of JSON) must yield an
    /// empty state (fresh start) instead of panicking.
    #[test]
    fn load_malformed_file_returns_empty() {
        let path = temp_state_path("malformed");

        fs::write(&path, b"{this is not json").expect("malformed file write");

        let state = PaneLayouts::load(&path);
        assert!(state.map.is_empty());
        assert_eq!(state.last_pane, None);

        let _ = fs::remove_file(&path);
    }

    /// Save-at-switch: when focus moves from pane 1 to pane 2, the layout
    /// currently observed ("us") is recorded into pane 1, and the stored
    /// layout of pane 2 is returned for the caller to apply.
    #[test]
    fn on_focus_saves_current_to_prev_and_returns_new_stored_layout() {
        let mut core = PaneCore::new(PaneLayouts::default());
        core.state.map.insert(1, "ru".to_string());
        core.state.map.insert(2, "de".to_string());

        core.on_focus(1, "ru"); // first event: no prev, skips the save
        let applied = core.on_focus(2, "us");

        assert_eq!(
            core.state.map.get(&1).map(String::as_str),
            Some("us"),
            "current layout must be saved into the previous pane"
        );
        assert_eq!(
            applied.as_deref(),
            Some("de"),
            "new pane's stored layout must be returned"
        );
        assert_eq!(core.prev_pane(), Some(2));
        assert_eq!(core.state.last_pane, Some(2));
    }

    #[test]
    fn on_focus_with_no_prev_skips_save_and_returns_layout() {
        let mut core = PaneCore::new(PaneLayouts::default());
        core.state.map.insert(7, "de".to_string());

        let applied = core.on_focus(7, "ru");

        assert_eq!(applied.as_deref(), Some("de"));
        assert_eq!(
            core.state.map.get(&7).map(String::as_str),
            Some("de"),
            "no prev pane: current layout must not overwrite the stored one"
        );
        assert_eq!(core.prev_pane(), Some(7));
        assert_eq!(core.state.last_pane, Some(7));
    }

    #[test]
    fn layout_for_unknown_pane_returns_default() {
        let core = PaneCore::new(PaneLayouts::default());

        assert_eq!(core.state.layout_for(123), DEFAULT_LAYOUT);
        assert_eq!(core.state.layout_for(123), "us");
    }

    /// A duplicate pane-focus-in for the already-active pane (terminal
    /// refocus) must not re-apply a layout and must not mutate state:
    /// re-applying would race the window layer's apply.
    #[test]
    fn on_focus_same_pane_returns_none_without_mutation() {
        let mut core = PaneCore::new(PaneLayouts::default());
        core.state.map.insert(1, "ru".to_string());
        core.on_focus(1, "ru");
        let map_before = core.state.map.clone();
        let last_pane_before = core.state.last_pane;

        let applied = core.on_focus(1, "us");

        assert_eq!(applied, None, "duplicate pane event must not apply");
        assert_eq!(
            core.state.map.get(&1).map(String::as_str),
            Some("ru"),
            "current layout must not overwrite the stored one"
        );
        assert_eq!(core.state.map, map_before);
        assert_eq!(core.state.last_pane, last_pane_before);
        assert_eq!(core.prev_pane(), Some(1));
    }

    #[test]
    fn last_pane_follows_latest_focus() {
        let mut core = PaneCore::new(PaneLayouts::default());

        core.on_focus(1, "us");
        assert_eq!(core.state.last_pane, Some(1));
        core.on_focus(2, "ru");
        assert_eq!(core.state.last_pane, Some(2));
        core.on_focus(7, "de");
        assert_eq!(core.state.last_pane, Some(7));
    }
}
