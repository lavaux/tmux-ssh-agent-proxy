use crate::ssh_agent;
use crate::state::StateFile;
use crate::tmux;
use anyhow::{Context, Result};
use nix::sys::stat::{umask, Mode};
use std::fs::Permissions;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

/// Mode the proxy socket must have. Connecting to a Unix socket requires write
/// permission on it, so anything looser hands the upstream SSH agent to every
/// local account.
const SOCKET_MODE: u32 = 0o600;

/// Transparent SSH agent proxy that forwards requests from a local Unix socket
/// to the upstream agent assigned to the currently active tmux client.
///
/// Spawns one thread per accepted client connection.
pub struct ProxyAgent {
    state_file: Arc<StateFile>,
}

impl ProxyAgent {
    /// Create a new proxy agent bound to the runtime state file.
    pub fn new() -> Result<Self> {
        Ok(Self {
            state_file: Arc::new(StateFile::open_default()?),
        })
    }

    /// Run the accept loop:
    /// 1. Create a random-named Unix socket in the runtime dir.
    /// 2. Set `SSH_AUTH_SOCK` in tmux global environment.
    /// 3. Call `ready` callback if provided (used by daemonized startup to signal parent).
    /// 4. Accept connections and spawn a handler thread for each.
    /// 5. Exit the loop when tmux server goes away (ping fails).
    /// 6. Clean up the socket file on exit.
    pub fn run(&self, ready: Option<Box<dyn FnOnce() + Send>>) -> Result<()> {
        let runtime_dir = StateFile::runtime_dir()?;

        // The socket inherits `0777 & ~umask` from bind(2), so a lax umask in
        // the invoking shell would publish a world-usable agent socket.
        umask(Mode::from_bits_truncate(0o077));

        let hex: String = (0..16)
            .map(|_| format!("{:02x}", rand::random::<u8>()))
            .collect();
        let socket_path = runtime_dir.join(hex);

        let listener = UnixListener::bind(&socket_path)
            .with_context(|| format!("Failed to bind {socket_path:?}"))?;
        // Belt and braces: the umask above already covers this, but set the
        // mode explicitly so the guarantee does not depend on it.
        std::fs::set_permissions(&socket_path, Permissions::from_mode(SOCKET_MODE))
            .with_context(|| format!("Failed to set permissions on {socket_path:?}"))?;
        listener.set_nonblocking(true)?;

        tmux::set_environment("SSH_AUTH_SOCK", &socket_path.to_string_lossy())?;

        if let Some(f) = ready {
            f();
        }

        loop {
            if !tmux::ping() {
                break;
            }

            match listener.accept() {
                Ok((stream, _)) => {
                    let state_file = Arc::clone(&self.state_file);
                    thread::spawn(move || {
                        serve_client(stream, &state_file);
                    });
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_secs(3));
                }
                Err(e) => {
                    eprintln!("accept error: {}", e);
                    break;
                }
            }
        }

        let _ = std::fs::remove_file(&socket_path);

        Ok(())
    }
}

/// Handle one downstream client connection.
/// Looks up the correct upstream agent for the current tmux client,
/// connects to it, and bidirectionally forwards SSH agent protocol packets.
/// Returns silently when either side disconnects or on any I/O error.
fn serve_client(mut downstream: UnixStream, state_file: &StateFile) {
    let agent_path = match get_agent_for_client(state_file) {
        Some(p) => p,
        None => return,
    };

    // Re-validate before connecting: the state file may have changed since the
    // path was registered, and this is where the keys actually go.
    let agent_path = match ssh_agent::validate_agent_socket(&agent_path) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("refusing upstream agent: {e:#}");
            return;
        }
    };

    let mut upstream = match UnixStream::connect(&agent_path) {
        Ok(s) => s,
        Err(_) => return,
    };

    loop {
        let payload = match ssh_agent::recv_packet(&mut downstream) {
            Ok(Some(p)) => p,
            _ => break,
        };

        let response = match ssh_agent::request(&mut upstream, &payload) {
            Ok(Some(r)) => r,
            _ => {
                let _ = ssh_agent::send_packet(&mut downstream, &[ssh_agent::SSH_AGENT_FAILURE]);
                break;
            }
        };

        if ssh_agent::send_packet(&mut downstream, &response).is_err() {
            break;
        }
    }
}

/// Look up the agent socket path for the currently active tmux client.
/// First checks per-TTY registration, then falls back to the default agent.
/// Returns `None` if no agent is registered and no default is set.
fn get_agent_for_client(state_file: &StateFile) -> Option<String> {
    // Resolve the client before taking the lock: this forks a tmux process, and
    // doing that under the lock would serialise every concurrent handler behind
    // a fork/exec.
    let tty = tmux::current_client();

    state_file
        .with_lock_read(|state| match tty {
            // A per-TTY registration wins; the default covers a client that has
            // not registered one.
            Some(tty) => state
                .agents
                .get(&tty)
                .cloned()
                .or_else(|| state.default_agent.clone()),
            // No active tmux client: there is no way to pick an agent, and
            // falling back to the default here would hand keys to whichever
            // agent happened to be registered last.
            None => None,
        })
        .ok()?
}
