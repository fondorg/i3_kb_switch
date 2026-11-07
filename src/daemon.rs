//! Pure save-at-switch core of the daemon, plus the run loop and its
//! helpers.
//!
//! [`Core`] owns the per-window layout state and implements the daemon's
//! decision logic with no I/O of its own: the run loop ([`run`]) feeds it
//! i3 events and XKB layout reads, and applies whatever layout name
//! [`Core::on_focus`] tells it to.
//!
//! The central concept is the *previous window*: the window focus is
//! leaving. When focus moves to a different window, the layout currently
//! observed on X (`current_layout`) belongs to that previous window and is
//! saved into the map before the new window's layout is returned. Clearing
//! the anchor on close prevents resurrecting dead window ids in the map.
//!
//! The run loop polls three file descriptors — the i3 IPC socket, the X
//! connection, and the pane event socket ([`PaneLayer`]) — and reconnects
//! to i3 with an exponential backoff whenever the IPC connection breaks. A
//! change of the root window's
//! `_XKB_RULES_NAMES` property (setxkbmap re-run, `i3 reload`) triggers a
//! re-apply of the focused window's layout.
//!
//! State persistence is debounced: every mutation marks a
//! [`DebouncedSaver`] dirty and the loop writes the state file and the
//! pane state file at most once per second. SIGTERM/SIGINT only set an
//! atomic flag (via signal-hook); the loop notices it within one poll
//! tick, flushes both files one last time, and exits 0. A flock on
//! `<state_dir>/lock` keeps a second instance from starting.

use std::collections::HashSet;
use std::ffi::OsStr;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use libc::{POLLERR, POLLHUP, POLLIN};
use signal_hook::consts::{SIGINT, SIGTERM};
use x11rb::errors::ReplyError;

use crate::ipc::{
    Ipc, IpcError, Message, WINDOW_EVENT, WindowEvent, collect_live_windows, find_focused_window,
    parse_window_event,
};
use crate::panes::{PaneCore, PaneLayouts};
use crate::sock;
use crate::state::{DEFAULT_LAYOUT, WindowLayouts};
use crate::xkb::{Xkb, XkbError};

/// First delay between i3 reconnect attempts; doubles up to [`MAX_BACKOFF`].
const INITIAL_BACKOFF: Duration = Duration::from_millis(500);

/// Upper bound for the reconnect backoff.
const MAX_BACKOFF: Duration = Duration::from_secs(5);

/// Upper bound for the event-loop poll timeout, in milliseconds: the loop
/// wakes at least this often to check the shutdown flag and the debounce
/// deadline.
const POLL_TICK_MS: i32 = 200;

/// Debounce window for state persistence: at most one write per second.
const SAVE_DEBOUNCE: Duration = Duration::from_secs(1);

/// Sleep granularity while waiting out the i3 reconnect backoff, so a
/// shutdown signal is noticed promptly instead of after the full wait.
const SHUTDOWN_POLL_INTERVAL: Duration = Duration::from_millis(200);

/// `eprintln!`-style logging gated on the `-v` flag.
macro_rules! vlog {
    ($verbose:expr, $($arg:tt)*) => {
        if $verbose {
            eprintln!("i3_kb_switch: {}", format_args!($($arg)*));
        }
    };
}

/// Options controlling the daemon run loop.
pub struct RunOpts {
    /// Emit progress and event logs to stderr.
    pub verbose: bool,
}

/// Errors that can stop the daemon.
#[derive(Debug)]
pub enum DaemonError {
    /// XKB/X11 failure.
    Xkb(XkbError),
    /// i3 IPC failure.
    Ipc(IpcError),
    /// Filesystem failure (state directory/file).
    Io(io::Error),
    /// Neither the X root property, `i3 --get-socketpath`, nor `$I3SOCK`
    /// yielded an i3 IPC socket path.
    NoSocketPath,
}

impl fmt::Display for DaemonError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Xkb(e) => write!(f, "xkb: {e}"),
            Self::Ipc(e) => write!(f, "i3 ipc: {e}"),
            Self::Io(e) => write!(f, "io: {e}"),
            Self::NoSocketPath => write!(
                f,
                "could not resolve the i3 IPC socket path (root property, \
                 `i3 --get-socketpath`, $I3SOCK)"
            ),
        }
    }
}

impl std::error::Error for DaemonError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Xkb(e) => Some(e),
            Self::Ipc(e) => Some(e),
            Self::Io(e) => Some(e),
            Self::NoSocketPath => None,
        }
    }
}

impl From<XkbError> for DaemonError {
    fn from(e: XkbError) -> Self {
        Self::Xkb(e)
    }
}

impl From<IpcError> for DaemonError {
    fn from(e: IpcError) -> Self {
        Self::Ipc(e)
    }
}

impl From<io::Error> for DaemonError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

/// Next reconnect delay: doubles `prev`, capped at [`MAX_BACKOFF`].
///
/// The sequence starting from [`INITIAL_BACKOFF`] is 500ms, 1s, 2s, 4s,
/// 5s, 5s, ...
pub fn next_backoff(prev: Duration) -> Duration {
    prev.saturating_mul(2).min(MAX_BACKOFF)
}

/// Resolves the i3 IPC socket path.
///
/// Order: `xkb_result` (the root window `I3_SOCKET_PATH` property) → the
/// output of `i3 --get-socketpath` → the `I3SOCK` environment variable.
/// The first available source wins; `None` when no source works.
pub fn resolve_socket_path(xkb_result: Option<String>) -> Option<String> {
    if xkb_result.is_some() {
        return xkb_result;
    }
    if let Ok(output) = std::process::Command::new("i3")
        .arg("--get-socketpath")
        .output()
        && output.status.success()
    {
        let path = String::from_utf8_lossy(&output.stdout);
        let path = path.trim_end_matches(['\n', '\r']);
        if !path.is_empty() {
            return Some(path.to_string());
        }
    }
    std::env::var("I3SOCK").ok().filter(|path| !path.is_empty())
}

