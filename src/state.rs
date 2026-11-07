//! Per-window keyboard layout state and its JSON persistence.
//!
//! The state maps i3 window ids to keyboard layout names and remembers the
//! last focused window. The on-disk state file format is:
//! `{"version": 1, "last_window": <id|null>, "layouts": {"<id>": "<name>"}}`.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Layout assumed for windows without a stored layout.
pub const DEFAULT_LAYOUT: &str = "us";

/// State-file schema version written by [`WindowLayouts::save_sync`].
const STATE_VERSION: u32 = 1;

/// Helper DTO mirroring the state-file JSON format (serde derive).
#[derive(Debug, Serialize, Deserialize)]
struct StateFile {
    version: u32,
    last_window: Option<u32>,
    /// `serde_json` writes integer map keys as JSON strings and parses them back.
    layouts: HashMap<u32, String>,
}

/// Per-window keyboard layout state.
#[derive(Debug, Default, Clone)]
pub struct WindowLayouts {
    /// Window id -> layout name.
    pub map: HashMap<u32, String>,
    /// Id of the last focused window, if known.
    pub last_window: Option<u32>,
}

/// Path of the temporary file used by [`atomic_write`] to write `path`
/// atomically (write + fsync there, then rename over `path`).
fn tmp_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".tmp");
    path.with_file_name(name)
}

/// Writes `text` to `path` atomically.
///
/// The text is written to `path` + `.tmp`, flushed to disk (`sync_all`),
/// and then renamed over `path`, so a crash or power event can never leave
/// a truncated file behind: `rename` is atomic, and any failure before it
/// keeps the previous file intact.
pub(crate) fn atomic_write(path: &Path, text: &str) -> io::Result<()> {
    use std::io::Write;

    let tmp = tmp_path(path);
    {
        let mut handle = fs::File::create(&tmp)?;
        handle.write_all(text.as_bytes())?;
        handle.sync_all()?;
    }
    fs::rename(&tmp, path)
}

impl WindowLayouts {
    /// Loads state from `path`.
    ///
    /// A missing, unreadable, or malformed state file yields an empty state
    /// (fresh start) instead of panicking.
    pub fn load(path: &Path) -> Self {
        let Ok(text) = fs::read_to_string(path) else {
            return Self::default();
        };
        match serde_json::from_str::<StateFile>(&text) {
            Ok(file) => Self {
                map: file.layouts,
                last_window: file.last_window,
            },
            Err(_) => Self::default(),
        }
    }

    /// Saves state to `path` synchronously as JSON, atomically via
    /// [`atomic_write`] (tmp write + fsync + rename, so the previous state
    /// file always survives a failed save). Called from the run loop's
    /// persistence (debounced tick and shutdown flush); unit tests
    /// exercise it directly.
    pub fn save_sync(&self, path: &Path) -> io::Result<()> {
        let file = StateFile {
            version: STATE_VERSION,
            last_window: self.last_window,
            layouts: self.map.clone(),
        };
        let text = serde_json::to_string(&file).map_err(io::Error::other)?;
        atomic_write(path, &text)
    }

    /// Drops stored layouts for windows that are no longer alive.
    ///
    /// `last_window` is cleared when that window is gone.
    pub fn retain_live(&mut self, live: &HashSet<u32>) {
        self.map.retain(|window, _| live.contains(window));
        if let Some(last) = self.last_window
            && !live.contains(&last)
        {
            self.last_window = None;
        }
    }

    /// Returns the stored layout for `window`, or [`DEFAULT_LAYOUT`].
    pub fn layout_for(&self, window: u32) -> &str {
        self.map
            .get(&window)
            .map(String::as_str)
            .unwrap_or(DEFAULT_LAYOUT)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, HashSet};
    use std::fs;
    use std::path::PathBuf;

    use super::DEFAULT_LAYOUT;
    use super::WindowLayouts;
    use super::tmp_path;

