//! Unix-socket transport for pane focus events.
//!
//! Tiny one-line protocol: the CLI connects to the daemon's socket and
//! writes `pane <id>\n` (id decimal with an optional `%` prefix — tmux's
//! `#{pane_id}` format, e.g. `%5`), then shuts down its write side. The
//! daemon accepts and reads a single line per connection.

use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::time::Duration;

/// One `read` buffer for a pane event line (`pane <id>\n`).
const READ_BUF_SIZE: usize = 256;
/// Read timeout applied before the single read in [`read_pane_event`].
const READ_TIMEOUT: Duration = Duration::from_millis(100);

/// Binds a unix socket listener at `path`, first unlinking any existing
/// file there (stale socket left behind by a crashed daemon).
pub fn bind(path: &Path) -> io::Result<UnixListener> {
    match fs::remove_file(path) {
        Ok(()) => {}
        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
        Err(err) => return Err(err),
    }
    UnixListener::bind(path)
}

/// Parses one pane event line (`pane <id>\n`; trailing newline optional).
/// Returns `None` for anything else: wrong verb, extra tokens, unparsable
/// or overflowing id, empty input.
pub fn parse_pane_msg(buf: &[u8]) -> Option<u32> {
    let line = buf.strip_suffix(b"\n").unwrap_or(buf);
    let rest = line.strip_prefix(b"pane ")?;
    let id = rest.strip_prefix(b"%").unwrap_or(rest);
    if id.is_empty() || !id.iter().all(u8::is_ascii_digit) {
        return None;
    }
    std::str::from_utf8(id).ok()?.parse().ok()
}

/// Parses a pane id from a CLI argument: tmux's `%<n>` form or a bare
/// decimal. Returns `None` for anything else (empty, unparsable,
/// overflowing).
pub fn parse_pane_id(s: &str) -> Option<u32> {
    let id = s.strip_prefix('%').unwrap_or(s);
    if id.is_empty() || !id.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    id.parse().ok()
}

/// Reads a single pane event from an accepted client stream: sets a
/// 100 ms read timeout, performs one `read` into a 256-byte buffer, and
/// delegates to [`parse_pane_msg`]. Returns `None` on timeout, reset,
/// EOF, or any parse failure.
pub fn read_pane_event(stream: &mut UnixStream) -> Option<u32> {
    stream.set_read_timeout(Some(READ_TIMEOUT)).ok()?;
    let mut buf = [0u8; READ_BUF_SIZE];
    match stream.read(&mut buf) {
        Ok(n) => parse_pane_msg(&buf[..n]),
        Err(_) => None,
    }
}