/// State directory for the state file.
///
/// `$XDG_STATE_HOME/i3_kb_switch` when `XDG_STATE_HOME` is set and
/// non-empty, `<home>/.local/state/i3_kb_switch` otherwise (XDG base
/// directory spec). When even `home` is missing, the process's working
/// directory is the last resort so the daemon still runs.
fn state_dir(home: Option<&OsStr>, xdg_state_home: Option<&OsStr>) -> PathBuf {
    match xdg_state_home.filter(|value| !value.is_empty()) {
        Some(xdg) => PathBuf::from(xdg).join("i3_kb_switch"),
        None => PathBuf::from(home.unwrap_or_else(|| OsStr::new(".")))
            .join(".local")
            .join("state")
            .join("i3_kb_switch"),
    }
}

/// State directory, creating it when missing.
fn state_dir_path() -> io::Result<PathBuf> {
    let dir = state_dir(
        std::env::var_os("HOME").as_deref(),
        std::env::var_os("XDG_STATE_HOME").as_deref(),
    );
    fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Path of the pane event socket: `<state_dir>/sock`, creating the state
/// directory when missing.
///
/// The daemon binds this socket and the CLI sends pane events to it, so
/// both resolve the path through this function.
pub fn socket_path() -> io::Result<PathBuf> {
    Ok(state_dir_path()?.join("sock"))
}

/// Single-instance guard: an exclusive, non-blocking `flock` on the lock
/// file in the state directory.
///
/// The lock is associated with the open file description owned by the
/// guard and is released when the guard (and with it the file) is
/// dropped, so no explicit unlock is needed.
pub struct SingleInstance {
    /// Keeps the lock file open for the guard's lifetime; closing it
    /// releases the flock.
    _file: File,
}

impl SingleInstance {
    /// Tries to take an exclusive, non-blocking lock on `path`.
    ///
    /// Returns the guard when this is the only instance, and `None` when
    /// another process already holds the lock (`EWOULDBLOCK`) or the
    /// lock file cannot be opened (treated the same way: never run
    /// twice).
    pub fn acquire(path: &Path) -> Option<Self> {
        let file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .ok()?;
        let locked = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        (locked == 0).then_some(Self { _file: file })
    }
}

/// Debounced persistence gate for the state file.
///
/// The run loop calls [`DebouncedSaver::mark_dirty`] whenever daemon
/// state changes; the actual `save_sync` happens only when
/// [`DebouncedSaver::should_save`] reports a due write (at most one per
/// [`SAVE_DEBOUNCE`]) or during the forced shutdown flush. The time
/// logic is pure with respect to the injected `now`, so tests need no
/// real delays.
#[derive(Debug, Default)]
pub struct DebouncedSaver {
    /// Time of the last accepted save; `None` until the first one.
    last: Option<Instant>,
    /// Unsaved state changes exist.
    dirty: bool,
}

impl DebouncedSaver {
    /// Creates an idle saver (clean, never saved).
    pub fn new() -> Self {
        Self::default()
    }

    /// Records that unsaved state changes exist.
    pub fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    /// Decides whether a debounced save is due at `now`.
    ///
    /// `true` only when dirty and at least [`SAVE_DEBOUNCE`] has passed
    /// since the last save (or none has happened yet). An accepted save
    /// consumes the dirty flag and stamps `last`; the caller performs the
    /// write and re-marks on failure.
    pub fn should_save(&mut self, now: Instant) -> bool {
        if !self.dirty {
            return false;
        }
        match self.last {
            Some(last) if now.duration_since(last) < SAVE_DEBOUNCE => false,
            _ => {
                self.dirty = false;
                self.last = Some(now);
                true
            }
        }
    }

    /// Writes `state` to `path` immediately, bypassing the debounce.
    ///
    /// On success the saver is clean and stamped; on failure the dirty
    /// flag is left set so a later tick retries.
    pub fn force_save(&mut self, state: &WindowLayouts, path: &Path) -> io::Result<()> {
        state.save_sync(path)?;
        self.dirty = false;
        self.last = Some(Instant::now());
        Ok(())
    }
}

/// True when an XKB error means the X connection itself is unusable and no
/// retry can help. Anything else (e.g. async protocol errors surfacing in
/// replies) is absorbed and logged.
fn xkb_fatal(err: &XkbError) -> bool {
    match err {
        XkbError::Connect(_) | XkbError::NoXkb => true,
        XkbError::X11(ReplyError::ConnectionError(_)) => true,
        XkbError::X11(_) | XkbError::Unknown(_) => false,
    }
}

/// What the event loop wants the enclosing reconnect loop to do next.
#[derive(Debug)]
enum LoopExit {
    /// The i3 connection broke: reconnect with backoff and resync.
    Reconnect(String),
    /// SIGTERM/SIGINT was received: flush state and stop.
    Shutdown,
    /// X itself is gone (or the daemon hit a fatal error): stop.
    Fatal(DaemonError),
}

/// Graceful-shutdown flush: when `shutdown` is set, writes `state` to
/// `path` and the pane state to `panes_path` (best effort — a failure is
/// logged but never blocks the exit, because state loss on kill is
/// acceptable).
///
/// Returns whether a shutdown was requested; the caller stops the loop
/// and exits 0.
pub fn handle_shutdown(
    shutdown: &AtomicBool,
    state: &WindowLayouts,
    path: &Path,
    panes: &PaneLayouts,
    panes_path: &Path,
) -> bool {
    if !shutdown.load(Ordering::Relaxed) {
        return false;
    }
    if let Err(err) = state.save_sync(path) {
        eprintln!("i3_kb_switch: shutdown flush failed: {err}");
    }
    if let Err(err) = panes.save_sync(panes_path) {
        eprintln!("i3_kb_switch: shutdown pane flush failed: {err}");
    }
    true
}

/// poll(2) timeout for the event loop: the next debounce deadline capped
/// at [`POLL_TICK_MS`], so the shutdown flag is checked regularly. A save
/// already due (dirty with no prior save, or overdue) wakes immediately;
/// a clean saver uses the plain tick.
fn poll_timeout_ms(saver: &DebouncedSaver, now: Instant) -> i32 {
    if !saver.dirty {
        return POLL_TICK_MS;
    }
    let remainder = match saver.last {
        None => Duration::ZERO,
        Some(last) => SAVE_DEBOUNCE.saturating_sub(now.duration_since(last)),
    };
    let tick = Duration::from_millis(POLL_TICK_MS as u64);
    remainder.min(tick).as_millis() as i32
}

/// Runs the daemon until the X connection dies, a fatal error occurs, or
/// SIGTERM/SIGINT request a graceful shutdown (final state flush, exit 0).
///
/// i3 disconnects (including `i3 exit`/restart) are recovered inside this
/// function via [`establish_i3`] with an exponential backoff. A second
/// daemon instance is prevented with a flock on `<state_dir>/lock`,
/// taken after the directory exists but before anything touches the
/// state file.
pub fn run(opts: RunOpts) -> Result<(), DaemonError> {
    let state_dir = state_dir_path()?;
    let _instance = match SingleInstance::acquire(&state_dir.join("lock")) {
        Some(instance) => instance,
        None => {
            eprintln!("i3_kb_switch: already running");
            return Ok(());
        }
    };
    let state_path = state_dir.join("state.json");
    let state = WindowLayouts::load(&state_path);
    let mut core = Core::new(state);
    vlog!(opts.verbose, "state loaded from {}", state_path.display());

    // Pane layer: panes.json lives next to state.json; the event socket
    // is optional (bind failure never stops the daemon).
    let panes_path = state_dir.join("panes.json");
    let panes_state = PaneLayouts::load(&panes_path);
    let sock_path = socket_path()?;
    let mut panes = PaneLayer::new(
        PaneCore::new(panes_state),
        panes_path,
        &sock_path,
        opts.verbose,
    );
    vlog!(
        opts.verbose,
        "pane state loaded from {}",
        panes.path.display()
    );

    // The signals only set an atomic flag; the run loop polls it and
    // performs the final state flush itself.
    let shutdown = Arc::new(AtomicBool::new(false));
    for signal in [SIGTERM, SIGINT] {
        signal_hook::flag::register(signal, Arc::clone(&shutdown))?;
    }
    vlog!(opts.verbose, "shutdown flag installed for SIGTERM/SIGINT");

    let mut saver = DebouncedSaver::new();
    let mut xkb = Xkb::connect()?;
    xkb.watch_rules_changes()?;
    vlog!(
        opts.verbose,
        "connected to X; watching _XKB_RULES_NAMES for changes"
    );

    let mut first = true;
    let mut backoff = INITIAL_BACKOFF;
    loop {
        if shutdown.load(Ordering::Relaxed) {
            break;
        }
        match establish_i3(&mut xkb, &mut core, &mut saver, first, opts.verbose) {
            Ok(mut ipc) => {
                // A successful subscribe resets the backoff.
                backoff = INITIAL_BACKOFF;
                let startup = first;
                first = false;
                // Startup-apply: persist the freshly synced states right
                // away so state.json and panes.json exist from the start.
                if startup {
                    if let Err(err) = saver.force_save(&core.state, &state_path) {
                        eprintln!("i3_kb_switch: initial state save failed: {err}");
                    }
                    if let Err(err) = panes.core.state.save_sync(&panes.path) {
                        eprintln!("i3_kb_switch: initial pane state save failed: {err}");
                        saver.mark_dirty(); // try again on a later tick
                    }
                }
                match event_loop(
                    &mut ipc,
                    &mut xkb,
                    &mut core,
                    &mut panes,
                    &mut saver,
                    &shutdown,
                    &state_path,
                    opts.verbose,
                ) {
                    LoopExit::Reconnect(reason) => {
                        vlog!(opts.verbose, "{reason}; reconnecting");
                    }
                    LoopExit::Shutdown => break,
                    LoopExit::Fatal(err) => return Err(err),
                }
            }
            Err(err) if establish_is_fatal(&err) => return Err(err),
            Err(err) => {
                // A dead X connection cannot heal by retrying i3; probe it
                // and stop when it is gone.
                if let Err(xerr) = xkb.flush()
                    && xkb_fatal(&xerr)
                {
                    return Err(DaemonError::Xkb(xerr));
                }
                vlog!(
                    opts.verbose,
                    "i3 connection failed, retrying in {backoff:?}: {err}"
                );
                // Sleep in slices so a shutdown signal ends the wait early.
                let deadline = Instant::now() + backoff;
                while Instant::now() < deadline && !shutdown.load(Ordering::Relaxed) {
                    thread::sleep(SHUTDOWN_POLL_INTERVAL);
                }
                if shutdown.load(Ordering::Relaxed) {
                    break;
                }
                backoff = next_backoff(backoff);
            }
        }
    }
    // Graceful stop (SIGTERM/SIGINT): best-effort final flush; a failure
    // is logged but the daemon still exits 0.
    handle_shutdown(
        &shutdown,
        &core.state,
        &state_path,
        &panes.core.state,
        &panes.path,
    );
    Ok(())
}

/// A failed (re)establish is fatal only when X itself is unusable; i3-side
/// failures are retried with backoff.
fn establish_is_fatal(err: &DaemonError) -> bool {
    match err {
        DaemonError::Xkb(xerr) => xkb_fatal(xerr),
        _ => false,
    }
}

/// (Re)establishes the i3 IPC connection and re-syncs state from the tree.
///
/// Steps: resolve the socket path fresh (root property first), connect,
/// subscribe to window events, fetch the tree, then resync [`Core`].
/// `filter` passes through to [`Core::resync_from_tree`]: startup prunes
/// dead window ids from the map (`true`), reconnects keep it (`false`)
/// because X window ids survive i3 restarts. When a focused window exists,
/// its stored layout is applied immediately (and flushed).
fn establish_i3(
    xkb: &mut Xkb,
    core: &mut Core,
    saver: &mut DebouncedSaver,
    filter: bool,
    verbose: bool,
) -> Result<Ipc, DaemonError> {
    let socket = resolve_socket_path(xkb.read_i3_socket_path()).ok_or(DaemonError::NoSocketPath)?;
    let mut ipc = Ipc::connect(&socket)?;
    ipc.subscribe_window()?;
    let tree = ipc.get_tree()?;
    let mut live = HashSet::new();
    collect_live_windows(&tree, &mut live);
    let focused = find_focused_window(&tree);
    core.resync_from_tree(focused, &live, filter);
    // The resync may have pruned dead ids or moved last_window: a change
    // worth persisting (via the startup save or the debounce tick).
    saver.mark_dirty();
    if let Some(window) = focused {
        let name = core.state.layout_for(window).to_string();
        xkb.set_group(&name)?;
        xkb.flush()?; // set_group does not flush: push the lock request out
        vlog!(verbose, "applied {name} to focused window {window}");
    }
    vlog!(
        verbose,
        "i3 IPC established at {socket} (subscribed to window events)"
    );
    Ok(ipc)
}

/// Per-pane layer of the daemon: pane layout state, its persistence path,
/// and the unix socket feeding it pane focus events.
///
/// Mirrors the window layer ([`Core`]): a `pane-focus-in` event delivered
/// over the socket triggers the same save-at-switch apply as an i3 focus
/// event ([`handle_focus`]). The socket is an optional feature: when
/// binding it fails, the layer keeps running with `listener: None` and
/// simply never sees pane events ([`sock::bind`] unlinks a stale socket
/// first, so a crash-restart is the only expected failure).
pub struct PaneLayer {
    /// Per-pane layout state, also used by the persistence layer.
    pub core: PaneCore,
    /// Path of the pane state file (`panes.json`).
    pub path: PathBuf,
    /// Listener for pane events; `None` when the socket could not be
    /// bound (pane support disabled for this run).
    listener: Option<UnixListener>,
}

impl PaneLayer {
    /// Creates the layer from already-loaded pane state and binds the
    /// event socket at `sock_path`.
    ///
    /// A bind failure is never fatal: it is logged (when `verbose`) and
    /// the layer continues with `listener: None` — [`PaneLayer::poll_fd`]
    /// then reports `-1` and no pane events are ever read.
    pub fn new(core: PaneCore, path: PathBuf, sock_path: &Path, verbose: bool) -> Self {
        let listener = match sock::bind(sock_path) {
            Ok(listener) => {
                vlog!(verbose, "pane socket listening at {}", sock_path.display());
                Some(listener)
            }
            Err(err) => {
                vlog!(
                    verbose,
                    "pane socket bind failed at {} ({err}); continuing without pane events",
                    sock_path.display()
                );
                None
            }
        };
        Self {
            core,
            path,
            listener,
        }
    }

    /// poll(2) fd for the pane socket: the listener's fd, or `-1` when
    /// there is no listener. poll(2) ignores negative fds, so the run
    /// loop can keep a fixed three-entry poll set.
    pub fn poll_fd(&self) -> i32 {
        self.listener
            .as_ref()
            .map_or(-1, |listener| listener.as_raw_fd())
    }

    /// Handles one readable pane-socket wake: accepts a single
    /// connection, reads one pane event, and mirrors [`handle_focus`]'s
    /// save-at-switch for panes: observe the current layout, let
    /// [`PaneCore::on_focus`] decide (duplicates and unparsable events do
    /// nothing), and apply the returned layout. X errors are routed
    /// fatal/absorbed exactly as there. Returns `Some(exit)` when the
    /// loop must stop.
    fn handle_readable(
        &mut self,
        xkb: &mut Xkb,
        saver: &mut DebouncedSaver,
        verbose: bool,
    ) -> Option<LoopExit> {
        let Some(listener) = self.listener.as_ref() else {
            return None; // no socket: this fd can never be reported ready
        };
        let Ok((mut stream, _)) = listener.accept() else {
            vlog!(verbose, "pane socket: accept failed, ignoring");
            return None;
        };
        let Some(pane) = sock::read_pane_event(&mut stream) else {
            vlog!(verbose, "pane socket: no parsable pane event, ignoring");
            return None;
        };
        // Observe the layout on the pane focus is leaving BEFORE
        // consulting the core: `on_focus` records it there when the
        // anchor moves.
        let current_name = match read_current_layout(xkb) {
            Ok(name) => name,
            Err(err) => {
                if xkb_fatal(&err) {
                    return Some(LoopExit::Fatal(DaemonError::Xkb(err)));
                }
                vlog!(
                    verbose,
                    "pane {pane}: cannot read current layout, skipping: {err}"
                );
                return None;
            }
        };
        if let Some(name) = self.core.on_focus(pane, &current_name) {
            saver.mark_dirty();
            match xkb.set_group(&name) {
                Ok(()) => {
                    if let Some(exit) = flush_or_exit(xkb, verbose) {
                        return Some(exit);
                    }
                    vlog!(verbose, "pane {pane}: applied {name}");
                }
                Err(err) if xkb_fatal(&err) => {
                    return Some(LoopExit::Fatal(DaemonError::Xkb(err)));
                }
                Err(err) => vlog!(
                    verbose,
                    "pane {pane}: set_group to {name} failed, continuing: {err}"
                ),
            }
        }
        None
    }
}

/// poll(2) loop over the i3 socket, the X connection, and the pane
/// socket.
///
/// Runs until the i3 connection breaks ([`LoopExit::Reconnect`]), X dies
/// ([`LoopExit::Fatal`]), or SIGTERM/SIGINT set the shutdown flag
/// ([`LoopExit::Shutdown`]). The poll timeout tracks the next debounce
/// deadline capped at [`POLL_TICK_MS`]; every wake re-checks the flag and
/// performs the debounced state save when one is due. The poll set is
/// rebuilt every iteration because the fds change across reconnects.
#[allow(clippy::too_many_arguments)] // explicit wiring beats a params struct here
fn event_loop(
    ipc: &mut Ipc,
    xkb: &mut Xkb,
    core: &mut Core,
    panes: &mut PaneLayer,
    saver: &mut DebouncedSaver,
    shutdown: &AtomicBool,
    state_path: &Path,
    verbose: bool,
) -> LoopExit {
    loop {
        let timeout_ms = poll_timeout_ms(saver, Instant::now());
        let mut fds = [
            libc::pollfd {
                fd: ipc.poll_fd(),
                events: POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: xkb.poll_fd(),
                events: POLLIN,
                revents: 0,
            },
            // No pane socket: fd -1, silently ignored by poll(2).
            libc::pollfd {
                fd: panes.poll_fd(),
                events: POLLIN,
                revents: 0,
            },
        ];
        let ready = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, timeout_ms) };
        match ready {
            -1 => {
                let err = io::Error::last_os_error();
                if err.kind() != io::ErrorKind::Interrupted {
                    return LoopExit::Reconnect(format!("poll failed: {err}"));
                }
                // EINTR (e.g. the signal handler firing): fall through so
                // the shutdown flag is re-checked before re-polling.
            }
            0 => {} // timeout: nothing pending
            _ => {
                // i3 socket: replies and events interleave; route by kind.
                if fds[0].revents & (POLLIN | POLLHUP | POLLERR) != 0 {
                    match ipc.read_message() {
                        Ok(msg) => {
                            if let Some(exit) = handle_frame(msg, xkb, core, saver, verbose) {
                                return exit;
                            }
                        }
                        Err(err) => {
                            // Any IpcError (including Truncated after a clean i3
                            // exit) means the connection is unusable: reconnect.
                            return LoopExit::Reconnect(err.to_string());
                        }
                    }
                }

                // X socket: drain pending events; react to rules-property changes.
                if fds[1].revents != 0 {
                    match xkb.take_rules_change() {
                        Ok(report) => {
                            for xerr in &report.x_errors {
                                vlog!(verbose, "async X error while draining events: {xerr}");
                            }
                            if report.rules_changed
                                && let Some(exit) = reapply_after_rules_change(xkb, core, verbose)
                            {
                                return exit;
                            }
                        }
                        Err(err) if xkb_fatal(&err) => {
                            return LoopExit::Fatal(DaemonError::Xkb(err));
                        }
                        Err(err) => {
                            vlog!(verbose, "non-fatal X error while draining events: {err}")
                        }
                    }
                }

                // Pane socket: one pane event per accepted connection.
                if fds[2].revents != 0
                    && let Some(exit) = panes.handle_readable(xkb, saver, verbose)
                {
                    return exit;
                }
            }
        }

        if shutdown.load(Ordering::Relaxed) {
            return LoopExit::Shutdown;
        }

        // Debounced persistence tick: at most one write per second, for
        // both the window and the pane state; a failure on either
        // re-marks dirty so a later tick retries.
        if saver.should_save(Instant::now()) {
            if let Err(err) = core.state.save_sync(state_path) {
                eprintln!("i3_kb_switch: state save failed: {err}");
                saver.mark_dirty(); // try again on a later tick
            } else {
                vlog!(verbose, "state saved to {}", state_path.display());
            }
            if let Err(err) = panes.core.state.save_sync(&panes.path) {
                eprintln!("i3_kb_switch: pane state save failed: {err}");
                saver.mark_dirty(); // try again on a later tick
            } else {
                vlog!(verbose, "pane state saved to {}", panes.path.display());
            }
        }
    }
}

