//! XKB operations: rules-property parsing, layout resolution, and keyboard
//! group switching over the X server via `x11rb`.

use std::fmt;
use std::os::unix::io::{AsRawFd, RawFd};

use x11rb::connection::Connection;
use x11rb::errors::{ConnectError, ConnectionError, ReplyError};
use x11rb::protocol::xproto::{
    AtomEnum, ChangeWindowAttributesAux, ConnectionExt as XprotoConnectionExt, EventMask, ModMask,
};
use x11rb::protocol::{
    Event,
    xkb::{ConnectionExt as XkbConnectionExt, Group},
};
use x11rb::rust_connection::RustConnection;

use crate::state::DEFAULT_LAYOUT;

/// `XkbUseCoreKbd` device spec: apply XKB requests to the core keyboard.
const USE_CORE_KBD: u16 = 0x100;

/// Parses the raw bytes of the root window `_XKB_RULES_NAMES` property into
/// layout names.
///
/// The property consists of five NUL-separated string fields
/// (rules, model, layouts, variants, options); field index 2 holds the
/// comma-separated layout list. Empty layout entries are filtered out.
pub fn parse_rules_names(prop: &[u8]) -> Vec<String> {
    let layouts_field = prop.split(|&byte| byte == 0).nth(2).unwrap_or(&[]);
    String::from_utf8_lossy(layouts_field)
        .split(',')
        .filter(|layout| !layout.is_empty())
        .map(|layout| layout.to_string())
        .collect()
}

/// Resolves a layout name to its group index within `layouts`.
pub fn resolve(name: &str, layouts: &[String]) -> Option<u8> {
    layouts
        .iter()
        .position(|layout| layout == name)
        .and_then(|index| u8::try_from(index).ok())
}

/// Like [`resolve`], but falls back to [`DEFAULT_LAYOUT`] when `name` is
/// unknown, and to group 0 when even the default layout is missing.
pub fn resolve_or_default(name: &str, layouts: &[String]) -> u8 {
    resolve(name, layouts)
        .or_else(|| resolve(DEFAULT_LAYOUT, layouts))
        .unwrap_or(0)
}

/// Errors produced by XKB/X11 operations.
#[derive(Debug)]
pub enum XkbError {
    /// Failed to connect to the X server.
    Connect(ConnectError),
    /// An X11 protocol/connection error.
    X11(ReplyError),
    /// The XKB extension is not supported by the X server.
    NoXkb,
    /// Any other unexpected failure.
    #[allow(dead_code)] // error surface completeness; not produced yet
    Unknown(String),
}

impl fmt::Display for XkbError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Connect(e) => write!(f, "failed to connect to X server: {e}"),
            Self::X11(e) => write!(f, "X11 protocol error: {e}"),
            Self::NoXkb => write!(f, "XKB extension is not supported by the X server"),
            Self::Unknown(msg) => write!(f, "unknown XKB error: {msg}"),
        }
    }
}

impl std::error::Error for XkbError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Connect(e) => Some(e),
            Self::X11(e) => Some(e),
            Self::NoXkb | Self::Unknown(_) => None,
        }
    }
}

impl From<ConnectError> for XkbError {
    fn from(e: ConnectError) -> Self {
        Self::Connect(e)
    }
}

impl From<ReplyError> for XkbError {
    fn from(e: ReplyError) -> Self {
        Self::X11(e)
    }
}

impl From<ConnectionError> for XkbError {
    fn from(e: ConnectionError) -> Self {
        Self::X11(e.into())
    }
}

/// Outcome of draining pending X events.
#[derive(Debug, Default)]
pub struct DrainReport {
    /// A PropertyNotify for the root `_XKB_RULES_NAMES` property arrived
    /// (setxkbmap re-run or `i3 reload`/`restart` rewrote the layout list).
    pub rules_changed: bool,
    /// Async X protocol errors (e.g. from failed void requests) found while
    /// draining; the caller logs and ignores them.
    pub x_errors: Vec<String>,
}

/// Live XKB state handle over an X server connection.
pub struct Xkb {
    conn: RustConnection,
    root: u32,
}

impl Xkb {
    /// Connects to the X server, enables the XKB extension, and remembers the
    /// root window of the first screen.
    pub fn connect() -> Result<Self, XkbError> {
        let (conn, screen) = RustConnection::connect(None)?;
        let root = conn.setup().roots[screen].root;
        let supported = conn.xkb_use_extension(1, 0)?.reply()?.supported;
        if !supported {
            return Err(XkbError::NoXkb);
        }
        Ok(Self { conn, root })
    }

    /// Reads layout names from the root window `_XKB_RULES_NAMES` property.
    pub fn layouts(&mut self) -> Result<Vec<String>, XkbError> {
        let atom = self
            .conn
            .intern_atom(false, b"_XKB_RULES_NAMES")?
            .reply()?
            .atom;
        let prop = self
            .conn
            .get_property(false, self.root, atom, AtomEnum::STRING, 0, u32::MAX)?
            .reply()?;
        Ok(parse_rules_names(&prop.value))
    }

    /// Returns the currently locked keyboard group (active layout index).
    pub fn locked_group(&mut self) -> Result<u8, XkbError> {
        let reply = self.conn.xkb_get_state(USE_CORE_KBD)?.reply()?;
        Ok(reply.locked_group.into())
    }