/// Sends one pane event to the daemon listening at `path`: connect,
/// write `pane <id>\n`, shut down the write side, drop the stream.
/// Returns `false` on any error — an absent daemon is routine, not an
/// error.
pub fn send_pane_event(path: &Path, pane: u32) -> bool {
    let Ok(mut stream) = UnixStream::connect(path) else {
        return false;
    };
    let msg = format!("pane {pane}\n");
    if stream.write_all(msg.as_bytes()).is_err() {
        return false;
    }
    let _ = stream.shutdown(std::net::Shutdown::Write);
    true
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::FileTypeExt;
    use std::path::PathBuf;

    use super::{bind, parse_pane_id, parse_pane_msg, read_pane_event, send_pane_event};

    /// Builds a unique temporary socket path under the system temp
    /// directory.
    fn temp_sock_path(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock is past the Unix epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "i3_kb_switch_sock_{tag}_{}_{}.sock",
            std::process::id(),
            nanos,
        ));
        let _ = fs::remove_file(&path); // start from a clean slate
        path
    }

    // ---- parse_pane_msg ------------------------------------------------

    #[test]
    fn parse_accepts_percent_prefixed_id() {
        assert_eq!(parse_pane_msg(b"pane %5\n"), Some(5));
    }

    #[test]
    fn parse_accepts_plain_decimal_without_newline() {
        assert_eq!(parse_pane_msg(b"pane 5"), Some(5));
    }

    #[test]
    fn parse_accepts_zero() {
        assert_eq!(parse_pane_msg(b"pane %0\n"), Some(0));
    }

    #[test]
    fn parse_rejects_wrong_verb() {
        assert_eq!(parse_pane_msg(b"focus 5\n"), None);
    }

    #[test]
    fn parse_rejects_extra_tokens() {
        assert_eq!(parse_pane_msg(b"pane 5 extra\n"), None);
    }

    #[test]
    fn parse_rejects_unparsable_id() {
        assert_eq!(parse_pane_msg(b"pane abc\n"), None);
    }

    #[test]
    fn parse_rejects_missing_id() {
        assert_eq!(parse_pane_msg(b"pane\n"), None);
        assert_eq!(parse_pane_msg(b"pane \n"), None);
        assert_eq!(parse_pane_msg(b"pane %\n"), None);
    }

    #[test]
    fn parse_rejects_empty_buffer() {
        assert_eq!(parse_pane_msg(b""), None);
    }

    /// An oversized line truncated by the 256-byte read buffer must not
    /// parse (the cut id overflows `u32`).
    #[test]
    fn parse_rejects_truncated_oversized_line() {
        let long = format!("pane {}\n", "9".repeat(300));
        assert_eq!(parse_pane_msg(long.as_bytes()), None);
    }

    // ---- parse_pane_id -------------------------------------------------

    #[test]
    fn pane_id_accepts_percent_prefix() {
        assert_eq!(parse_pane_id("%5"), Some(5));
    }

    #[test]
    fn pane_id_accepts_plain_decimal() {
        assert_eq!(parse_pane_id("5"), Some(5));
    }

    #[test]
    fn pane_id_accepts_zero() {
        assert_eq!(parse_pane_id("%0"), Some(0));
    }

    #[test]
    fn pane_id_rejects_empty() {
        assert_eq!(parse_pane_id(""), None);
        assert_eq!(parse_pane_id("%"), None);
    }

    #[test]
    fn pane_id_rejects_non_numeric() {
        assert_eq!(parse_pane_id("x"), None);
    }

    #[test]
    fn pane_id_rejects_negative() {
        assert_eq!(parse_pane_id("-1"), None);
    }

    // ---- bind ----------------------------------------------------------

    /// A stale file at the socket path (left by a crashed daemon) must be
    /// unlinked first, and the path must be a socket after binding.
    #[test]
    fn bind_unlinks_stale_file_and_binds() {
        let path = temp_sock_path("stale");

        fs::write(&path, b"stale socket from a crash").expect("dummy file");
        let listener = bind(&path).expect("bind must succeed over stale file");

        let meta = fs::metadata(&path).expect("socket path after bind");
        assert!(meta.file_type().is_socket());

        drop(listener);
        let _ = fs::remove_file(&path);
    }

    // ---- read_pane_event + send_pane_event (in-process roundtrip) ------

    /// Full roundtrip without threads: the client finishes writing (and
    /// drops its stream) before the server side accepts, so the single
    /// buffered read still sees the whole line.
    #[test]
    fn roundtrip_delivers_pane_id() {
        let path = temp_sock_path("roundtrip");

        let listener = bind(&path).expect("bind");
        assert!(send_pane_event(&path, 42));

        let (mut stream, _) = listener.accept().expect("accept");
        assert_eq!(read_pane_event(&mut stream), Some(42));

        drop(stream);
        drop(listener);
        let _ = fs::remove_file(&path);
    }

    /// Daemon absent is routine: sending to a non-existent socket path
    /// must return `false`, not panic.
    #[test]
    fn send_returns_false_when_daemon_absent() {
        let path = temp_sock_path("absent");
        assert!(!send_pane_event(&path, 1));
    }
}