/// Routes one decoded IPC frame; `Some(exit)` when the loop must stop.
fn handle_frame(
    msg: Message,
    xkb: &mut Xkb,
    core: &mut Core,
    saver: &mut DebouncedSaver,
    verbose: bool,
) -> Option<LoopExit> {
    if !msg.is_event || msg.kind != WINDOW_EVENT {
        return None; // replies and non-window events are not ours to act on
    }
    match parse_window_event(&msg.body) {
        Some(WindowEvent::Focus { window, class }) => {
            handle_focus(window, class.as_deref(), xkb, core, saver, verbose)
        }
        Some(WindowEvent::Close { window }) => {
            core.on_close(window);
            saver.mark_dirty();
            vlog!(verbose, "window {window} closed");
            None
        }
        None => None, // title and other uninteresting changes
    }
}

/// Applies the save-at-switch logic for a focus event.
fn handle_focus(
    window: u32,
    class: Option<&str>,
    xkb: &mut Xkb,
    core: &mut Core,
    saver: &mut DebouncedSaver,
    verbose: bool,
) -> Option<LoopExit> {
    // Observe the layout on the anchor (previous) window BEFORE consulting
    // the core: `on_focus` records it there when the anchor moves.
    let current_name = match read_current_layout(xkb) {
        Ok(name) => name,
        Err(err) => {
            if xkb_fatal(&err) {
                return Some(LoopExit::Fatal(DaemonError::Xkb(err)));
            }
            vlog!(
                verbose,
                "focus {window}: cannot read current layout, skipping: {err}"
            );
            return None;
        }
    };
    if let Some(name) = core.on_focus(window, class, &current_name) {
        saver.mark_dirty();
        match xkb.set_group(&name) {
            Ok(()) => {
                // set_group does not flush: without this the lock request
                // sits in the output buffer forever.
                if let Some(exit) = flush_or_exit(xkb, verbose) {
                    return Some(exit);
                }
                vlog!(
                    verbose,
                    "focus {window} ({}): applied {name}",
                    class.unwrap_or("?")
                );
            }
            Err(err) if xkb_fatal(&err) => return Some(LoopExit::Fatal(DaemonError::Xkb(err))),
            Err(err) => vlog!(
                verbose,
                "focus {window}: set_group to {name} failed, continuing: {err}"
            ),
        }
    }
    None
}

