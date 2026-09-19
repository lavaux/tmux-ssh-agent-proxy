# tmux-ssh-agent

Transparent SSH agent proxy for tmux. Each tmux client can use its own SSH agent without conflict. The proxy selects an agent automatically based on which tmux client is active.

## How it works

When you launch tmux, the proxy starts a forwarding Unix socket and sets `SSH_AUTH_SOCK` to point at it. When an SSH client connects to this socket, the proxy looks up which SSH agent belongs to the currently active tmux client and forwards requests to it.

This lets you attach to tmux from different machines, each with its own SSH agent, and have SSH work in every pane without a stale socket or a "wrong agent" error.

### Which agent gets picked

First the proxy works out which tmux client the request belongs to:

1. It asks the socket for the pid on the other end (`SO_PEERCRED`), walks that process up the tree until it reaches a tmux pane process, and takes that pane's session. The agent is then chosen from the clients attached to that session. This is what makes several attached clients work: a request from a pane is answered by that pane's client, whoever typed most recently elsewhere.
2. If the peer cannot be placed in a pane, for instance because it is not running under tmux at all, it falls back to the most recently active client on the server.

Then, given that client's TTY:

1. If the TTY has a registered agent, that agent is used.
2. Otherwise the default agent is used, if one is set.
3. If no tmux client could be identified at all, no agent is returned. There is nothing to identify the caller by, and falling back to the default here would mean handing keys to whichever agent happened to be registered last.

Step 1 of the first list needs `SO_PEERCRED` and `/proc`, so it is Linux-only. On other unix systems the proxy uses the fallback, which is exact as long as one client is attached.

## Security

The proxy socket grants full use of your SSH agent: not just signing, but every agent operation, including adding and removing identities. Anyone who can connect to it can use your keys.

So the proxy keeps everything owner-only:

- The runtime directory is created `0700`. If it already exists, it is checked: it must be a real directory owned by you, or the proxy refuses to start.
- The socket is `0600`, and the daemon sets its umask to `0077` before binding.
- The state file is `0600`.
- A registered agent path must be an existing socket owned by you. This is checked when you register it and again before the proxy connects to it.
- Connections from another uid are refused outright, rather than relying on the socket mode alone.

The runtime directory is, in order of preference:

1. `$RUNTIME_DIRECTORY` (set by systemd)
2. `$XDG_RUNTIME_DIR/tmux-ssh-agent-proxy`
3. `$TMPDIR/tmux-ssh-agent-proxy-$UID`, defaulting to `/tmp`

It holds the proxy socket, the `state` file mapping TTYs to agents, and `proxy.lock`.

Developed and tested on Linux. It builds and runs on other unix systems, with two caveats: per-pane routing needs `SO_PEERCRED` and `/proc`, so elsewhere the proxy uses the server-wide fallback described above; and the accept loop assumes an accepted socket does not inherit the listener's non-blocking flag, which holds on Linux but not on the BSDs, so the proxy sets it back to blocking explicitly.

## Build

Requires Rust 1.71 or later. CI builds against exactly that version, so the claim is enforced rather than aspirational.

```sh
cargo build --release
```

Binary at `target/release/tmux-ssh-agent`. Install it somewhere on `$PATH`:

```sh
cp target/release/tmux-ssh-agent ~/.local/bin/
```

## Setup

### Quick install

```sh
tmux-ssh-agent install
```

This adds a marked block to `~/.tmux.conf`, backing up the original first. Reload tmux after:

```sh
tmux source-file ~/.tmux.conf
```

Re-running `install` is a no-op, and re-sourcing `tmux.conf` will not start a second daemon.

To also add the login registration snippet to your shell profile:

```sh
tmux-ssh-agent install --shell
```

The profile is chosen from `$SHELL`: `~/.bashrc` for bash, `~/.zshrc` for zsh, `~/.config/fish/config.fish` for fish, which gets fish syntax rather than POSIX. If `$SHELL` is unset or unrecognised, the first of those that already exists is used.

Everything written is bracketed by marker comments:

```
# Added by tmux-ssh-agent install
...
# End tmux-ssh-agent
```

`uninstall` removes only what is between those markers. If you edit the block by hand and remove the end marker, `uninstall` will tell you to remove it yourself rather than guess where it ends.

### Manual setup

#### 1. Start the proxy inside tmux

Add to `~/.tmux.conf`:

```
run 'tmux-ssh-agent proxy'
```

This forks a daemon that creates a random-named Unix socket and sets the tmux global environment variable `SSH_AUTH_SOCK` to point at it. The proxy runs as long as the tmux server does, and only one runs at a time.

To run in the foreground for debugging:

```sh
tmux-ssh-agent proxy --no-daemon
```

#### 2. Register an agent on login

In your shell profile:

```sh
if [ -z "$TMUX" ] && [ -n "$SSH_AUTH_SOCK" ]; then
  tmux-ssh-agent register-agent
fi
```

This registers the current SSH agent for the current TTY. When you later use that TTY inside tmux, the proxy forwards to this agent.

#### 3. Register a default agent (optional)

```sh
tmux-ssh-agent register-default-agent
```

Sets the fallback described under "Which agent gets picked". Use `--override` to replace an existing default.

## Commands

| Command | Description |
|---|---|
| `proxy [--no-daemon]` | Start the proxy under the tmux server, or run it in the foreground |
| `install [--shell]` | Add config to `~/.tmux.conf` (and optionally the shell profile) |
| `uninstall [--shell]` | Remove config from `~/.tmux.conf` (and optionally the shell profile) |
| `register-agent [SSH_AUTH_SOCK]` | Register an agent for the current TTY |
| `register-default-agent [--override] [SSH_AUTH_SOCK]` | Register the fallback agent |
| `remove-agent [TTY]` | Remove the registration for a TTY (defaults to current) |
| `clear-agents [--yes]` | Remove all registrations and the default; asks first |
| `list-agents` / `list` | List registrations, marking the current TTY with `*` |
| `--help`, `--version` | Usage and version |

## Troubleshooting

**`ssh-add -l` says the agent has no identities, or cannot connect.**
Check `tmux-ssh-agent list-agents`. If the list is empty, nothing registered: confirm `$SSH_AUTH_SOCK` was set when your login shell ran the registration snippet. If no tmux client is attached, no agent is returned by design.

**A pane is using the wrong agent.**
`list-agents` marks the current TTY with `*`. A pane is routed by the session it belongs to, so check that the client attached to that session is the one whose TTY you registered. A process that detached itself from the pane's process tree, such as something started with `setsid`, cannot be traced back to a pane and falls back to the most recently active client.

**Registration fails with "is not a socket" or "does not exist".**
The path is checked before it is stored. A stale `$SSH_AUTH_SOCK` from a dead agent gives this.

**The proxy refuses to start, citing the runtime directory.**
Something else owns that directory, or it is a symlink. On a shared machine with the `/tmp` fallback, that is worth looking into before working around it.

**Nothing happens when tmux starts.**
Run `tmux-ssh-agent proxy --no-daemon` from inside tmux to see errors directly. The daemon sends its output to `/dev/null`.

## License

MIT
