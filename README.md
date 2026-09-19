# tmux-ssh-agent

Transparent SSH agent proxy for tmux. Each tmux client (pane/window) can use its own SSH agent without conflict. The proxy selects the right agent automatically based on which tmux client is active.

## How it works

When you launch tmux, the proxy starts a forwarding Unix socket and sets `SSH_AUTH_SOCK` to point at it. When an SSH client connects to this socket, the proxy looks up which SSH agent belongs to the currently active tmux client and forwards requests to it. If no agent is registered for that client, it falls back to a default agent.

This lets you attach to tmux from different machines (each with its own SSH agent) and have SSH just work in every pane — no stale socket, no "wrong agent" errors.

## Build

Requires Rust 1.63+.

```sh
cargo build --release
```

Binary at `target/release/tmux-ssh-agent`. Install it somewhere on `$PATH`:

```sh
cp target/release/tmux-ssh-agent ~/.local/bin/
```

## Usage

### 1. Start the proxy inside tmux

Add to `~/.tmux.conf`:

```
run 'tmux-ssh-agent proxy'
```

This forks a daemon that creates a random-named Unix socket and sets the tmux global environment variable `SSH_AUTH_SOCK` to point at it. The proxy survives as long as the tmux server is running.

To run in the foreground (debugging):

```sh
tmux-ssh-agent proxy --no-daemon
```

### 2. Register an agent on login

In your shell profile (`~/.bashrc`, `~/.zshrc`, etc.):

```sh
if [ -z "$TMUX" ] && [ -n "$SSH_AUTH_SOCK" ]; then
  tmux-ssh-agent register-agent
fi
```

This registers the current SSH agent for the current TTY. When you later use that TTY inside tmux (via `attach`, `new-window`, etc.), the proxy forwards to this agent.

### 3. Register a default agent (optional)

```sh
tmux-ssh-agent register-default-agent
```

Sets a fallback agent used when no per-TTY agent is registered.

Use `--override` to replace an existing default:

```sh
tmux-ssh-agent register-default-agent --override
```

## Commands

| Command | Description |
|---|---|
| `proxy` | Start proxy daemon under tmux server |
| `proxy --no-daemon` | Run proxy in foreground |
| `register-agent [SSH_AUTH_SOCK]` | Register agent for current TTY (reads `SSH_AUTH_SOCK` or arg) |
| `register-default-agent [--override] [SSH_AUTH_SOCK]` | Register fallback agent |
| `list-agents` / `list` | List all registered agents, mark current TTY with `*` |

## License

MIT