/// Locked-group index -> layout name via a fresh rules-property read; an
/// index outside the current layout list falls back to [`DEFAULT_LAYOUT`].
fn read_current_layout(xkb: &mut Xkb) -> Result<String, XkbError> {
    let group = xkb.locked_group()?;
    let layouts = xkb.layouts()?;
    Ok(layouts
        .get(usize::from(group))
        .cloned()
        .unwrap_or_else(|| DEFAULT_LAYOUT.to_string()))
}

/// Flushes the X connection; a dead connection becomes a fatal exit.
fn flush_or_exit(xkb: &Xkb, verbose: bool) -> Option<LoopExit> {
    if let Err(err) = xkb.flush() {
        if xkb_fatal(&err) {
            return Some(LoopExit::Fatal(DaemonError::Xkb(err)));
        }
        vlog!(verbose, "X flush failed, continuing: {err}");
    }
    None
}

/// Re-applies the anchor window's layout after the rules property changed
/// (setxkbmap re-run, `i3 reload`/`restart`): the layout list may have been
/// reordered, so the stored name is resolved freshly by set_group.
fn reapply_after_rules_change(xkb: &mut Xkb, core: &Core, verbose: bool) -> Option<LoopExit> {
    let Some(window) = core.prev_window() else {
        vlog!(
            verbose,
            "rules property changed: no focused window, nothing to re-apply"
        );
        return None;
    };
    let name = core.state.layout_for(window).to_string();
    if let Err(err) = xkb.set_group(&name) {
        if xkb_fatal(&err) {
            return Some(LoopExit::Fatal(DaemonError::Xkb(err)));
        }
        vlog!(
            verbose,
            "rules change: set_group to {name} failed, continuing: {err}"
        );
        return None;
    }
    if let Some(exit) = flush_or_exit(xkb, verbose) {
        return Some(exit);
    }
    vlog!(
        verbose,
        "rules property changed: re-applied {name} to window {window}"
    );
    None
}

