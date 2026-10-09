use crate::routing;
use crate::ssh_agent;
use crate::state::StateFile;
use crate::tmux;
use anyhow::{Context, Result};
use fs2::FileExt;
use nix::poll::{poll, PollFd, PollFlags, PollTimeout};
use nix::sys::stat::{umask, Mode};
use std::fs::Permissions;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::os::unix::io::AsFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

/// Mode the proxy socket must have. Connecting to a Unix socket requires write
/// permission on it, so anything looser hands the upstream SSH agent to every
/// local account.
const SOCKET_MODE: u32 = 0o600;

/// Usable bytes in `sockaddr_un.sun_path`, less the NUL terminator.
///
/// Linux gives 108, the BSDs including Darwin give 104. This is a real
/// constraint rather than a formality on macOS, where `$TMPDIR` is a per-user
/// path around 49 characters before anything of ours is appended.
#[cfg(target_os = "linux")]
const MAX_SOCKET_PATH_LEN: usize = 107;
#[cfg(not(target_os = "linux"))]
const MAX_SOCKET_PATH_LEN: usize = 103;

/// Bytes of the tmux server key in the socket name, rendered as twice as many
/// hex characters.
///
/// Eight keeps the whole path inside Darwin's limit with room to spare. The
/// socket sits in a 0700 directory whose ownership is checked, so the name is
/// not an access control boundary and 64 bits is ample against collision.
const SOCKET_NAME_BYTES: usize = 8;

/// How long to wait for a connection before checking whether tmux is still
/// alive. The listener wakes immediately on a connection, so this is the idle
/// poll interval, not added latency.
const POLL_TIMEOUT_MS: u16 = 3_000;

/// Minimum gap between `tmux display-message` liveness checks, so a burst of
/// connections cannot fork one tmux process per connection.
const PING_INTERVAL: Duration = Duration::from_secs(3);

/// How long to wait on the upstream agent before giving up. Only applied to the
/// upstream: waiting for the next request from a downstream client is normal
/// and may take as long as the SSH session lives.
const UPSTREAM_TIMEOUT: Duration = Duration::from_secs(30);

/// How long to wait for a write to drain before giving up on a peer.
const WRITE_TIMEOUT: Duration = Duration::from_secs(30);

/// Maximum number of connections served at once. Each takes a thread, and
/// before this cap a client could spawn them without limit.
const MAX_CONCURRENT_CLIENTS: usize = 64;

/// Number of handler threads currently running.
static ACTIVE_CLIENTS: AtomicUsize = AtomicUsize::new(0);

/// Set by the signal handler to ask the accept loop to shut down.
static SHUTDOWN: AtomicBool = AtomicBool::new(false);

/// Signal handler. Only an atomic store, which is async-signal-safe.
extern "C" fn handle_shutdown(_: nix::libc::c_int) {
    SHUTDOWN.store(true, Ordering::SeqCst);
}

/// Ask for SIGTERM, SIGINT and SIGHUP to stop the accept loop rather than kill
/// the process outright, so the socket file is unlinked on the way out.
/// Deliberately installed without SA_RESTART: a pending accept should fail with
/// EINTR so the loop notices the flag promptly.
fn install_signal_handlers() -> Result<()> {
    use nix::sys::signal::{sigaction, SaFlags, SigAction, SigHandler, SigSet, Signal};

    let action = SigAction::new(
        SigHandler::Handler(handle_shutdown),
        SaFlags::empty(),
        SigSet::empty(),
    );
    for sig in [Signal::SIGTERM, Signal::SIGINT, Signal::SIGHUP] {
        unsafe { sigaction(sig, &action) }
            .with_context(|| format!("Failed to install handler for {sig:?}"))?;
    }
    Ok(())
}

/// Decrements the active-client count when a handler thread finishes.
struct ClientSlot;

