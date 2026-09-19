#!/bin/bash
# Runtime smoke test for tmux-ssh-agent on macOS.
#
# macOS support is verified to compile by cross-checking from Linux, but the
# runtime behaviour needs a Mac. This script exercises it and prints a pass or
# fail line per check. Run it from the repo root:
#
#   ./contrib/macos-smoke-test.sh
#
# Requirements: tmux (brew install tmux), ssh-agent and ssh-keygen (system),
# and a Rust toolchain. Nothing is installed and nothing outside a scratch
# directory under $TMPDIR is touched.

set -u

PASS=0
FAIL=0

ok()   { printf '  \033[32mPASS\033[0m  %s\n' "$1"; PASS=$((PASS + 1)); }
bad()  { printf '  \033[31mFAIL\033[0m  %s\n' "$1"; FAIL=$((FAIL + 1)); }
note() { printf '        %s\n' "$1"; }
head_() { printf '\n== %s ==\n' "$1"; }

if [ "$(uname -s)" != "Darwin" ]; then
  echo "This script is for macOS; uname says $(uname -s)." >&2
  exit 2
fi

for tool in tmux ssh-agent ssh-keygen cargo; do
  command -v "$tool" >/dev/null 2>&1 || { echo "missing required tool: $tool" >&2; exit 2; }
done

T=$(mktemp -d "${TMPDIR:-/tmp}/tsa-smoke.XXXXXX")
BIN="$PWD/target/debug/tmux-ssh-agent"
SOCKET_DIR="$T/rt"
export TMUX_TMPDIR="$T/tmux"
mkdir -p "$SOCKET_DIR" "$TMUX_TMPDIR"

# Pids of daemons using OUR scratch runtime dir. `ps -E` prints a process's
# environment on macOS.
#
# Never `pkill -x tmux-ssh-agent`: the machine running this very likely has the
# user's own proxy running against their real runtime directory, and killing it
# would drop their agent out from under them.
our_daemons() {
  for p in $(pgrep -x tmux-ssh-agent 2>/dev/null); do
    if ps -Eww -p "$p" 2>/dev/null | grep -qF "TMPDIR=$SOCKET_DIR"; then
      echo "$p"
    fi
  done
}
our_daemon_count() { our_daemons | grep -c . ; }

cleanup() {
  for p in $(our_daemons); do kill "$p" 2>/dev/null; done
  for n in 1 2; do
    [ -S "$T/ag$n.sock" ] && SSH_AUTH_SOCK="$T/ag$n.sock" ssh-agent -k >/dev/null 2>&1
  done
  tmux -L smoke kill-server 2>/dev/null
  rm -rf "$T"
}
trap cleanup EXIT

# Run tmux-ssh-agent with the scratch runtime dir, and without the Linux-only
# variables in case they are set.
run() { env -u XDG_RUNTIME_DIR -u RUNTIME_DIRECTORY TMPDIR="$SOCKET_DIR" "$BIN" "$@"; }

head_ "Environment"
note "TMPDIR        = ${TMPDIR:-<unset>} (${#TMPDIR} chars)"
note "uid           = $(id -u)"
note "tmux          = $(command -v tmux)"

head_ "1. Build and unit tests"
if cargo build >/dev/null 2>&1; then ok "cargo build"; else bad "cargo build"; exit 1; fi
if cargo test >/dev/null 2>&1; then ok "cargo test"; else bad "cargo test (run 'cargo test' to see why)"; fi

head_ "2. Daemon starts"
tmux -L smoke kill-server 2>/dev/null; sleep 0.3
tmux -L smoke -f /dev/null new-session -d -s s1 bash
# BSD script takes the command as trailing arguments, unlike Linux's -c form.
script -q /dev/null tmux -L smoke attach -t s1 >/dev/null 2>&1 &
sleep 2

TTY1=$(tmux -L smoke list-clients -F '#{client_tty}' | head -1)
[ -n "$TTY1" ] && ok "a client is attached ($TTY1)" || bad "no client attached"

ssh-agent -s -a "$T/ag1.sock" >/dev/null
ssh-keygen -q -t ed25519 -N '' -f "$T/k1" -C KEY1
SSH_AUTH_SOCK="$T/ag1.sock" ssh-add "$T/k1" >/dev/null 2>&1
run register-agent "$T/ag1.sock" >/dev/null 2>&1 && ok "register-agent" || bad "register-agent"

TMUXVAR=$(tmux -L smoke display-message -p '#{socket_path},#{pid},0')
if env -u XDG_RUNTIME_DIR -u RUNTIME_DIRECTORY TMPDIR="$SOCKET_DIR" TMUX="$TMUXVAR" "$BIN" proxy; then
  ok "proxy exits 0 (this is what fails on an unpatched build)"
