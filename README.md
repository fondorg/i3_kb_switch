# i3wm Keyboard Switcher

A small daemon for [i3wm](https://i3wm.org) on X11 that remembers the
keyboard layout **per window**. When you switch focus between windows, the
daemon automatically applies the layout each window was last using — the
classic "per-window layout memory" that i3 itself does not provide.

Single static binary, no configuration file. Written in Rust (edition 2024)
using `x11rb` (XKB extension) and a hand-rolled i3 IPC client — the only
dependencies are `x11rb`, `serde`/`serde_json`, `signal-hook` and `libc`.

## How it works

- Connects to i3 over its IPC socket (path discovered from the
  `I3_SOCKET_PATH` root window property, falling back to
  `i3 --get-socketpath` and then the `I3SOCK` environment variable) and
  subscribes to window focus/close events.
- Reads the active layout through the XKB extension (`_XKB_RULES_NAMES`
  root property → layout names; `XkbGetState` → locked group; `XkbLatchLockState`
  → lock a group). The Alt+Shift toggle (or any other group switch) is part
  of the same X server state the daemon reads and writes.
- **Save-at-switch:** when focus leaves a window, the layout currently
  observed on X is recorded for that window.
- **Apply-on-focus:** when a window gains focus, its stored layout is
  applied (falling back to `us` for never-seen windows).
- **Rules watch:** the daemon subscribes to root window property changes for
  `_XKB_RULES_NAMES`. If something re-runs `setxkbmap` (`i3-msg reload`,
  `i3-msg restart`, manual run) the daemon re-resolves and re-applies the
  focused window's layout **by name**, so reordered layout lists cannot
  flip the active group semantics. Note the semantics: it re-applies the
  focused window's **stored** layout — a manual toggle made after the last
  focus event is intentionally not preserved by this path.
- **Reconnect:** if the i3 connection breaks (WM restart), the daemon
  retries with exponential backoff, re-reads the tree and keeps the stored
  layouts (X window ids survive an i3 restart).
- **Persistence:** the layout map is written (debounced, at most once per
  second, plus a flush on SIGTERM/SIGINT) to
  `~/.local/state/i3_kb_switch/state.json`.
- **Single instance:** guarded by an flock on
  `~/.local/state/i3_kb_switch/lock`; a second instance prints
  `i3_kb_switch: already running` and exits 0.

### Ignored windows

Focus events for windows of class `Rofi` / `rofi` are ignored entirely:
nothing is applied, nothing is saved, `last_window` is untouched. In
practice rofi runs as an override-redirect window and never appears in the
i3 tree at all, so the daemon usually sees no events from it either way.

## Build

Any recent Rust toolchain (edition 2024, e.g. 1.85+) works.

```sh
# glibc build
cargo build --release          # binary: target/release/i3_kb_switch

# static musl build
rustup target add x86_64-unknown-linux-musl
cargo build --release --target x86_64-unknown-linux-musl
ldd target/x86_64-unknown-linux-musl/release/i3_kb_switch   # "statically linked"
```

Note (this machine): `/usr/bin/{rustc,cargo}` here are the *system*
Gentoo Rust (1.97), while `rustup`'s stable toolchain (1.99) holds the
musl std. Cargo resolves `rustc` via `PATH`, so the musl build must run
with the rustup toolchain first on PATH:

```sh
PATH="$HOME/.cargo/bin:$PATH" cargo build --release --target x86_64-unknown-linux-musl
```

No linking fallback was needed: the musl build links statically out of the
box (all dependencies are pure Rust).

## Install

Quick way — this builds the static binary, replaces `~/.local/bin/i3_kb_switch`
(stops the running daemon gracefully first) and restarts it:

```sh
make install
```

`make help` lists all targets (`static`, `test`, `lint`, `install`,
`tmux-setup`, `restart`, `stop`, `status`, `clean`).

Manual equivalent:

```sh
make static
pkill -x i3_kb_switch   # graceful: flushes state before exit
cp target/x86_64-unknown-linux-musl/release/i3_kb_switch ~/.local/bin/
~/.local/bin/i3_kb_switch &
```

## i3 integration

One line in `~/.config/i3/config`:

```
exec_always --no-startup-id ~/.local/bin/i3_kb_switch
```

`exec_always` (not `exec`) matters: the daemon must also start again after
`i3-msg reload`/`restart`. If it is already running the new instance exits
immediately thanks to the flock guard, so the line is idempotent.

Run it by hand (without re-loading i3) with:

```sh
~/.local/bin/i3_kb_switch &
```

Run `~/.local/bin/i3_kb_switch -v` to see every apply/save/reconnect on stderr.

## tmux pane layouts

Inside tmux, keyboard layout memory also works **per pane**: when focus
moves between panes (including via vim-tmux-navigator, which switches panes
with `select-pane`), the daemon saves the outgoing pane's layout and applies
the one remembered for the incoming pane, keyed by the stable tmux
`#{pane_id}`. Refocusing the terminal window applies the active pane's
remembered layout. The window-level behavior is unchanged; the two layers
are independent.

### Activation

- `make install` wires this automatically: after installing and restarting
  the daemon it runs `make tmux-setup`, which looks for an existing tmux
  config (`~/.tmux.conf` first, then `~/.config/tmux/tmux.conf`) and
  appends the marker block below. **No tmux config means nothing happens** —
  the script never creates one. With a running tmux server the config is
  re-sourced immediately; otherwise the hooks apply at the next tmux start.
- `make tmux-setup` is standalone and idempotent (rerunning replaces the
  marker block in place, e.g. after moving the binary to a new prefix):
  `make tmux-setup BIN=~/.local/bin/i3_kb_switch`.
- The block turns on `focus-events` — unless the config already sets it
  (any value; commented-out lines do not count). An existing
  `set ... focus-events` line is left untouched and honored.
- The hook calls the daemon CLI with the **absolute** binary path (the
  tmux server's inherited `PATH` is not trusted). The CLI
  (`i3_kb_switch pane <pane_id>`) exits 0 silently when the daemon is not
  running, so the hook never spams errors.

The marker block as written by `make tmux-setup` (the hook embeds the
absolute binary path; shown here abbreviated with `~`):

```tmux
# >>> i3_kb_switch (pane layout memory) >>>
set -g focus-events on
set-hook -g pane-focus-in 'run-shell -b "~/.local/bin/i3_kb_switch pane #{pane_id}"'
# <<< i3_kb_switch <<<
```

### Manual enable / disable

To disable, delete the block between the two `i3_kb_switch` marker comment
lines from your tmux config and reload (`tmux source-file ~/.tmux.conf`).
To re-enable later, run `make tmux-setup` again. Pane state lives in
`~/.local/state/i3_kb_switch/panes.json` and can be deleted anytime.

### Limitations

- A `select-pane` in an **unfocused background terminal** (e.g. triggered
  by a script or another client) changes the global layout; rare, and the
  next focus switch self-corrects it.
- The pane map grows **unboundedly** — panes die unobserved and their
  entries are never pruned. Bytes-scale; accepted.
- If your config sets `focus-events` **off**, `pane-focus-in` does not fire
  on terminal-window refocus (the install deliberately does not touch your
  line). Refocus then still applies the window layout, which equals the
  active pane's layout as of the last save.
- No layouts inside vim splits — that is vim's own concern.

## Behavior summary

| Aspect | Value |
| --- | --- |
| Default layout | `us` (used for unknown windows and as fallback) |
| Persistence | `~/.local/state/i3_kb_switch/state.json` (debounced ≤1/s + flush on SIGTERM/SIGINT) |
| Lock file | `~/.local/state/i3_kb_switch/lock` (single instance) |
| Ignored classes | `Rofi`, `rofi` |
| i3 reconnect | exponential backoff (500 ms doubling, capped), layouts kept |
| X loss (X server death) | fatal — the daemon exits |

## Troubleshooting / observed behavior

Observations from live verification on this machine (Gentoo, X11,
i3 4.25.1, layouts `us,ru`, toggle `grp:alt_shift_toggle`):

- **`i3-msg reload` did not reset the keyboard group** in testing: the
  active layout survived every reload (sampled at ~20 ms resolution), and
  the daemon logged no rules change. `exec_always setxkbmap` did not
  observably touch XKB state on reload.
- **`i3-msg restart` re-runs `exec_always setxkbmap`** (the keymap is
  rebuilt) — the rules property changes, the daemon logs
  `rules property changed: re-applied <layout> to window <id>` and the
  focused window keeps its layout. The daemon reconnects to the fresh i3
  socket automatically (`truncated frame; reconnecting` → backoff →
  `i3 IPC established`); stored layouts survive the restart because X
  window ids are stable.
- **Manual `setxkbmap` runs** (identical or changed options) do **not**
  reset the locked group on this X server, but they always produce a
  `_XKB_RULES_NAMES` property change; the daemon re-applies the saved
  layout immediately in every case.
- **Rofi** (override-redirect) never appears in `i3-msg -t get_tree`, so
  it cannot steal the daemon's layout tracking; the layout in force when
  rofi opened stays active while it is open.
- **Known issue on this machine (unrelated to this daemon):**
  `~/bin/xkb-switch` is broken — it fails with
  `libxkbswitch.so.2: cannot open shared object file` (the library does
  not exist anywhere on the system). This silently disables the
  layout-save/restore prefix of the owner's `$mod+d` / `$mod+r` rofi
  bindings (`layout=$(xkb-switch -p); xkb-switch -s us; ...; xkb-switch -s
  ${layout}`); rofi itself still opens fine.
- If layouts seem stuck, check for a stale instance:
  `pgrep -a i3_kb_switch`, then `pkill i3_kb_switch` (state is flushed on
  SIGTERM) and start it again — on startup it applies the stored layout
  of the currently focused window.

## Roadmap

- **Phase 2 (implemented):** per-pane layout memory inside tmux — see
  "tmux pane layouts" above.
- Explicitly out of scope for v1: per-application-class layout defaults,
  polybar layout indicator, sway/Wayland support.
