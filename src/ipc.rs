//! Minimal hand-rolled i3 IPC client.
//!
//! Implements the i3 IPC wire protocol directly (verified against i3
//! 4.25.1): every frame is the 6-byte magic string `i3-ipc`, a native-endian
//! u32 payload length, a native-endian u32 message type, then `length` bytes
//! of JSON payload. i3 uses the host byte order (on x86_64 that is little
//! endian), so encoding/decoding always goes through native `*_ne_bytes`
//! conversions instead of assuming LE explicitly.
//!
//! After [`Ipc::subscribe_window`] succeeds, replies and events interleave
//! on the same socket; [`Ipc::read_message`] just returns the next frame and
//! callers route by `kind` / `is_event`.

use std::collections::HashSet;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::UnixStream;

use serde::de::Error as _;

/// Magic string starting every i3 IPC frame, both directions.
pub const MAGIC: &[u8; 6] = b"i3-ipc";

/// High bit set on the message type of event frames.
pub const EVENT_FLAG: u32 = 0x8000_0000;

// Protocol constants below are part of the documented i3 IPC surface;
// this daemon only needs SUBSCRIBE/GET_TREE/WINDOW_EVENT so far.
#[allow(dead_code)]
pub const RUN_COMMAND: u32 = 0;
#[allow(dead_code)]
pub const GET_WORKSPACES: u32 = 1;
pub const SUBSCRIBE: u32 = 2;

/// Message type number carried by window event frames.
pub const WINDOW_EVENT: u32 = 3;

pub const GET_TREE: u32 = 4;

/// One decoded i3 IPC frame (a reply or an event).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    /// Message type with the event flag masked off (`& 0x7FFF_FFFF`).
    pub kind: u32,
    /// True when the frame carried the event flag.
    pub is_event: bool,
    /// Raw JSON payload bytes.
    pub body: Vec<u8>,
}

/// Errors produced by the i3 IPC client.
#[derive(Debug)]
pub enum IpcError {
    /// Underlying socket I/O failure.
    Io(std::io::Error),
    /// Frame did not start with the `i3-ipc` magic string.
    BadMagic,
    /// Peer closed the stream in the middle of a frame.
    Truncated,
    /// Invalid JSON payload, or a reply reporting failure.
    Json(serde_json::Error),
}

impl std::fmt::Display for IpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            IpcError::Io(e) => write!(f, "i3 ipc: io error: {e}"),
            IpcError::BadMagic => write!(f, "i3 ipc: bad magic string in frame"),
            IpcError::Truncated => write!(f, "i3 ipc: truncated frame"),
            IpcError::Json(e) => write!(f, "i3 ipc: json error: {e}"),
        }
    }
}

impl std::error::Error for IpcError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            IpcError::Io(e) => Some(e),
            IpcError::Json(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for IpcError {
    fn from(e: std::io::Error) -> Self {
        IpcError::Io(e)
    }
}

impl From<serde_json::Error> for IpcError {
    fn from(e: serde_json::Error) -> Self {
        IpcError::Json(e)
    }
}

/// A window event worth reacting to (layout switching cares about these).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WindowEvent {
    /// A window gained focus.
    Focus {
        /// X window id of the newly focused window.
        window: u32,
        /// WM_CLASS of the window, when the container carries properties.
        class: Option<String>,
    },
    /// A window was closed.
    Close {
        /// X window id of the closed window.
        window: u32,
    },
}

/// Connection to the i3 IPC socket (usually `$I3SOCK`).
pub struct Ipc {
    stream: UnixStream,
}

impl Ipc {
    /// Connect to the i3 IPC socket listening at `path`.
    pub fn connect(path: &str) -> Result<Self, IpcError> {
        Ok(Ipc {
            stream: UnixStream::connect(path)?,
        })
    }

    /// Raw socket file descriptor, for poll(2)-style event waiting.
    pub fn poll_fd(&self) -> RawFd {
        self.stream.as_raw_fd()
    }

    /// Write one frame: magic, native-endian length, native-endian type, body.
    fn send(&mut self, kind: u32, body: &[u8]) -> Result<(), IpcError> {
        self.stream.write_all(MAGIC)?;
        self.stream.write_all(&(body.len() as u32).to_ne_bytes())?;
        self.stream.write_all(&kind.to_ne_bytes())?;
        self.stream.write_all(body)?;
        Ok(())
    }

