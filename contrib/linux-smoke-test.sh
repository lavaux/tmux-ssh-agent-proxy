#!/bin/bash
# Runtime smoke test for tmux-ssh-agent on Linux.
#
# The unit suite covers the pure logic; this covers the parts that only show up
# against a live tmux server and a real ssh-agent. Run it from the repo root:
#
#   ./contrib/linux-smoke-test.sh
#
# Requirements: tmux, ssh-agent, ssh-keygen, util-linux script(1), and a Rust
# toolchain. Nothing is installed and nothing outside a scratch directory is
# touched. The macOS equivalent is macos-smoke-test.sh.

set -u

PASS=0
FAIL=0

ok()   { printf '  \033[32mPASS\033[0m  %s\n' "$1"; PASS=$((PASS + 1)); }
bad()  { printf '  \033[31mFAIL\033[0m  %s\n' "$1"; FAIL=$((FAIL + 1)); }
note() { printf '        %s\n' "$1"; }
head_() { printf '\n== %s ==\n' "$1"; }

if [ "$(uname -s)" != "Linux" ]; then
  echo "This script is for Linux; uname says $(uname -s). See macos-smoke-test.sh." >&2
  exit 2
fi

for tool in tmux ssh-agent ssh-keygen script cargo; do
  command -v "$tool" >/dev/null 2>&1 || { echo "missing required tool: $tool" >&2; exit 2; }
done

# Short base path on purpose: these directories hold bound Unix sockets, and
# sun_path allows 108 bytes on Linux.
T=$(mktemp -d /tmp/tsa-smoke.XXXXXX)
BIN="$PWD/target/debug/tmux-ssh-agent"
SOCKET_DIR="$T/rt"
export TMUX_TMPDIR="$T/tmux"
mkdir -p "$SOCKET_DIR" "$TMUX_TMPDIR"

