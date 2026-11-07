//! Library crate for i3_kb_switch: daemon, i3 IPC, per-window state and
//! XKB layout resolution. The binary in `main.rs` is a thin CLI wrapper.

pub mod daemon;
pub mod ipc;
pub mod panes;
pub mod sock;
pub mod state;
pub mod xkb;