    /// Builds a unique temporary file path under the system temp directory.
    fn temp_state_path(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock is past the Unix epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "i3_kb_switch_{tag}_{}_{}.json",
            std::process::id(),
            nanos,
        ));
        let _ = fs::remove_file(&path); // start from a clean slate
        path
    }

    #[test]
    fn roundtrip_persists_map_and_last_window() {
        let path = temp_state_path("roundtrip");

        let mut state = WindowLayouts {
            map: HashMap::new(),
            last_window: None,
        };
        state.map.insert(42, "us".to_string());
        state.map.insert(7, "ru".to_string());
        state.last_window = Some(7);

        state.save_sync(&path).expect("save_sync must succeed");

        let loaded = WindowLayouts::load(&path);
        assert_eq!(loaded.map, state.map);
        assert_eq!(loaded.last_window, Some(7));

        let _ = fs::remove_file(&path);
    }

    #[test]
    fn load_missing_file_returns_empty() {
        let path = temp_state_path("missing");

        let state = WindowLayouts::load(&path);

        assert!(state.map.is_empty());
        assert_eq!(state.last_window, None);
    }

    /// A successful atomic save leaves no `.tmp` behind and a stale `.tmp`
    /// (e.g. from a killed older run) neither affects `load` nor survives
    /// the next save.
    #[test]
    fn save_atomic_leaves_no_tmp_and_overwrites_stale_tmp() {
        let path = temp_state_path("atomic");

        let mut state = WindowLayouts::default();
        state.map.insert(5, "ru".to_string());
        state.last_window = Some(5);

        state.save_sync(&path).expect("first save must succeed");
        let tmp = tmp_path(&path);
        assert!(!tmp.exists(), "successful save must remove the tmp file");
        assert_eq!(
            WindowLayouts::load(&path).map.get(&5).map(String::as_str),
            Some("ru"),
            "saved content must be readable back"
        );

        // A stale tmp file from a crashed run: load must ignore it ...
        fs::write(&tmp, b"garbage from a killed run").expect("stale tmp write");
        assert_eq!(
            WindowLayouts::load(&path).map.get(&5).map(String::as_str),
            Some("ru"),
            "load must read the state file, not the tmp file"
        );
        // ... and the next save must overwrite/remove it.
        state.map.insert(6, "us".to_string());
        state.save_sync(&path).expect("second save must succeed");
        assert!(!tmp.exists(), "save must not leave the tmp file behind");
        assert_eq!(
            WindowLayouts::load(&path).map.get(&6).map(String::as_str),
            Some("us")
        );

        let _ = fs::remove_file(&path);
    }

    /// When the save fails before the rename (here: the tmp path is a
    /// directory, so creating the tmp file fails — simulating a mid-write
    /// crash without an actual crash), the previous state file must stay
    /// intact and loadable.
    #[test]
    fn save_failure_keeps_previous_state_file() {
        let path = temp_state_path("crashsafe");
        let tmp = tmp_path(&path);

        let mut state = WindowLayouts::default();
        state.map.insert(9, "ru".to_string());
        state.last_window = Some(9);
        state.save_sync(&path).expect("initial save must succeed");

        fs::create_dir(&tmp).expect("tmp obstacle must be creatable");
        assert!(
            state.save_sync(&path).is_err(),
            "save must fail while the tmp path is occupied"
        );

        // The pre-failure state file survived and is still fully loadable.
        let loaded = WindowLayouts::load(&path);
        assert_eq!(loaded.map.get(&9).map(String::as_str), Some("ru"));
        assert_eq!(loaded.last_window, Some(9));

        fs::remove_dir(&tmp).expect("tmp obstacle removal");
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn retain_live_drops_dead_windows() {
        let mut state = WindowLayouts {
            map: HashMap::from([(1, "us".to_string()), (2, "ru".to_string())]),
            last_window: Some(2),
        };
        let live: HashSet<u32> = HashSet::from([1]);

        state.retain_live(&live);

        assert_eq!(state.map.len(), 1);
        assert!(state.map.contains_key(&1));
        assert_eq!(state.last_window, None);
    }

    #[test]
    fn layout_for_unknown_returns_default() {
        let state = WindowLayouts {
            map: HashMap::new(),
            last_window: None,
        };

        assert_eq!(state.layout_for(123), DEFAULT_LAYOUT);
        assert_eq!(state.layout_for(123), "us");
    }
}