/// Window classes whose focus events must be ignored entirely: no layout is
/// applied for them and the save-at-switch anchor (`prev_window`) is left
/// untouched.
pub const IGNORED_CLASSES: &[&str] = &["Rofi", "rofi"];

/// Pure decision core of the daemon: save-at-switch bookkeeping.
pub struct Core {
    /// Per-window layout state, also used by the persistence layer.
    pub state: WindowLayouts,
    /// Window focus is currently on; the layout save target when focus
    /// moves away. Private: mutated only through the event handlers.
    prev_window: Option<u32>,
}

impl Core {
    /// Wraps already-loaded state (e.g. restored from the state file).
    ///
    /// `state.last_window` is trusted as-is; `prev_window` starts `None`
    /// because no focus event has been seen yet in this session.
    pub fn new(state: WindowLayouts) -> Self {
        Self {
            state,
            prev_window: None,
        }
    }

    /// Handles a focus event for `window` of `class`.
    ///
    /// Returns the layout name the caller must apply, or `None` when the
    /// window's class is in [`IGNORED_CLASSES`] (nothing is applied and no
    /// state is mutated: map, `last_window`, and `prev_window` all stay
    /// as they were).
    pub fn on_focus(
        &mut self,
        window: u32,
        class: Option<&str>,
        current_layout: &str,
    ) -> Option<String> {
        if class.is_some_and(|name| IGNORED_CLASSES.contains(&name)) {
            return None;
        }
        // Save-at-switch: the layout currently observed on X belongs to the
        // window focus is leaving, so record it before switching away.
        if let Some(prev) = self.prev_window
            && prev != window
        {
            self.state.map.insert(prev, current_layout.to_string());
        }
        self.state.last_window = Some(window);
        self.prev_window = Some(window);
        Some(self.state.layout_for(window).to_string())
    }