impl Drop for ClientSlot {
    fn drop(&mut self) {
        ACTIVE_CLIENTS.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Holds the flock that makes the daemon single-instance.
///
/// `run 'tmux-ssh-agent proxy'` fires on every `tmux source-file`, so without
/// this every config reload started another daemon, rebound SSH_AUTH_SOCK and
/// orphaned the previous socket.
struct InstanceLock {
    _file: std::fs::File,
}

impl InstanceLock {
    /// Take the lock for the tmux server identified by `key`, or return `None`
    /// if another daemon already holds it.
    fn acquire(runtime_dir: &Path, key: &str) -> Result<Option<Self>> {
        use std::os::unix::fs::OpenOptionsExt;

        let path = runtime_dir.join(format!("{key}.lock"));
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(&path)
            .with_context(|| format!("Failed to open {path:?}"))?;

        match FileExt::try_lock_exclusive(&file) {
            Ok(()) => Ok(Some(Self { _file: file })),
            Err(_) => Ok(None),
        }
    }
}

/// Unlinks the proxy socket when the daemon stops, however it stops.
///
/// The previous code only unlinked after a normal loop exit, so an error on the
/// way up, or any signal, left the socket behind for good.
struct SocketGuard {
    path: PathBuf,
}

impl Drop for SocketGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Remove sockets in the runtime directory that no process is listening on.
///
/// Every abrupt termination before this release leaked one, and nothing ever
/// collected them. A socket with a live daemon behind it accepts connections,
/// so only the dead ones are removed.
fn sweep_stale_sockets(runtime_dir: &Path) {
    let Ok(entries) = std::fs::read_dir(runtime_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        match entry.metadata() {
            Ok(meta) if meta.file_type().is_socket() => {}
            _ => continue,
        }
        // Only a refused connection proves nobody is listening. Treating every
        // error as "stale" would unlink a socket a live daemon is serving when
        // the failure was, say, a path too long or a transient permission error.
        match UnixStream::connect(&path) {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::ConnectionRefused => {
                let _ = std::fs::remove_file(&path);
            }
            Err(_) => {}
        }
    }
}

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
    /// 1. Create a Unix socket in the runtime dir, named after the tmux server.
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

        // The name is fixed per tmux server rather than random. Shells keep the
        // SSH_AUTH_SOCK they were started with forever, so a proxy restarted
        // under a new name strands every pane that already exists; with a
        // stable name the replacement binds the same path and they keep working.
        let key = server_key(&tmux_socket_path()?);
        let socket_path = runtime_dir.join(&key);
        check_socket_path_fits(&socket_path)?;

        let Some(_instance) = InstanceLock::acquire(&runtime_dir, &key)? else {
            // Another daemon is already serving this socket. Still publish it:
            // the caller may be a tmux server that started after that daemon did
            // (the previous one exited and a new one took over its socket
            // path), and nothing else would ever set its environment.
            publish("SSH_AUTH_SOCK", &socket_path)?;
            if let Some(f) = ready {
                f();
            }
            return Ok(());
        };

        sweep_stale_sockets(&runtime_dir);
        // Holding the lock means no daemon is serving this path, so anything
        // still there is left over from one that died, and bind would fail on it.
        if std::fs::symlink_metadata(&socket_path).is_ok_and(|m| m.file_type().is_socket()) {
            let _ = std::fs::remove_file(&socket_path);
        }
        install_signal_handlers()?;

        let listener = UnixListener::bind(&socket_path)
            .with_context(|| format!("Failed to bind {socket_path:?}"))?;
        // From here on the socket file is removed however this function exits.
        let _socket_guard = SocketGuard {
            path: socket_path.clone(),
        };
        // Belt and braces: the umask above already covers this, but set the
        // mode explicitly so the guarantee does not depend on it.
        std::fs::set_permissions(&socket_path, Permissions::from_mode(SOCKET_MODE))
            .with_context(|| format!("Failed to set permissions on {socket_path:?}"))?;
        listener.set_nonblocking(true)?;

        publish("SSH_AUTH_SOCK", &socket_path)?;

        if let Some(f) = ready {
            f();
        }

        let mut last_ping = Instant::now();

        loop {
            if SHUTDOWN.load(Ordering::SeqCst) {
                break;
            }

            if last_ping.elapsed() >= PING_INTERVAL {
                if !tmux::ping() {
                    break;
                }
                last_ping = Instant::now();
            }

            // Wait for a connection rather than sleeping blindly. The previous
            // code slept a flat 3 s whenever accept would have blocked, so a
            // connection arriving just after the sleep started waited it out.
            match wait_for_connection(&listener) {
                Ok(true) => {}
                Ok(false) => continue,
                Err(_) => break,
            }

            match listener.accept() {
                Ok((stream, _)) => {
                    if ACTIVE_CLIENTS.load(Ordering::SeqCst) >= MAX_CONCURRENT_CLIENTS {
                        // Dropping the stream closes it; the client sees a
                        // refused agent rather than the daemon exhausting itself.
                        continue;
                    }
                    ACTIVE_CLIENTS.fetch_add(1, Ordering::SeqCst);
                    let state_file = Arc::clone(&self.state_file);
                    if thread::Builder::new()
                        .spawn(move || {
                            let _slot = ClientSlot;
                            serve_client(stream, &state_file);
                        })
                        .is_err()
                    {
                        ACTIVE_CLIENTS.fetch_sub(1, Ordering::SeqCst);
                    }
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
                // A signal arriving mid-accept is how shutdown reaches us.
                Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => {
                    eprintln!("accept error: {}", e);
                    break;
                }
            }
        }

        Ok(())
    }
}

/// The tmux server's socket path: the first field of `$TMUX`, which tmux sets
/// as `socket_path,server_pid,session_id`.
///
/// Only the path identifies the server for our purposes. The pid changes when a
/// server exits and a new one starts on the same path, but every `tmux` command
/// the daemon runs goes by the path, so it is the same server as far as the
/// daemon can tell.
fn tmux_socket_path() -> Result<String> {
    let tmux = std::env::var("TMUX").context("Not running under tmux server")?;
    let path = tmux.split(',').next().unwrap_or_default();
    if path.is_empty() {
        anyhow::bail!("$TMUX does not name a socket: {tmux:?}");
    }
    Ok(path.to_string())
}

/// A short, stable name derived from the tmux socket path.
///
/// FNV-1a rather than `DefaultHasher`, whose algorithm the standard library
/// does not promise to keep: a new build of the proxy has to arrive at the same
/// name as the one that published the path panes are already using.
fn server_key(tmux_socket: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in tmux_socket.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    let hex = format!("{hash:016x}");
    hex[..SOCKET_NAME_BYTES * 2].to_string()
}

/// Point the calling tmux server's panes at the proxy socket.
fn publish(name: &str, socket_path: &Path) -> Result<()> {
    tmux::set_environment(name, &socket_path.to_string_lossy())?;
    claim_environment(name)
}

/// Stop tmux from overwriting the variable we just set.
///
/// Setting the global environment is not enough on its own. tmux's
/// `update-environment` option copies a list of variables from the attaching
/// client into the *session* environment on every new-session and every
/// attach, and the session environment shadows the global one.
/// `SSH_AUTH_SOCK` is on that list by default, so the proxy would publish its
/// socket and tmux would immediately hide it behind the client's own agent.
/// That is the exact problem this tool exists to solve, so it has to be dealt
/// with rather than assumed away.
///
/// Two steps: drop the variable from `update-environment` so no future attach
/// re-shadows it, and clear the stale copy from sessions that already have one,
/// which removing it from the list does not do.
///
/// This only touches a global option, which lives exactly as long as the tmux
/// server does, and so does this daemon. It is not a lasting edit to the user's
/// configuration.
fn claim_environment(name: &str) -> Result<()> {
    if let Some(vars) = tmux::update_environment() {
        if vars.iter().any(|v| v == name) {
            let kept: Vec<String> = vars.into_iter().filter(|v| v != name).collect();
            tmux::set_update_environment(&kept)
                .with_context(|| format!("Failed to take {name} out of update-environment"))?;
        }
    }

    for session in tmux::sessions().unwrap_or_default() {
        tmux::unset_session_environment(&session, name);
    }

    Ok(())
}

/// Refuse to go further if the socket path will not fit in `sockaddr_un`.
///
/// Binding would otherwise fail with a message about `SUN_LEN` that says
/// nothing about what to do, and in the daemon it would not be seen at all
/// because stdio is already `/dev/null` by then.
fn check_socket_path_fits(socket_path: &Path) -> Result<()> {
    let len = socket_path.as_os_str().as_bytes().len();
    if len <= MAX_SOCKET_PATH_LEN {
        return Ok(());
    }
    anyhow::bail!(
        "Socket path is {len} bytes but this platform allows {MAX_SOCKET_PATH_LEN}: \
         {socket_path:?}. Set $XDG_RUNTIME_DIR or $RUNTIME_DIRECTORY to a shorter \
         directory, for example $HOME/.tmux-ssh-agent."
    );
}

/// Block until the listener has a connection ready or the poll times out.
/// `Ok(true)` means a connection is pending, `Ok(false)` means try again.
fn wait_for_connection(listener: &UnixListener) -> Result<bool> {
    let mut fds = [PollFd::new(listener.as_fd(), PollFlags::POLLIN)];
    match poll(&mut fds, PollTimeout::from(POLL_TIMEOUT_MS)) {
        Ok(0) => Ok(false),
        Ok(_) => Ok(true),
        Err(nix::errno::Errno::EINTR) => Ok(false),
        Err(e) => Err(e).context("poll on listener failed"),
    }
}

/// Handle one downstream client connection.
/// Looks up the correct upstream agent for the current tmux client,
/// connects to it, and bidirectionally forwards SSH agent protocol packets.
/// Returns silently when either side disconnects or on any I/O error.
fn serve_client(mut downstream: UnixStream, state_file: &StateFile) {
    // Linux does not pass the listener's non-blocking flag to accepted sockets,
    // but some other unices do. Be explicit so the forwarding loop is blocking
    // wherever this runs.
    if downstream.set_nonblocking(false).is_err() {
        return;
    }
    let _ = downstream.set_write_timeout(Some(WRITE_TIMEOUT));

    // The socket is 0600, so this should be unreachable. Check anyway: the
    // whole point of this process is to decide who may use the user's keys,
    // and that decision should not rest on a file mode alone.
    if let Some(uid) = routing::peer_uid(&downstream) {
        if uid != nix::unistd::getuid().as_raw() {
            eprintln!("refusing connection from uid {uid}");
            return;
        }
    }

    let agent_path = match get_agent_for_client(state_file, &downstream) {
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
    // Bound the wait on the agent: a wedged upstream used to pin this thread
    // for the lifetime of the daemon.
    let _ = upstream.set_read_timeout(Some(UPSTREAM_TIMEOUT));
    let _ = upstream.set_write_timeout(Some(WRITE_TIMEOUT));

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

/// Identify the tmux client this connection belongs to.
///
/// Prefers the pane the connecting process actually sits in, which is the only
/// answer that is correct with more than one client attached. Falls back to the
/// most recently active client server-wide when the peer cannot be placed, for
/// instance when it is not running under tmux at all.
fn resolve_client_tty(downstream: &UnixStream) -> Option<String> {
    if let Some(tty) = routing::client_tty_for_peer(downstream) {
        return Some(tty);
    }
    tmux::current_client()
}

/// Look up the agent socket path for the currently active tmux client.
/// First checks per-TTY registration, then falls back to the default agent.
/// Returns `None` if no agent is registered and no default is set.
fn get_agent_for_client(state_file: &StateFile, downstream: &UnixStream) -> Option<String> {
    // Resolve the client before taking the lock: this forks a tmux process, and
    // doing that under the lock would serialise every concurrent handler behind
    // a fork/exec.
    let tty = resolve_client_tty(downstream);

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

#[cfg(test)]
mod tests {
    use super::*;

    /// A representative macOS `$TMPDIR`: per-user, created by
    /// `confstr(_CS_DARWIN_USER_TEMP_DIR)`, and 49 characters before anything
    /// of ours is appended.
    const DARWIN_TMPDIR: &str = "/var/folders/2z/_0y8k1zj0_35nfvq1n5jn4wm0000gn/T/";

    fn socket_path_for(tmpdir: &str, uid: u32, name_bytes: usize) -> PathBuf {
        PathBuf::from(tmpdir)
            .join(format!("tmux-ssh-agent-proxy-{uid}"))
            .join("a".repeat(name_bytes * 2))
    }

    /// The bug that stopped the daemon ever starting on macOS: the socket name
    /// was 16 random bytes as 32 hex characters, which put the path over
    /// Darwin's 104-byte `sun_path` for every uid.
    #[test]
    fn socket_path_fits_darwins_limit_for_realistic_uids() {
        // The check itself is compiled for this host, so assert against the
        // Darwin number directly rather than MAX_SOCKET_PATH_LEN.
        const DARWIN_MAX: usize = 103;

        for uid in [0, 501, 1000, 502_364] {
            let path = socket_path_for(DARWIN_TMPDIR, uid, SOCKET_NAME_BYTES);
            let len = path.as_os_str().len();
            assert!(
                len <= DARWIN_MAX,
                "uid {uid}: socket path is {len} bytes, over Darwin's {DARWIN_MAX}: {path:?}"
            );

            // And confirm the old size really was the problem, so nobody
            // "tidies" the name back up to 16 bytes.
            let old = socket_path_for(DARWIN_TMPDIR, uid, 16);
            assert!(
                old.as_os_str().len() > DARWIN_MAX,
                "uid {uid}: 16 bytes should not fit; this test is no longer meaningful"
            );
        }
    }

    #[test]
    fn the_server_key_is_stable_and_fits_the_socket_name() {
        let key = server_key("/tmp/tmux-1000/default");
        // Pinned, so a change to the hash that would strand every pane using
        // the old path shows up as a test failure rather than in the field.
        assert_eq!(key, server_key("/tmp/tmux-1000/default"));
        assert_eq!(key.len(), SOCKET_NAME_BYTES * 2);
        assert_ne!(key, server_key("/tmp/tmux-1000/other"));
    }

    #[test]
    fn fnv1a_matches_the_reference_vectors() {
        // From the FNV reference test suite: "" and "a" under 64-bit FNV-1a.
        assert_eq!(server_key(""), "cbf29ce484222325");
        assert_eq!(server_key("a"), "af63dc4c8601ec8c");
    }

    #[test]
    fn an_over_long_socket_path_is_refused_with_advice() {
        let path = PathBuf::from("/x".repeat(MAX_SOCKET_PATH_LEN));
        let err = check_socket_path_fits(&path).expect_err("must be refused");
        let msg = err.to_string();
        assert!(msg.contains("XDG_RUNTIME_DIR"), "unhelpful message: {msg}");
    }

    #[test]
    fn a_path_within_the_limit_is_accepted() {
        assert!(check_socket_path_fits(&socket_path_for("/tmp/", 1000, SOCKET_NAME_BYTES)).is_ok());
    }
}
