#!/usr/bin/env bash
# Fixture checks for scripts/tmux-setup.sh.
#
# Spec: docs/superpowers/specs/2026-11-07-tmux-pane-layout-design.md,
# section "Install wiring".
#
# Every run uses a tempdir HOME — the real ~/.tmux.conf is never touched.
# `tmux` and `id` are stubbed (log-only tmux; a fake-uid id mode), so the
# runs can never source a config into a live tmux server either.
#
# Coverage:
#   (a) no tmux config anywhere  -> nothing created, exit 0
#   (b) plain ~/.tmux.conf       -> marker block appended (hook + focus-events)
#  (b2) tmux source-file guard   -> attempted only when the server socket exists
#   (c) second run, same BIN     -> file unchanged (idempotent)
#  (c2) rerun with different BIN -> block replaced in place, single marker pair
#   (d) user sets focus-events   -> block without the set line, user line intact
# (d2) only a commented-out line -> block DOES include set -g focus-events on
#   (e) hook line                -> absolute BIN path, nothing left unexpanded
#   (f) XDG config resolution + ~/.tmux.conf precedence
set -eu

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SCRIPT="$ROOT/scripts/tmux-setup.sh"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

BIN1="$TMP/bin/i3_kb_switch"
BIN2="$TMP/bin2/i3_kb_switch"

fail=0
pass()   { echo "PASS: $*"; }
failf()  { echo "FAIL: $*"; fail=1; }

# --- stubs: tmux logs its args and exits 0; id has real/fake-uid variants ---
for mode in real fake; do
    mkdir -p "$TMP/stub-$mode/bin"
    printf '#!/usr/bin/env bash\nprintf "%%s\\n" "$*" >> "${TMUX_LOG:?}"\nexit 0\n' \
        > "$TMP/stub-$mode/bin/tmux"
    if [ "$mode" = fake ]; then
        printf '#!/usr/bin/env bash\ncase "${1:-}" in -u) echo 999999 ;; *) exec /usr/bin/id "$@" ;; esac\n' \
            > "$TMP/stub-$mode/bin/id"
    else
        printf '#!/usr/bin/env bash\nexec /usr/bin/id "$@"\n' \
            > "$TMP/stub-$mode/bin/id"
    fi
    chmod +x "$TMP/stub-$mode/bin/tmux" "$TMP/stub-$mode/bin/id"
done

newhome() {
    rm -rf "$TMP/home"
    mkdir -p "$TMP/home"
}

run() { # run [BIN] [real|fake] — tempdir HOME, stubbed tmux/id on PATH
    bin="${1:-$BIN1}"
    mode="${2:-real}"
    : > "$TMP/tmux-$mode.log"
    HOME="$TMP/home" PATH="$TMP/stub-$mode/bin:$PATH" TMUX_LOG="$TMP/tmux-$mode.log" \
        bash "$SCRIPT" "$bin"
}

require_file() { # file desc — guard the assert helpers against missing files
    if [ ! -f "$1" ]; then failf "$2 (file missing: $1)"; return 1; fi
}
assert_contains() { # file string desc
    require_file "$1" "$3" || return 0
    if grep -qF -- "$2" "$1"; then pass "$3"; else failf "$3 (missing: $2)"; fi
}
assert_not_contains() { # file string desc
    require_file "$1" "$3" || return 0
    if grep -qF -- "$2" "$1"; then failf "$3 (unwanted: $2)"; else pass "$3"; fi
}
assert_line() { # file regex desc
    require_file "$1" "$3" || return 0
    if grep -qE -- "$2" "$1"; then pass "$3"; else failf "$3 (no line matching: $2)"; fi
}
assert_no_line() { # file regex desc
    require_file "$1" "$3" || return 0
    if grep -qE -- "$2" "$1"; then failf "$3 (found unwanted line: $2)"; else pass "$3"; fi
}
assert_count() { # file regex want desc
    require_file "$1" "$4" || return 0
    n="$(grep -cE -- "$2" "$1" || true)"
    if [ "$n" -eq "$3" ]; then pass "$4"; else failf "$4 (count $n, want $3)"; fi
}

# ---------- (a) no tmux conf -> nothing created ----------
newhome
rc=0; run >/dev/null 2>&1 || rc=$?
[ "$rc" -eq 0 ] && pass "(a) no conf: exit 0" || failf "(a) no conf: exit $rc, want 0"
if [ -e "$TMP/home/.tmux.conf" ] || [ -e "$TMP/home/.config/tmux/tmux.conf" ]; then
    failf "(a) a tmux config was created"
else
    pass "(a) no conf: no file created"
fi