    /// Handles a close event for `window`.
    ///
    /// The window's stored layout is dropped. When the closed window is the
    /// save-at-switch anchor, the anchor is cleared so a later focus switch
    /// cannot write a layout into a dead window id.
    ///
    /// `last_window` is deliberately left as-is even when it equals the
    /// closed window: it is persistence-layer state and the persistence
    /// task owns its lifecycle semantics.
    pub fn on_close(&mut self, window: u32) {
        self.state.map.remove(&window);
        if self.prev_window == Some(window) {
            self.prev_window = None;
        }
    }

    /// Reconciles state with the i3 window tree.
    ///
    /// With `filter == true` (daemon startup) the stored map is pruned to
    /// `live` window ids, because X is fresh and no old id is valid. With
    /// `filter == false` (i3 reconnect) the map is left untouched: X windows
    /// survive an i3 restart, so stored ids remain valid.
    ///
    /// In both cases the save-at-switch anchor is re-seeded from the tree's
    /// focused window, and `last_window` follows it when present.
    pub fn resync_from_tree(&mut self, focused: Option<u32>, live: &HashSet<u32>, filter: bool) {
        if filter {
            self.state.retain_live(live);
        }
        self.prev_window = focused;
        if let Some(focused) = focused {
            self.state.last_window = Some(focused);
        }
    }

    /// Current save-at-switch anchor (test accessor).
    pub fn prev_window(&self) -> Option<u32> {
        self.prev_window
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, HashSet};
    use std::ffi::OsStr;
    use std::fs;
    use std::os::fd::AsRawFd;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    use super::Core;
    use super::{
        DebouncedSaver, POLL_TICK_MS, PaneLayer, handle_shutdown, next_backoff, poll_timeout_ms,
        resolve_socket_path, socket_path, state_dir,
    };
    use crate::panes::{PaneCore, PaneLayouts};
    use crate::sock;
    use crate::state::WindowLayouts;

    /// Builds `WindowLayouts` from `(window, layout)` pairs.
    fn state_with(entries: &[(u32, &str)]) -> WindowLayouts {
        WindowLayouts {
            map: entries
                .iter()
                .map(|(window, layout)| (*window, layout.to_string()))
                .collect(),
            last_window: None,
        }
    }

    #[test]
    fn on_focus_saves_current_to_prev_and_returns_new() {
        // prev 1 ("us" in map), focus 2, current "ru":
        // map[1] must become "ru" and the layout for window 2 returned.
        let mut core = Core::new(state_with(&[(1, "us")]));
        core.on_focus(1, None, "us"); // parks the anchor on window 1

        let applied = core.on_focus(2, None, "ru");

        assert_eq!(core.state.map.get(&1).map(String::as_str), Some("ru"));
        assert_eq!(applied.as_deref(), Some(core.state.layout_for(2)));
        assert_eq!(applied.as_deref(), Some("us")); // 2 has no stored entry
        assert_eq!(core.prev_window(), Some(2));
        assert_eq!(core.state.last_window, Some(2));
    }

    #[test]
    fn on_focus_with_no_prev_skips_save() {
        // No previous window: nothing is saved, the new window's layout is
        // still returned.
        let mut core = Core::new(state_with(&[(5, "ru")]));

        let applied = core.on_focus(7, None, "de");

        assert_eq!(core.state.map, HashMap::from([(5, "ru".to_string())]));
        assert_eq!(applied.as_deref(), Some(core.state.layout_for(7)));
        assert_eq!(core.prev_window(), Some(7));
    }