else
  bad "proxy exited non-zero; run it with --no-daemon to see the error"
fi
sleep 1

RT="$SOCKET_DIR/tmux-ssh-agent-proxy-$(id -u)"
SOCK=$(tmux -L smoke show-environment -g SSH_AUTH_SOCK 2>/dev/null | cut -d= -f2)

head_ "3. Socket path length"
if [ -n "$SOCK" ]; then
  note "socket = $SOCK"
  note "length = ${#SOCK} bytes, Darwin allows 103"
  [ "${#SOCK}" -le 103 ] && ok "socket path within sun_path" || bad "socket path too long"
  [ -S "$SOCK" ] && ok "socket exists" || bad "socket missing"
else
  bad "SSH_AUTH_SOCK was never set in the tmux environment"
fi

head_ "4. Permissions"
check_mode() {
  actual=$(stat -f '%Lp' "$1" 2>/dev/null)
  [ "$actual" = "$2" ] && ok "$3 is $2" || bad "$3 is ${actual:-missing}, expected $2"
}
check_mode "$RT" 700 "runtime dir"
[ -n "$SOCK" ] && check_mode "$SOCK" 600 "socket"
check_mode "$RT/state" 600 "state file"

head_ "5. Agent forwarding"
if [ -n "$SOCK" ] && SSH_AUTH_SOCK="$SOCK" ssh-add -l 2>/dev/null | grep -q KEY1; then
  ok "ssh-add -l through the proxy lists the registered key"
else
  bad "ssh-add -l through the proxy did not list the key"
fi

head_ "6. Per-pane routing (the parity test)"
tmux -L smoke -f /dev/null new-session -d -s s2 bash
script -q /dev/null tmux -L smoke attach -t s2 >/dev/null 2>&1 &
sleep 2
TTY2=$(tmux -L smoke list-clients -F '#{client_tty} #{session_name}' | awk '$2=="s2"{print $1}')
ssh-agent -s -a "$T/ag2.sock" >/dev/null
ssh-keygen -q -t ed25519 -N '' -f "$T/k2" -C KEY2
SSH_AUTH_SOCK="$T/ag2.sock" ssh-add "$T/k2" >/dev/null 2>&1
env -u XDG_RUNTIME_DIR -u RUNTIME_DIRECTORY TMPDIR="$SOCKET_DIR" SSH_TTY="$TTY2" \
    "$BIN" register-agent "$T/ag2.sock" >/dev/null 2>&1

# Make s2's client the most recently active, so a server-wide heuristic would
# answer "s2" for both panes.
tmux -L smoke send-keys -t s2 "" Enter; sleep 1
for s in s1 s2; do
  tmux -L smoke send-keys -t "$s" "SSH_AUTH_SOCK=$SOCK ssh-add -l > $T/out.$s 2>&1" Enter
done
sleep 4
SAW1=$(grep -o 'KEY[0-9]' "$T/out.s1" 2>/dev/null | head -1)
SAW2=$(grep -o 'KEY[0-9]' "$T/out.s2" 2>/dev/null | head -1)
note "pane in s1 sees ${SAW1:-nothing}, pane in s2 sees ${SAW2:-nothing}"
if [ "$SAW1" = "KEY1" ] && [ "$SAW2" = "KEY2" ]; then
  ok "each pane routed to its own agent"
elif [ "$SAW1" = "$SAW2" ] && [ -n "$SAW1" ]; then
  bad "both panes got $SAW1: per-pane routing is not working, fell back to the server-wide heuristic"
else
  bad "unexpected result; see $T/out.s1 and $T/out.s2"
fi

head_ "7. Fallback from outside any pane"
if [ -n "$SOCK" ] && SSH_AUTH_SOCK="$SOCK" ssh-add -l 2>/dev/null | grep -q 'KEY[0-9]'; then
  ok "a request from outside tmux still resolves an agent"
else
  bad "fallback path returned no agent"
fi

head_ "8. Shutdown is clean"
for p in $(our_daemons); do kill -TERM "$p" 2>/dev/null; done
sleep 5
[ "$(our_daemon_count)" = "0" ] && ok "daemon exited" || bad "daemon still running"
LEFT=$(ls -1 "$RT" 2>/dev/null | grep -v -e '^state$' -e '^proxy.lock$' | wc -l | tr -d ' ')
[ "$LEFT" = "0" ] && ok "socket removed on SIGTERM" || bad "$LEFT socket(s) left behind"

printf '\n== Summary ==\n  %d passed, %d failed\n' "$PASS" "$FAIL"
[ "$FAIL" -eq 0 ] || exit 1