# ---------- (b) plain ~/.tmux.conf -> block appended ----------
newhome
conf="$TMP/home/.tmux.conf"
printf '# my tmux conf\nset -g base-index 0\n' > "$conf"
run >/dev/null
assert_line "$conf" '^# >>> i3_kb_switch \(pane layout memory\) >>>$' "(b) open marker appended"
assert_line "$conf" '^# <<< i3_kb_switch <<<$'                       "(b) close marker appended"
assert_line "$conf" '^set -g focus-events on$'                       "(b) focus-events line included"
assert_contains "$conf" "run-shell -b \"$BIN1 pane #{pane_id}\""     "(b) hook calls BIN with pane id"
assert_line "$conf" '^set-hook -g pane-focus-in '                    "(b) hook is pane-focus-in set-hook"
assert_line "$conf" '^# my tmux conf$'                               "(b) user content preserved"
assert_line "$conf" '^set -g base-index 0$'                          "(b) user option preserved"
assert_count  "$conf" '# >>> i3_kb_switch \(pane layout memory\) >>>' 1 "(b) exactly one open marker"

# ---------- (b2) tmux source-file only when the server socket exists ----------
run "$BIN1" real >/dev/null
run "$BIN1" fake >/dev/null
if [ -e "/tmp/tmux-$(id -u)/default" ]; then
    assert_contains "$TMP/tmux-real.log" "source-file $conf" "(b2) socket present: tmux source-file attempted"
else
    assert_not_contains "$TMP/tmux-real.log" "source-file" "(b2) no socket: no source attempt"
fi
assert_not_contains "$TMP/tmux-fake.log" "source-file" "(b2) absent socket: no source attempt"

# ---------- (c) second run -> unchanged (idempotent) ----------
before="$(cat "$conf")"
run >/dev/null
after="$(cat "$conf")"
[ "$before" = "$after" ] && pass "(c) second run: file unchanged" || failf "(c) second run: file changed"
assert_count "$conf" '# >>> i3_kb_switch \(pane layout memory\) >>>' 1 "(c) still exactly one block"

# ---------- (c2) rerun with different BIN -> block replaced in place ----------
run "$BIN2" >/dev/null
assert_contains     "$conf" "run-shell -b \"$BIN2 pane #{pane_id}\"" "(c2) hook updated to new BIN"
assert_not_contains "$conf" "$BIN1"                                  "(c2) old BIN path gone"
assert_count "$conf" '# >>> i3_kb_switch \(pane layout memory\) >>>' 1 "(c2) still exactly one block"
assert_line "$conf" '^# my tmux conf$'                               "(c2) user content still intact"

# ---------- (d) user sets focus-events -> block omits it ----------
newhome
conf="$TMP/home/.tmux.conf"
printf 'set -g focus-events off\n' > "$conf"
run >/dev/null
assert_line    "$conf" '^# >>> i3_kb_switch \(pane layout memory\) >>>$' "(d) block appended"
assert_line    "$conf" '^set-hook -g pane-focus-in '                     "(d) hook appended"
assert_no_line "$conf" '^set -g focus-events on$'                        "(d) no own focus-events line"
assert_count   "$conf" '^set -g focus-events off$' 1                     "(d) user line untouched, once"

# ---------- (d2) commented-out focus-events does NOT count ----------
newhome
conf="$TMP/home/.tmux.conf"
printf '# set -g focus-events on\n' > "$conf"
run >/dev/null
assert_line "$conf" '^set -g focus-events on$'                    "(d2) commented line ignored: set line included"
assert_line "$conf" '^# set -g focus-events on$'                  "(d2) user comment untouched"
assert_count "$conf" '^set -g focus-events on$' 1                 "(d2) exactly one active set line"

# ---------- (e) hook uses the absolute BIN path, nothing unexpanded ----------
assert_not_contains "$conf" '$BIN'   "(e) no unexpanded \$BIN in config"
assert_not_contains "$conf" 'HOME/.local/bin' "(e) no hardcoded \$HOME path in config"

# ---------- (f) XDG resolution + ~/.tmux.conf precedence ----------
newhome
mkdir -p "$TMP/home/.config/tmux"
xdg="$TMP/home/.config/tmux/tmux.conf"
printf 'set -g base-index 1\n' > "$xdg"
run >/dev/null
assert_line "$xdg" '^# >>> i3_kb_switch \(pane layout memory\) >>>$' "(f) XDG config wired when it is the only one"
[ -e "$TMP/home/.tmux.conf" ] && failf "(f) ~/.tmux.conf was created" || pass "(f) no ~/.tmux.conf created"

newhome
mkdir -p "$TMP/home/.config/tmux"
printf 'set -g base-index 1\n' > "$xdg"
printf 'set -g base-index 9\n' > "$TMP/home/.tmux.conf"
before="$(cat "$xdg")"
run >/dev/null
assert_line "$TMP/home/.tmux.conf" '^# >>> i3_kb_switch \(pane layout memory\) >>>$' "(f) ~/.tmux.conf wins when both exist"
[ "$(cat "$xdg")" = "$before" ] && pass "(f) XDG config untouched on precedence" || failf "(f) XDG config was modified"

# ---------- summary ----------
echo "---"
if [ "$fail" -eq 0 ]; then
    echo "ALL FIXTURES PASSED"
else
    echo "FIXTURES FAILED ($fail)"
    exit 1
fi