    #[test]
    fn on_focus_same_window_skips_save_but_returns_layout() {
        // Repeat focus on the anchor window itself (prev == window): the
        // observed layout must NOT be saved into it, and its own layout is
        // still returned for the caller to apply.
        let mut core = Core::new(state_with(&[(1, "us")]));
        core.on_focus(1, None, "us"); // parks the anchor on window 1

        let applied = core.on_focus(1, None, "ru");

        assert_eq!(core.state.map.get(&1).map(String::as_str), Some("us"));
        assert_eq!(applied.as_deref(), Some("us"));
        assert_eq!(core.prev_window(), Some(1));
    }

    #[test]
    fn on_focus_ignored_class_returns_none_and_keeps_prev() {
        // "Rofi" focus events change nothing at all: no apply, no save, no
        // anchor movement.
        let mut core = Core::new(state_with(&[(1, "us")]));
        core.prev_window = Some(1);
        core.state.last_window = Some(1);
        let map_before = core.state.map.clone();

        let applied = core.on_focus(3, Some("Rofi"), "ru");

        assert_eq!(applied, None);
        assert_eq!(core.state.map, map_before);
        assert_eq!(core.prev_window(), Some(1));
        assert_eq!(core.state.last_window, Some(1));
    }

    #[test]
    fn on_close_of_prev_window_clears_it() {
        // Closing the anchor window drops its entry and clears the anchor,
        // so the next focus switch saves nothing into the dead id.
        let mut core = Core::new(state_with(&[(2, "ru")]));
        core.prev_window = Some(2);

        core.on_close(2);

        assert!(!core.state.map.contains_key(&2));
        assert_eq!(core.prev_window(), None);

        // The next switch must not resurrect window 2's entry.
        let applied = core.on_focus(3, None, "de");
        assert_eq!(applied.as_deref(), Some("us"));
        assert!(!core.state.map.contains_key(&2));
        assert!(!core.state.map.contains_key(&3));
    }

    #[test]
    fn on_close_other_keeps_prev() {
        // Closing a non-anchor window drops only that window's entry.
        let mut core = Core::new(state_with(&[(1, "us"), (2, "ru")]));
        core.prev_window = Some(1);

        core.on_close(2);

        assert_eq!(core.prev_window(), Some(1));
        assert!(!core.state.map.contains_key(&2));
        assert_eq!(core.state.map.get(&1).map(String::as_str), Some("us"));
    }

    #[test]
    fn resync_sets_prev_from_tree_and_filters() {
        let live: HashSet<u32> = HashSet::from([1, 2]);
        let focused = Some(2);

        // filter == true (startup): dead window ids are pruned.
        let mut core = Core::new(state_with(&[(99, "ru"), (1, "us")]));
        core.resync_from_tree(focused, &live, true);
        assert!(!core.state.map.contains_key(&99));
        assert_eq!(core.prev_window(), Some(2));
        assert_eq!(core.state.last_window, Some(2));

        // filter == false (reconnect): the map is untouched, X windows
        // survived the i3 restart.
        let mut core = Core::new(state_with(&[(99, "ru"), (1, "us")]));
        core.resync_from_tree(focused, &live, false);
        assert!(core.state.map.contains_key(&99));
        assert_eq!(core.state.map.get(&1).map(String::as_str), Some("us"));
        assert_eq!(core.prev_window(), Some(2));
        assert_eq!(core.state.last_window, Some(2));

        // No focused window in the tree: anchor stays empty.
        let mut core = Core::new(state_with(&[(1, "us")]));
        core.resync_from_tree(None, &live, true);
        assert_eq!(core.prev_window(), None);
        assert_eq!(core.state.last_window, None);
    }

    #[test]
    fn next_backoff_doubles_and_caps() {
        assert_eq!(
            next_backoff(Duration::from_millis(500)),
            Duration::from_secs(1)
        );
        assert_eq!(next_backoff(Duration::from_secs(1)), Duration::from_secs(2));
        assert_eq!(next_backoff(Duration::from_secs(2)), Duration::from_secs(4));
        assert_eq!(next_backoff(Duration::from_secs(4)), Duration::from_secs(5));
        assert_eq!(next_backoff(Duration::from_secs(5)), Duration::from_secs(5));
        // Past the cap: stays at 5s.
        assert_eq!(
            next_backoff(Duration::from_secs(10)),
            Duration::from_secs(5)
        );
    }

    #[test]
    fn socket_resolution_order() {
        // Priority 1: the Xkb-provided result (root property) wins outright,
        // no matter what the environment says.
        assert_eq!(
            resolve_socket_path(Some("/run/xkb.sock".to_string())).as_deref(),
            Some("/run/xkb.sock")
        );

        // Priority 3 (env I3SOCK) is reached by making priority 2
        // (`i3 --get-socketpath`) fail: PATH points at an empty directory.
        // Environment mutation is unsafe in edition 2024 and process-global;
        // no other test in this crate reads PATH/I3SOCK, and the original
        // values are restored below.
        let path = std::env::var("PATH").ok();
        let sock = std::env::var("I3SOCK").ok();
        unsafe {
            std::env::set_var("PATH", "/nonexistent-i3-kb-switch-test");
            std::env::set_var("I3SOCK", "/tmp/i3sock-env-test");
        }
        assert_eq!(
            resolve_socket_path(None).as_deref(),
            Some("/tmp/i3sock-env-test")
        );

        // Nothing resolvable at all: None.
        unsafe { std::env::remove_var("I3SOCK") };
        assert_eq!(resolve_socket_path(None), None);

        // Restore the process environment.
        unsafe {
            match path {
                Some(p) => std::env::set_var("PATH", p),
                None => std::env::remove_var("PATH"),
            }
            match sock {
                Some(s) => std::env::set_var("I3SOCK", s),
                None => std::env::remove_var("I3SOCK"),
            }
        }
    }