    /// Locks the group corresponding to `name`, falling back to
    /// [`DEFAULT_LAYOUT`] on unknown names.
    ///
    /// `lock_group` is a void request: protocol errors arrive asynchronously
    /// on the connection and are drained by the run loop.
    pub fn set_group(&mut self, name: &str) -> Result<(), XkbError> {
        let layouts = self.layouts()?;
        let group = resolve_or_default(name, &layouts);
        // The dedicated LockGroup request was folded into LatchLockState in
        // the current XKB protocol: lock only the group, touch no modifiers.
        self.conn.xkb_latch_lock_state(
            USE_CORE_KBD,        // device_spec: core keyboard
            ModMask::from(0u16), // affect_mod_locks: none
            ModMask::from(0u16), // mod_locks: none
            true,                // lock_group
            Group::from(group),  // group_lock
            ModMask::from(0u16), // affect_mod_latches: none
            false,               // latch_group
            0,                   // group_latch
        )?;
        Ok(())
    }

    /// Reads the i3 IPC socket path from the root window `I3_SOCKET_PATH`
    /// property; `None` when the property is missing or unreadable.
    pub fn read_i3_socket_path(&mut self) -> Option<String> {
        let atom = self
            .conn
            .intern_atom(false, b"I3_SOCKET_PATH")
            .ok()?
            .reply()
            .ok()?
            .atom;
        let prop = self
            .conn
            .get_property(false, self.root, atom, AtomEnum::STRING, 0, u32::MAX)
            .ok()?
            .reply()
            .ok()?;
        let path = String::from_utf8(prop.value).ok()?;
        let path = path.trim_end_matches('\0');
        if path.is_empty() {
            None
        } else {
            Some(path.to_string())
        }
    }

    /// Subscribes to root window property changes (for `_XKB_RULES_NAMES`
    /// updates) and flushes the request.
    pub fn watch_rules_changes(&mut self) -> Result<(), XkbError> {
        let aux = ChangeWindowAttributesAux::new().event_mask(EventMask::PROPERTY_CHANGE);
        self.conn.change_window_attributes(self.root, &aux)?;
        self.conn.flush()?;
        Ok(())
    }

    /// Flushes the connection's output buffer.
    ///
    /// Void requests such as [`Xkb::set_group`] are buffered locally until
    /// this is called; without a flush they never reach the server.
    pub fn flush(&self) -> Result<(), XkbError> {
        self.conn.flush()?;
        Ok(())
    }

    /// Drains all pending X events (after the connection fd was reported
    /// readable) and summarizes them in a [`DrainReport`].
    pub fn take_rules_change(&mut self) -> Result<DrainReport, XkbError> {
        let atom = self
            .conn
            .intern_atom(false, b"_XKB_RULES_NAMES")?
            .reply()?
            .atom;
        let mut report = DrainReport::default();
        while let Some(event) = self.conn.poll_for_event()? {
            match event {
                Event::PropertyNotify(notify) if notify.atom == atom => {
                    report.rules_changed = true;
                }
                Event::Error(err) => report.x_errors.push(format!("{err:?}")),
                _ => {} // events this daemon does not care about
            }
        }
        Ok(report)
    }

    /// Returns the raw file descriptor of the X connection for polling.
    pub fn poll_fd(&self) -> RawFd {
        self.conn.stream().as_raw_fd()
    }
}

#[cfg(test)]
mod tests {
    use super::{parse_rules_names, resolve, resolve_or_default};
    use crate::state::DEFAULT_LAYOUT;

    /// Layouts parsed from the real `_XKB_RULES_NAMES` capture on this
    /// machine: rules=evdev, model=pc104, layouts="us,ru", variants="" and
    /// options="grp:alt_shift_toggle,caps:escape"; the property ends with a
    /// double NUL and the variants field is an empty string.
    #[test]
    fn parse_rules_names_real_capture() {
        let prop = b"evdev\0pc104\0us,ru\0\0grp:alt_shift_toggle,caps:escape\0\0";
        assert_eq!(
            parse_rules_names(prop),
            vec!["us".to_string(), "ru".to_string()]
        );
    }

    #[test]
    fn parse_rules_names_flipped_list() {
        let prop = b"evdev\0pc104\0ru,us\0\0grp:alt_shift_toggle,caps:escape\0\0";
        assert_eq!(
            parse_rules_names(prop),
            vec!["ru".to_string(), "us".to_string()]
        );
    }

    #[test]
    fn parse_rules_names_no_trailing_nul_and_empty_layouts() {
        // Truncated property (no trailing NUL) must not panic.
        assert_eq!(
            parse_rules_names(b"evdev\0pc104\0us,ru"),
            vec!["us".to_string(), "ru".to_string()]
        );
        // An empty layouts field yields no layouts.
        let prop = b"evdev\0pc104\0\0\0grp:alt_shift_toggle,caps:escape\0\0";
        assert!(parse_rules_names(prop).is_empty());
    }

    #[test]
    fn resolve_known_and_unknown() {
        let layouts = vec!["us".to_string(), "ru".to_string()];
        assert_eq!(resolve("ru", &layouts), Some(1));
        assert_eq!(resolve("de", &layouts), None);
    }

    /// Emulates the set_group policy: an unknown layout name falls back to
    /// `DEFAULT_LAYOUT`.
    #[test]
    fn resolve_unknown_name_falls_back() {
        let layouts = vec!["ru".to_string(), "us".to_string()];
        // "xx" is unknown -> DEFAULT_LAYOUT ("us") -> group 1 (not 0).
        assert_eq!(resolve_or_default("xx", &layouts), 1);
        assert_eq!(resolve(DEFAULT_LAYOUT, &layouts), Some(1));
        // No fallback possible -> group 0.
        assert_eq!(resolve_or_default("xx", &["ru".to_string()]), 0);
    }
}