# Pids of daemons using OUR scratch runtime dir.
#
# Never `pkill -x tmux-ssh-agent`: the machine running this very likely has the
# user's own proxy running against their real runtime directory, and killing it
# would drop their agent out from under them.
our_daemons() {
  for p in $(pgrep -x tmux-ssh-agent 2>/dev/null); do
    if tr '\0' '\n' < "/proc/$p/environ" 2>/dev/null | grep -qxF "TMPDIR=$SOCKET_DIR"; then
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

# Run with the scratch runtime dir, ignoring any ambient XDG/systemd variables.
run() { env -u XDG_RUNTIME_DIR -u RUNTIME_DIRECTORY TMPDIR="$SOCKET_DIR" "$BIN" "$@"; }
run_tmux() {
  env -u XDG_RUNTIME_DIR -u RUNTIME_DIRECTORY TMPDIR="$SOCKET_DIR" \
      TMUX="$(tmux -L smoke display-message -p '#{socket_path},#{pid},0')" "$BIN" "$@"
}

head_ "Environment"
note "uid  = $(id -u)"
note "tmux = $(command -v tmux)"

head_ "1. Build and unit tests"
if cargo build >/dev/null 2>&1; then ok "cargo build"; else bad "cargo build"; exit 1; fi
if cargo test >/dev/null 2>&1; then ok "cargo test"; else bad "cargo test (run 'cargo test' to see why)"; fi

head_ "2. Daemon starts"
tmux -L smoke kill-server 2>/dev/null; sleep 0.3
tmux -L smoke -f /dev/null new-session -d -s s1 bash
setsid script -qfc "tmux -L smoke attach -t s1" /dev/null >/dev/null 2>&1 &
sleep 2

TTY1=$(tmux -L smoke list-clients -F '#{client_tty}' | head -1)
if [ -n "$TTY1" ]; then ok "a client is attached ($TTY1)"; else bad "no client attached"; fi

ssh-agent -s -a "$T/ag1.sock" >/dev/null
ssh-keygen -q -t ed25519 -N '' -f "$T/k1" -C KEY1
SSH_AUTH_SOCK="$T/ag1.sock" ssh-add "$T/k1" >/dev/null 2>&1
if env -u XDG_RUNTIME_DIR -u RUNTIME_DIRECTORY TMPDIR="$SOCKET_DIR" SSH_TTY="$TTY1" \
     "$BIN" register-agent "$T/ag1.sock" >/dev/null 2>&1; then
  ok "register-agent"
else
  bad "register-agent"
fi

if run_tmux proxy; then
  ok "proxy exits 0"
else
  bad "proxy exited non-zero; run it with --no-daemon to see the error"
fi
sleep 1

RT="$SOCKET_DIR/tmux-ssh-agent-proxy-$(id -u)"
SOCK=$(tmux -L smoke show-environment -g SSH_AUTH_SOCK 2>/dev/null | cut -d= -f2)

head_ "3. Socket path length"
if [ -n "$SOCK" ]; then
  note "socket = $SOCK"
  note "length = ${#SOCK} bytes, Linux allows 107"
  if [ "${#SOCK}" -le 107 ]; then ok "socket path within sun_path"; else bad "socket path too long"; fi
  if [ -S "$SOCK" ]; then ok "socket exists"; else bad "socket missing"; fi
else
  bad "SSH_AUTH_SOCK was never set in the tmux environment"
fi

head_ "4. Permissions"
check_mode() {
  actual=$(stat -c '%a' "$1" 2>/dev/null)
  if [ "$actual" = "$2" ]; then ok "$3 is $2"; else bad "$3 is ${actual:-missing}, expected $2"; fi
}
check_mode "$RT" 700 "runtime dir"
[ -n "$SOCK" ] && check_mode "$SOCK" 600 "socket"
check_mode "$RT/state" 600 "state file"

head_ "5. Agent forwarding"
if [ -n "$SOCK" ] && SSH_AUTH_SOCK="$SOCK" timeout 15 ssh-add -l 2>/dev/null | grep -q KEY1; then
  ok "ssh-add -l through the proxy lists the registered key"
else
  bad "ssh-add -l through the proxy did not list the key"
fi

head_ "6. Single instance"
# 'run tmux-ssh-agent proxy' fires on every 'tmux source-file', so repeated
# starts must not leave extra daemons or orphaned sockets behind.
run_tmux proxy >/dev/null 2>&1
run_tmux proxy >/dev/null 2>&1
sleep 1
N_DAEMON=$(our_daemon_count)
N_SOCK=$(find "$RT" -maxdepth 1 -type s | wc -l | tr -d ' ')
if [ "$N_DAEMON" = "1" ]; then ok "three starts left one daemon"; else bad "$N_DAEMON daemons running"; fi
if [ "$N_SOCK" = "1" ]; then ok "three starts left one socket"; else bad "$N_SOCK sockets in the runtime dir"; fi

head_ "7. Per-pane routing"
tmux -L smoke -f /dev/null new-session -d -s s2 bash
setsid script -qfc "tmux -L smoke attach -t s2" /dev/null >/dev/null 2>&1 &
sleep 2
TTY2=$(tmux -L smoke list-clients -F '#{client_tty} #{session_name}' | awk '$2=="s2"{print $1}')
ssh-agent -s -a "$T/ag2.sock" >/dev/null
ssh-keygen -q -t ed25519 -N '' -f "$T/k2" -C KEY2
SSH_AUTH_SOCK="$T/ag2.sock" ssh-add "$T/k2" >/dev/null 2>&1
env -u XDG_RUNTIME_DIR -u RUNTIME_DIRECTORY TMPDIR="$SOCKET_DIR" SSH_TTY="$TTY2" \
    "$BIN" register-agent "$T/ag2.sock" >/dev/null 2>&1

# Make s2's client the most recently active, so a server-wide heuristic would
# answer "s2" for both panes. This is what fails without per-pane routing.
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
elif [ -n "$SAW1" ] && [ "$SAW1" = "$SAW2" ]; then
  bad "both panes got $SAW1: fell back to the server-wide heuristic"
else
  bad "unexpected result; see $T/out.s1 and $T/out.s2"
fi

head_ "8. Fallback from outside any pane"
if [ -n "$SOCK" ] && SSH_AUTH_SOCK="$SOCK" timeout 15 ssh-add -l 2>/dev/null | grep -q 'KEY[0-9]'; then
  ok "a request from outside tmux still resolves an agent"
else
  bad "fallback path returned no agent"
fi

head_ "9. Hostile packet"
# A four-byte header claiming 4 GiB must be refused, not allocated.
DPID=$(our_daemons | head -1)
RSS_BEFORE=$(ps -o rss= -p "$DPID" 2>/dev/null | tr -d ' ')
python3 - "$SOCK" <<'PY' >/dev/null 2>&1
import socket, sys
s = socket.socket(socket.AF_UNIX); s.connect(sys.argv[1])
s.sendall(b"\xff\xff\xff\xff"); s.settimeout(10)
try: s.recv(16)
except Exception: pass
PY
sleep 1
RSS_AFTER=$(ps -o rss= -p "$DPID" 2>/dev/null | tr -d ' ')
note "daemon RSS ${RSS_BEFORE:-?} kB -> ${RSS_AFTER:-?} kB"
if [ "$(our_daemon_count)" = "1" ]; then
  ok "daemon survived an oversized length prefix"
else
  bad "daemon died on an oversized length prefix"
fi
if [ -n "$SOCK" ] && SSH_AUTH_SOCK="$SOCK" timeout 15 ssh-add -l >/dev/null 2>&1; then
  ok "still serving normal requests afterwards"
else
  bad "no longer serving requests"
fi

head_ "10. Shutdown is clean"
for p in $(our_daemons); do kill -TERM "$p" 2>/dev/null; done
sleep 5
if [ "$(our_daemon_count)" = "0" ]; then
  ok "daemon exited"
else
  bad "daemon still running"
fi
LEFT=$(find "$RT" -maxdepth 1 -type s | wc -l | tr -d ' ')
if [ "$LEFT" = "0" ]; then ok "socket removed on SIGTERM"; else bad "$LEFT socket(s) left behind"; fi

printf '\n== Summary ==\n  %d passed, %d failed\n' "$PASS" "$FAIL"
[ "$FAIL" -eq 0 ] || exit 1