    #[test]
    fn debounce_saves_at_most_once_per_second() {
        let t0 = Instant::now();
        let mut saver = DebouncedSaver::new();

        // Dirty at t0 and never saved: due immediately.
        saver.mark_dirty();
        assert!(saver.should_save(t0));

        // Dirty again half a second later: suppressed by the debounce.
        saver.mark_dirty();
        assert!(!saver.should_save(t0 + Duration::from_millis(500)));

        // After the full debounce window: due again.
        saver.mark_dirty();
        assert!(saver.should_save(t0 + Duration::from_millis(1100)));
    }

    #[test]
    fn signal_flush_writes_state_and_panes() {
        let dir =
            std::env::temp_dir().join(format!("i3_kb_switch_shutdown_{}", std::process::id()));
        fs::create_dir_all(&dir).expect("temp dir creation must succeed");
        let path = dir.join("state.json");
        let panes_path = dir.join("panes.json");
        let state = state_with(&[(1, "us")]);
        let mut panes = PaneLayouts::default();
        panes.map.insert(3, "ru".to_string());
        panes.last_pane = Some(3);
        let shutdown = Arc::new(AtomicBool::new(false));

        // Flag clear: no flush, no files.
        assert!(!handle_shutdown(
            &shutdown,
            &state,
            &path,
            &panes,
            &panes_path
        ));
        assert!(!path.exists());
        assert!(!panes_path.exists());

        // Flag set: state and pane state written to the files.
        shutdown.store(true, Ordering::Relaxed);
        assert!(handle_shutdown(
            &shutdown,
            &state,
            &path,
            &panes,
            &panes_path
        ));
        assert!(path.exists());
        assert!(panes_path.exists());
        let restored = WindowLayouts::load(&path);
        assert_eq!(restored.map.get(&1).map(String::as_str), Some("us"));
        let restored_panes = PaneLayouts::load(&panes_path);
        assert_eq!(restored_panes.map.get(&3).map(String::as_str), Some("ru"));
        assert_eq!(restored_panes.last_pane, Some(3));

        let _ = fs::remove_dir_all(&dir);
    }

    /// Builds a unique temporary socket path under the system temp
    /// directory.
    fn pane_sock_path(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock is past the Unix epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "i3_kb_switch_daemon_sock_{tag}_{}_{}.sock",
            std::process::id(),
            nanos,
        ));
        let _ = fs::remove_file(&path); // start from a clean slate
        path
    }

    /// Without a listener (bind failed, pane support disabled) the layer
    /// must report fd -1: poll(2) ignores negative fds, so the loop can
    /// keep a fixed three-entry poll set.
    #[test]
    fn pane_layer_poll_fd_is_minus_one_without_listener() {
        let layer = PaneLayer {
            core: PaneCore::new(PaneLayouts::default()),
            path: PathBuf::from("/nonexistent/panes.json"),
            listener: None,
        };
        assert_eq!(layer.poll_fd(), -1);
    }

    /// With a bound listener the layer must report its real fd, so poll
    /// wakes the loop when a pane event connection arrives.
    #[test]
    fn pane_layer_poll_fd_is_listener_fd_with_listener() {
        let sock_path = pane_sock_path("pollfd");
        let listener = sock::bind(&sock_path).expect("pane socket bind must succeed");
        let expected = listener.as_raw_fd();

        let layer = PaneLayer {
            core: PaneCore::new(PaneLayouts::default()),
            path: PathBuf::from("/nonexistent/panes.json"),
            listener: Some(listener),
        };

        assert_eq!(layer.poll_fd(), expected);
        assert!(layer.poll_fd() >= 0);

        drop(layer);
        let _ = fs::remove_file(&sock_path);
    }

    #[test]
    fn socket_path_lives_in_state_dir() {
        // Environment mutation is unsafe in edition 2024 and process-global;
        // no other test in this crate reads XDG_STATE_HOME, and the original
        // value is restored below.
        let xdg = std::env::var("XDG_STATE_HOME").ok();
        let dir =
            std::env::temp_dir().join(format!("i3_kb_switch_sockpath_{}", std::process::id()));
        unsafe { std::env::set_var("XDG_STATE_HOME", &dir) };

        assert_eq!(
            socket_path().expect("socket_path must succeed"),
            dir.join("i3_kb_switch").join("sock")
        );

        unsafe {
            match xdg {
                Some(value) => std::env::set_var("XDG_STATE_HOME", value),
                None => std::env::remove_var("XDG_STATE_HOME"),
            }
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn poll_timeout_tracks_debounce_deadline_and_tick_cap() {
        let t0 = Instant::now();

        // Clean saver: plain tick so the shutdown flag stays polled.
        let mut saver = DebouncedSaver::new();
        assert_eq!(poll_timeout_ms(&saver, t0), POLL_TICK_MS);

        // Dirty and never saved: a save is due right now.
        saver.mark_dirty();
        assert_eq!(poll_timeout_ms(&saver, t0), 0);

        // Dirty, last save recent: wake at the deadline, capped by the tick.
        saver.last = Some(t0);
        assert_eq!(
            poll_timeout_ms(&saver, t0 + Duration::from_millis(100)),
            POLL_TICK_MS
        );
        assert_eq!(
            poll_timeout_ms(&saver, t0 + Duration::from_millis(900)),
            100
        );

        // Overdue (past the debounce window): immediate wake.
        assert_eq!(poll_timeout_ms(&saver, t0 + Duration::from_millis(1100)), 0);
    }

    #[test]
    fn state_dir_prefers_xdg_and_falls_back_to_home() {
        assert_eq!(
            state_dir(Some(OsStr::new("/home/u")), Some(OsStr::new("/xdg/state"))),
            PathBuf::from("/xdg/state/i3_kb_switch")
        );
        // An empty XDG_STATE_HOME counts as unset (XDG base-dir spec).
        assert_eq!(
            state_dir(Some(OsStr::new("/home/u")), Some(OsStr::new(""))),
            PathBuf::from("/home/u/.local/state/i3_kb_switch")
        );
        assert_eq!(
            state_dir(Some(OsStr::new("/home/u")), None),
            PathBuf::from("/home/u/.local/state/i3_kb_switch")
        );
    }
}