    /// `read_exact` that reports a mid-frame EOF as [`IpcError::Truncated`].
    ///
    /// UnixStream reads can return partial data; `read_exact` loops
    /// internally until the buffer is full, so it is the only read primitive
    /// needed here.
    fn read_exact_or_truncated(&mut self, buf: &mut [u8]) -> Result<(), IpcError> {
        match self.stream.read_exact(buf) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => Err(IpcError::Truncated),
            Err(e) => Err(IpcError::Io(e)),
        }
    }

    /// Read the next frame from the socket, whatever it is.
    pub fn read_message(&mut self) -> Result<Message, IpcError> {
        let mut magic = [0u8; 6];
        self.read_exact_or_truncated(&mut magic)?;
        if &magic != MAGIC {
            return Err(IpcError::BadMagic);
        }
        let mut len_buf = [0u8; 4];
        self.read_exact_or_truncated(&mut len_buf)?;
        let len = u32::from_ne_bytes(len_buf);
        let mut type_buf = [0u8; 4];
        self.read_exact_or_truncated(&mut type_buf)?;
        let raw_type = u32::from_ne_bytes(type_buf);
        let is_event = (raw_type & EVENT_FLAG) != 0;
        let kind = raw_type & !EVENT_FLAG;
        // The payload length comes from the wire and can be large (GET_TREE
        // replies reach hundreds of KB); allocate exactly what was promised.
        let mut body = vec![0u8; len as usize];
        self.read_exact_or_truncated(&mut body)?;
        Ok(Message {
            kind,
            is_event,
            body,
        })
    }

    /// Subscribe to i3 window events (`["window"]`).
    ///
    /// Skips any frames that are not the SUBSCRIBE reply — events and stale
    /// replies may interleave on the same socket before it arrives.
    pub fn subscribe_window(&mut self) -> Result<(), IpcError> {
        self.send(SUBSCRIBE, br#"["window"]"#)?;
        loop {
            let msg = self.read_message()?;
            if msg.is_event || msg.kind != SUBSCRIBE {
                continue;
            }
            let reply: serde_json::Value = serde_json::from_slice(&msg.body)?;
            if reply.get("success").and_then(serde_json::Value::as_bool) != Some(true) {
                return Err(IpcError::Json(serde_json::Error::custom(
                    "i3 rejected the [\"window\"] subscription",
                )));
            }
            return Ok(());
        }
    }

    /// Fetch the current i3 layout tree.
    ///
    /// Skips interleaved events until the GET_TREE reply arrives.
    pub fn get_tree(&mut self) -> Result<serde_json::Value, IpcError> {
        self.send(GET_TREE, b"")?;
        loop {
            let msg = self.read_message()?;
            if msg.is_event || msg.kind != GET_TREE {
                continue;
            }
            return Ok(serde_json::from_slice(&msg.body)?);
        }
    }
}

/// Numeric `window` field of a container, if it is a real X window id (> 0).
fn node_window(node: &serde_json::Value) -> Option<u32> {
    let window = node.get("window")?.as_u64()?;
    u32::try_from(window).ok().filter(|w| *w > 0)
}

/// Parse a window event payload (`change` + `container`).
///
/// Only focus and close events matter for layout switching; any other
/// `change` yields `None`. When `change` is `"focus"` but the container or
/// its window id is missing/null (the JSON schema allows it, though real i3
/// focus events always carry one), the event is dropped rather than guessed
/// at — without a window id there is nothing to switch layouts for.
pub fn parse_window_event(body: &[u8]) -> Option<WindowEvent> {
    let event: serde_json::Value = serde_json::from_slice(body).ok()?;
    let change = event.get("change")?.as_str()?;
    let container = event.get("container")?;
    match change {
        "focus" => {
            let window = node_window(container)?;
            let class = container
                .get("window_properties")
                .and_then(|props| props.get("class"))
                .and_then(|class| class.as_str())
                .map(str::to_string);
            Some(WindowEvent::Focus { window, class })
        }
        "close" => Some(WindowEvent::Close {
            window: node_window(container)?,
        }),
        _ => None,
    }
}

