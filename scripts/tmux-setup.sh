#!/usr/bin/env bash
# Wire i3_kb_switch pane-layout hooks into an existing tmux config.
#
# Spec: docs/superpowers/specs/2026-11-07-tmux-pane-layout-design.md,
# section "Install wiring".
#
# Config resolution:
#   1. $HOME/.tmux.conf exists            -> wire there
#   2. $HOME/.config/tmux/tmux.conf       -> wire there
#   3. neither exists                     -> skip silently, never create one
#
# Idempotent: the marker block is replaced if present, appended if absent.
# If the user's config already sets focus-events (any value; commented-out
# lines do not count) the block omits `set -g focus-events on` and the
# user's line is left untouched. The hook uses the absolute binary path so
# the tmux server's inherited PATH is irrelevant.
#
# Usage: tmux-setup.sh [BIN]
#   BIN — absolute path to the installed binary (first arg, else $BIN, else
#   $HOME/.local/bin/i3_kb_switch). `make install` passes it explicitly.

set -eu

BIN="${1:-${BIN:-$HOME/.local/bin/i3_kb_switch}}"

MARK_OPEN='# >>> i3_kb_switch (pane layout memory) >>>'
MARK_CLOSE='# <<< i3_kb_switch <<<'

conf=""
if [ -f "$HOME/.tmux.conf" ]; then
    conf="$HOME/.tmux.conf"
elif [ -f "$HOME/.config/tmux/tmux.conf" ]; then
    conf="$HOME/.config/tmux/tmux.conf"
else
    echo "tmux-setup: no tmux config (~/.tmux.conf or ~/.config/tmux/tmux.conf); skipping" >&2
    exit 0
fi

# Build the block. set -g focus-events on is omitted when the user already
# sets focus-events themselves (any value; comments do not count). Detection
# ignores our own previous block, so reruns stay byte-identical.
if ! I3_OPEN="$MARK_OPEN" I3_CLOSE="$MARK_CLOSE" awk '
    $0 == ENVIRON["I3_OPEN"]  { skip = 1; next }
    $0 == ENVIRON["I3_CLOSE"] { skip = 0; next }
    !skip { print }
' "$conf" | grep -Eq '^[[:space:]]*set[a-z-]*([[:space:]]+-[A-Za-z]+)*[[:space:]]+focus-events([[:space:]]|$)'; then
    block="$MARK_OPEN
set -g focus-events on"
else
    block="$MARK_OPEN"
fi
block="$block
set-hook -g pane-focus-in 'run-shell -b \"$BIN pane #{pane_id}\"'
$MARK_CLOSE"

tmp="$(mktemp "${conf}.XXXXXX")"
if I3_BLOCK="$block" I3_OPEN="$MARK_OPEN" I3_CLOSE="$MARK_CLOSE" awk '
    $0 == ENVIRON["I3_OPEN"]  { print ENVIRON["I3_BLOCK"]; skip = 1; seen = 1; next }
    $0 == ENVIRON["I3_CLOSE"] { skip = 0; next }
    !skip { print }
    END { exit seen ? 0 : 42 }
' "$conf" > "$tmp"; then
    # Marker block present -> replace it in place (keeps everything around it).
    mv "$tmp" "$conf"
else
    rc=$?
    rm -f "$tmp"
    if [ "$rc" -eq 42 ]; then
        # Absent -> append (blank line first for readability).
        printf '\n%s\n' "$block" >> "$conf"
    else
        echo "tmux-setup: failed to rewrite $conf" >&2
        exit "$rc"
    fi
fi

# Apply immediately only when a tmux server is actually running; otherwise
# it applies on the next tmux start.
if [ -e "/tmp/tmux-$(id -u)/default" ] && command -v tmux >/dev/null 2>&1; then
    if ! tmux source-file "$conf"; then
        echo "tmux-setup: warning: tmux source-file $conf failed" >&2
    fi
fi
