# tmux-ssh-agent

Transparent SSH agent proxy for tmux. Each tmux client can use its own SSH agent without conflict. The proxy selects an agent automatically based on which tmux client is active.

## How it works

When you launch tmux, the proxy starts a forwarding Unix socket and sets `SSH_AUTH_SOCK` to point at it. When an SSH client connects to this socket, the proxy looks up which SSH agent belongs to the currently active tmux client and forwards requests to it.

This lets you attach to tmux from different machines, each with its own SSH agent, and have SSH work in every pane without a stale socket or a "wrong agent" error.

### Which agent gets picked

In order:

1. If a tmux client is active and its TTY has a registered agent, that agent is used.
2. If a tmux client is active and its TTY has no registration, the default agent is used, if one is set.
3. If no tmux client is active, no agent is returned. There is nothing to identify the caller by, and falling back to the default here would mean handing keys to whichever agent happened to be registered last.

Known limitation: "active client" means the most recently active client on the whole tmux server, not the client that owns the pane the request came from. With a single client attached this is exact. With several attached at once, whichever one you typed in most recently wins for every pane. Routing per pane requires identifying the connecting process, which is not yet implemented.

## Security

The proxy socket grants full use of your SSH agent: not just signing, but every agent operation, including adding and removing identities. Anyone who can connect to it can use your keys.

So the proxy keeps everything owner-only:

- The runtime directory is created `0700`. If it already exists, it is checked: it must be a real directory owned by you, or the proxy refuses to start.
- The socket is `0600`, and the daemon sets its umask to `0077` before binding.
- The state file is `0600`.
- A registered agent path must be an existing socket owned by you. This is checked when you register it and again before the proxy connects to it.

The runtime directory is, in order of preference:

1. `$RUNTIME_DIRECTORY` (set by systemd)
2. `$XDG_RUNTIME_DIR/tmux-ssh-agent-proxy`
3. `$TMPDIR/tmux-ssh-agent-proxy-$UID`, defaulting to `/tmp`

It holds the proxy socket, the `state` file mapping TTYs to agents, and `proxy.lock`.

Developed and tested on Linux. It should work on other unix systems, with one caveat: the accept loop assumes an accepted socket does not inherit the listener's non-blocking flag. That holds on Linux; on the BSDs it does not, so the proxy sets the accepted socket back to blocking explicitly.

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

**Registration fails with "is not a socket" or "does not exist".**
The path is checked before it is stored. A stale `$SSH_AUTH_SOCK` from a dead agent gives this.

**The proxy refuses to start, citing the runtime directory.**
Something else owns that directory, or it is a symlink. On a shared machine with the `/tmp` fallback, that is worth looking into before working around it.

**Nothing happens when tmux starts.**
Run `tmux-ssh-agent proxy --no-daemon` from inside tmux to see errors directly. The daemon sends its output to `/dev/null`.

## License

MIT