/// Find the id of the currently focused X window in the layout tree.
///
/// Depth-first pre-order over `nodes` and `floating_nodes`; returns the first
/// node marked `focused` that carries a real X window id (> 0). Focused
/// containers without a window (workspaces, split containers) do not count.
pub fn find_focused_window(tree: &serde_json::Value) -> Option<u32> {
    if tree.get("focused").and_then(|v| v.as_bool()) == Some(true)
        && let Some(window) = node_window(tree)
    {
        return Some(window);
    }
    for key in ["nodes", "floating_nodes"] {
        if let Some(children) = tree.get(key).and_then(|v| v.as_array()) {
            for child in children {
                if let Some(window) = find_focused_window(child) {
                    return Some(window);
                }
            }
        }
    }
    None
}

/// Collect every live X window id (> 0) present anywhere in the tree.
pub fn collect_live_windows(tree: &serde_json::Value, out: &mut HashSet<u32>) {
    if let Some(window) = node_window(tree) {
        out.insert(window);
    }
    for key in ["nodes", "floating_nodes"] {
        if let Some(children) = tree.get(key).and_then(|v| v.as_array()) {
            for child in children {
                collect_live_windows(child, out);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fake i3 server side: write one wire frame to the peer.
    fn write_frame(stream: &mut UnixStream, raw_kind: u32, body: &[u8]) -> std::io::Result<()> {
        stream.write_all(MAGIC)?;
        stream.write_all(&(body.len() as u32).to_ne_bytes())?;
        stream.write_all(&raw_kind.to_ne_bytes())?;
        stream.write_all(body)
    }

    /// Fake i3 server side: read one wire frame from the peer.
    fn read_frame(stream: &mut UnixStream) -> std::io::Result<(u32, Vec<u8>)> {
        let mut magic = [0u8; 6];
        stream.read_exact(&mut magic)?;
        let mut len_buf = [0u8; 4];
        stream.read_exact(&mut len_buf)?;
        let len = u32::from_ne_bytes(len_buf) as usize;
        let mut kind_buf = [0u8; 4];
        stream.read_exact(&mut kind_buf)?;
        let kind = u32::from_ne_bytes(kind_buf);
        let mut body = vec![0u8; len];
        stream.read_exact(&mut body)?;
        Ok((kind, body))
    }

    fn connected_pair() -> (Ipc, UnixStream) {
        let (client, server) = UnixStream::pair().unwrap();
        (Ipc { stream: client }, server)
    }

    /// Tree shape used by the focused-window and live-window tests: the
    /// focused node (window 42) sits deep inside `floating_nodes`, next to
    /// an unfocused tiled window (11).
    fn sample_tree() -> serde_json::Value {
        serde_json::json!({
            "id": 1,
            "type": "root",
            "focused": false,
            "window": null,
            "nodes": [
                {
                    "id": 2,
                    "type": "workspace",
                    "focused": false,
                    "window": null,
                    "nodes": [
                        {
                            "id": 3,
                            "type": "con",
                            "focused": false,
                            "window": 11,
                            "nodes": [],
                            "floating_nodes": []
                        }
                    ],
                    "floating_nodes": [
                        {
                            "id": 4,
                            "type": "floating_con",
                            "focused": true,
                            "window": 42,
                            "nodes": [],
                            "floating_nodes": []
                        }
                    ]
                }
            ],
            "floating_nodes": []
        })
    }

    #[test]
    fn read_message_decodes_framed_reply() {
        let (mut ipc, mut server) = connected_pair();
        let body: &[u8] = br#"[{"num":1,"name":"1"}]"#;
        write_frame(&mut server, GET_WORKSPACES, body).unwrap();
        let msg = ipc.read_message().unwrap();
        assert_eq!(msg.kind, GET_WORKSPACES);
        assert!(!msg.is_event);
        assert_eq!(msg.body, body);
    }

    #[test]
    fn read_message_rejects_truncated_and_bad_magic() {
        // Header promises a 64-byte body; the stream ends after 5 bytes.
        {
            let (mut ipc, mut server) = connected_pair();
            server.write_all(MAGIC).unwrap();
            server.write_all(&64u32.to_ne_bytes()).unwrap();
            server.write_all(&RUN_COMMAND.to_ne_bytes()).unwrap();
            server.write_all(b"short").unwrap();
            drop(server); // EOF for the client
            match ipc.read_message() {
                Err(IpcError::Truncated) => {}
                other => panic!("expected Truncated, got {other:?}"),
            }
        }
        // Wrong magic string.
        {
            let (mut ipc, mut server) = connected_pair();
            server.write_all(b"XXXXXX").unwrap();
            drop(server);
            match ipc.read_message() {
                Err(IpcError::BadMagic) => {}
                other => panic!("expected BadMagic, got {other:?}"),
            }
        }
    }

    #[test]
    fn read_message_masks_event_flag_and_sets_is_event() {
        let (mut ipc, mut server) = connected_pair();
        let body: &[u8] = br#"{"change":"focus"}"#;
        write_frame(&mut server, EVENT_FLAG | WINDOW_EVENT, body).unwrap();
        let msg = ipc.read_message().unwrap();
        assert_eq!(msg.kind, WINDOW_EVENT);
        assert_eq!(msg.kind, 3);
        assert!(msg.is_event);
    }

    #[test]
    fn parse_window_event_focus_with_class() {
        let body: &[u8] = br#"{
            "change": "focus",
            "container": {
                "id": 4194305,
                "window": 4194305,
                "window_properties": {
                    "class": "Alacritty",
                    "instance": "alacritty",
                    "title": "nvim"
                }
            }
        }"#;
        assert_eq!(
            parse_window_event(body),
            Some(WindowEvent::Focus {
                window: 4194305,
                class: Some("Alacritty".to_string()),
            })
        );
    }

    #[test]
    fn parse_window_event_close() {
        let body: &[u8] = br#"{
            "change": "close",
            "container": {
                "window": 9437184,
                "window_properties": { "class": "Firefox" }
            }
        }"#;
        assert_eq!(
            parse_window_event(body),
            Some(WindowEvent::Close { window: 9437184 })
        );
    }

    #[test]
    fn parse_window_event_ignores_uninteresting_changes() {
        let body: &[u8] = br#"{
            "change": "title",
            "container": { "window": 1, "window_properties": { "class": "X" } }
        }"#;
        assert_eq!(parse_window_event(body), None);
    }

    #[test]
    fn parse_window_event_drops_focus_without_container() {
        // Documented choice: a focus event without a usable container/window
        // id is dropped (None) — without a window id there is nothing to
        // switch layouts for.
        let body: &[u8] = br#"{"change":"focus"}"#;
        assert_eq!(parse_window_event(body), None);
    }

    #[test]
    fn parse_window_event_drops_null_window() {
        let body: &[u8] = br#"{"change":"close","container":{"window":null}}"#;
        assert_eq!(parse_window_event(body), None);
    }

    #[test]
    fn find_focused_window_deep_in_floating_nodes() {
        assert_eq!(find_focused_window(&sample_tree()), Some(42));
    }

    #[test]
    fn find_focused_window_ignores_focused_node_without_window() {
        // A focused workspace carries no X window id.
        let tree = serde_json::json!({
            "id": 1,
            "type": "workspace",
            "focused": true,
            "window": null,
            "nodes": [],
            "floating_nodes": []
        });
        assert_eq!(find_focused_window(&tree), None);
    }

    #[test]
    fn collect_live_windows_finds_all_window_ids() {
        let mut out = HashSet::new();
        collect_live_windows(&sample_tree(), &mut out);
        assert_eq!(out, HashSet::from([11, 42]));
    }

    #[test]
    fn subscribe_window_skips_interleaved_event_and_confirms_success() {
        let (mut ipc, mut server) = connected_pair();
        // Fake i3: an unrelated event shows up before the SUBSCRIBE reply.
        write_frame(
            &mut server,
            EVENT_FLAG | WINDOW_EVENT,
            br#"{"change":"title"}"#,
        )
        .unwrap();
        write_frame(&mut server, SUBSCRIBE, br#"{"success":true}"#).unwrap();
        ipc.subscribe_window().unwrap();
        // The client must have sent exactly the ["window"] subscription.
        let (kind, body) = read_frame(&mut server).unwrap();
        assert_eq!(kind, SUBSCRIBE);
        let expected: &[u8] = br#"["window"]"#;
        assert_eq!(body, expected);
    }

    #[test]
    fn subscribe_window_errors_when_i3_rejects() {
        let (mut ipc, mut server) = connected_pair();
        write_frame(&mut server, SUBSCRIBE, br#"{"success":false}"#).unwrap();
        assert!(ipc.subscribe_window().is_err());
    }

    #[test]
    fn get_tree_returns_parsed_tree() {
        let (mut ipc, mut server) = connected_pair();
        let tree = sample_tree();
        write_frame(&mut server, GET_TREE, tree.to_string().as_bytes()).unwrap();
        assert_eq!(ipc.get_tree().unwrap(), tree);
    }
}
