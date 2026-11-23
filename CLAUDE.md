# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

`tmux-ssh-agent`: a single Rust binary that runs a per-tmux-server SSH agent proxy. Panes get `SSH_AUTH_SOCK` pointing at the proxy socket; each connection is forwarded to the SSH agent registered for the tmux client the request belongs to. README.md is the user-facing spec (routing rules, security guarantees, macOS notes) and is kept in sync with behaviour: update it when behaviour changes.

## Commands

CI (`.forgejo/workflows/ci.yml`) runs all of these; match them before considering work done:

```sh
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo test --all-targets
cargo test <name_substring>            # single test, e.g. cargo test parse_ppid
# MSRV is 1.71 and CI builds with exactly that toolchain: no newer std APIs or syntax
cargo +1.71 build --all-targets
# macOS cfg branches are typechecked from Linux (clippy doesn't link):
rustup target add aarch64-apple-darwin
cargo clippy --all-targets --target aarch64-apple-darwin -- -D warnings
```

Runtime smoke tests (need tmux, ssh-agent, ssh-keygen, `script`; they build the debug binary themselves and only touch a scratch dir under `/tmp`, using `tmux -f /dev/null`):

```sh
./contrib/linux-smoke-test.sh     # run from repo root
./contrib/macos-smoke-test.sh     # Mac only; the only real runtime check of macOS behaviour
```

Debug the daemon in the foreground from inside tmux: `tmux-ssh-agent proxy --no-daemon` (the daemon otherwise sends stdio to `/dev/null`).

## Architecture

No CLI framework; `cli.rs` hand-parses `std::env::args` and dispatches `cmd_*` functions. Modules:

- `cli.rs` — subcommands, plus the daemonisation: fork → setsid → fork, with a pipe the grandchild writes `ok` on once the socket is published, so `tmux run 'tmux-ssh-agent proxy'` only returns after the env is set. Between fork and the main loop, avoid anything that touches Darwin system frameworks (getpwuid, getaddrinfo, etc.) — macOS aborts forked-not-exec'd processes that do.
- `proxy.rs` — `ProxyAgent::run`: the accept loop. Socket name is a hash of the tmux socket path (from `$TMUX`), deliberately stable so a restarted proxy rebinds the path existing shells already hold. An `InstanceLock` (flock) keeps one daemon per tmux server; if one already holds it, `run` still publishes `SSH_AUTH_SOCK` and returns. The loop exits when `tmux::ping()` fails or on SIGTERM/INT/HUP. One thread per connection (capped at 64); each re-validates the upstream agent socket, then relays length-prefixed packets.
- `routing.rs` — maps a connection to a tmux client TTY: peer pid (`SO_PEERCRED` on Linux, `LOCAL_PEERPID` on macOS) → walk parents (`/proc` / libproc) until a pane pid → session → active client in that session. Returns `None` on failure, and `proxy.rs` falls back to `tmux::current_client()`. Platform code is `cfg`-gated per OS with a stub for other unices; keep the shared logic in pure functions (`match_session`, `parse_ppid`) so it stays unit-testable.
- `tmux.rs` — thin wrappers that shell out to `tmux` and parse output; parsers are separate pure functions with tests. Also manages `update-environment`: the proxy removes `SSH_AUTH_SOCK` from it and unsets it in existing sessions, otherwise the session env shadows the global one.
- `state.rs` — runtime dir resolution (`$RUNTIME_DIRECTORY` > `$XDG_RUNTIME_DIR/tmux-ssh-agent-proxy` > `$TMPDIR/tmux-ssh-agent-proxy-$UID`), ownership/mode validation, and the JSON state file (TTY → agent path, plus `default_agent`) under flock. The proxy hot path uses `with_lock_read`; mutations use `with_lock`. Unparseable state is moved aside, never discarded.
- `ssh_agent.rs` — packet framing (`MAX_PACKET_LEN` cap, matching OpenSSH) and `validate_agent_socket` (must be an existing socket owned by the user; paths are canonicalised).
- `config_edit.rs` — pure text transforms for `install`/`uninstall` marker blocks in `~/.tmux.conf` and shell profiles (POSIX vs fish), plus backup and atomic write. Removal requires the end marker and refuses to guess.

`contrib/tmux-ssh-agent.rb` is the original Ruby implementation, kept for reference only; it is unmaintained and predates the security fixes.

## Conventions worth knowing

- Security invariants are load-bearing and documented in README "Security": runtime dir 0700 and owner-checked, socket and state file 0600, umask 0077 before bind, peer uid checked on every connection, no default-agent fallback when no tmux client can be identified. Don't loosen these.
- Socket path length is a real constraint (107 usable bytes on Linux, 103 on Darwin, and macOS `$TMPDIR` is ~49 chars); `check_socket_path_fits` enforces it.
- Comments explain *why* (often the bug a line prevents); preserve that style when editing.
- Unit tests live in `#[cfg(test)] mod tests` at the bottom of each module; there is no `tests/` directory.
