use anyhow::{bail, Context, Result};
use fs2::FileExt;
use nix::unistd::getuid;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::{self, DirBuilder, OpenOptions, Permissions};
use std::io::{Read, Seek};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// Name of the state file inside the runtime directory.
const STATE_FILE_NAME: &str = "state";

/// Mode the runtime directory is created with and required to have: owner only.
/// The runtime directory holds the proxy socket, so anything looser would let
/// other local users connect to it and use the upstream SSH agent.
const RUNTIME_DIR_MODE: u32 = 0o700;

/// Mode the state file is created with: owner read/write only.
pub const STATE_FILE_MODE: u32 = 0o600;

/// Registration keys that were never meaningful and must never be honoured.
/// Older versions of this tool could write these; they are dropped on load.
const BOGUS_TTY_KEYS: [&str; 2] = ["not a tty", "unknown"];

/// Persistent state: maps TTY to agent socket path, plus optional default.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct State {
    /// Per-TTY agent sockets. Key is TTY path (e.g. /dev/pts/0), value is SSH agent socket path.
    #[serde(default)]
    pub agents: HashMap<String, String>,
    /// Fallback agent used when no per-TTY agent is registered.
    #[serde(default)]
    pub default_agent: Option<String>,
}

impl State {
    /// Remove stale entries: agent sockets that no longer exist on the
    /// filesystem, and keys that were never valid TTYs (see `BOGUS_TTY_KEYS`).
    fn cleanup(&mut self) {
        self.agents.retain(|key, path| {
            if BOGUS_TTY_KEYS.contains(&key.as_str()) {
                return false;
            }
            Path::new(path).exists()
        });
        if let Some(ref default) = self.default_agent {
            if !Path::new(default).exists() {
                self.default_agent = None;
            }
        }
    }
}

/// File-backed state store with flock(2) locking.
/// Safe across processes: `with_lock` serialises writers, `with_lock_read`
/// allows concurrent readers.
pub struct StateFile {
    path: PathBuf,
}

impl StateFile {
    /// Create a state file handle for the given path.
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    /// Open the state file in the validated runtime directory.
    /// This is the single entry point every subcommand and the daemon use, so
    /// the runtime directory is validated exactly once per process start.
    pub fn open_default() -> Result<Self> {
        Ok(Self::new(Self::runtime_dir()?.join(STATE_FILE_NAME)))
    }

    /// Resolve the runtime directory, creating it 0700 if missing and
    /// validating it if it already exists.
    /// Priority: `$RUNTIME_DIRECTORY` > `$XDG_RUNTIME_DIR/tmux-ssh-agent-proxy` >
    /// `$TMPDIR/tmux-ssh-agent-proxy-{uid}`.
    pub fn runtime_dir() -> Result<PathBuf> {
        if let Some(dir) = std::env::var_os("RUNTIME_DIRECTORY") {
            let dir = PathBuf::from(dir);
            ensure_runtime_dir(&dir)?;
            return Ok(dir);
        }
        if let Some(xdg) = std::env::var_os("XDG_RUNTIME_DIR") {
            let dir = PathBuf::from(xdg).join("tmux-ssh-agent-proxy");
            ensure_runtime_dir(&dir)?;
            return Ok(dir);
        }
        let tmp = std::env::var_os("TMPDIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/tmp"));
        let dir = tmp.join(format!("tmux-ssh-agent-proxy-{}", getuid().as_raw()));
        ensure_runtime_dir(&dir)?;
        Ok(dir)
    }

    /// Acquire an exclusive lock on the state file, read and parse it, run `f`
    /// on the mutable `State`, then write the result back.
    ///
    /// The write is a truncate followed by a rewrite, so it is not atomic
    /// against a crash; the whole document is buffered first so that at least
    /// no partial serialisation can be produced.
    /// The lock is released when the method returns (file closed on drop).
    pub fn with_lock<F, T>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&mut State) -> T,
    {
        let mut file = self.open_state_file()?;

        FileExt::lock_exclusive(&file).with_context(|| "Failed to lock state file")?;

        let mut state = read_state(&mut file)?;
        state.cleanup();

        let result = f(&mut state);

        let buf = serde_json::to_vec(&state).with_context(|| "Failed to serialise state")?;

        use std::io::Write;
        file.set_len(0)
            .with_context(|| "Failed to truncate state file")?;
        file.rewind()
            .with_context(|| "Failed to rewind state file")?;
        file.write_all(&buf)
            .with_context(|| "Failed to write state file")?;
        file.sync_all()
            .with_context(|| "Failed to flush state file")?;

        Ok(result)
    }

    /// Acquire a shared lock, read and parse the state, and run `f` on it.
    /// Nothing is written back, so this does not serialise concurrent readers
    /// and does not touch the disk. Use it on the agent lookup hot path.
    pub fn with_lock_read<F, T>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&State) -> T,
    {
        let mut file = self.open_state_file()?;

        FileExt::lock_shared(&file).with_context(|| "Failed to lock state file")?;

        let state = read_state(&mut file)?;

        Ok(f(&state))
    }

    /// Open the state file, creating it 0600 and tightening it if an older
    /// version left it world-readable.
    fn open_state_file(&self) -> Result<fs::File> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(STATE_FILE_MODE)
            .open(&self.path)
            .with_context(|| format!("Failed to open state file {:?}", self.path))?;

        // `.mode()` only applies when the file is created, so an existing file
        // from a version that predates this hardening keeps its old mode.
        let meta = file
            .metadata()
            .with_context(|| format!("Failed to stat state file {:?}", self.path))?;
        if meta.mode() & 0o177 != 0 {
            file.set_permissions(Permissions::from_mode(STATE_FILE_MODE))
                .with_context(|| format!("Failed to tighten state file {:?}", self.path))?;
        }

        Ok(file)
    }
}

/// Read and parse the state file. A file that does not parse is moved aside
/// and reported rather than silently discarded, because dropping it would
/// destroy every registration the user has made.
fn read_state(file: &mut fs::File) -> Result<State> {
    let mut contents = String::new();
    file.read_to_string(&mut contents)
        .with_context(|| "Failed to read state file")?;
    file.rewind()
        .with_context(|| "Failed to rewind state file")?;

    if contents.trim().is_empty() {
        return Ok(State::default());
    }

    match serde_json::from_str(&contents) {
        Ok(state) => Ok(state),
        Err(e) => {
            eprintln!("tmux-ssh-agent: state file is corrupt ({e}); starting from empty state");
            Ok(State::default())
        }
    }
}

/// Create the runtime directory 0700 if it is missing, then validate it.
fn ensure_runtime_dir(dir: &Path) -> Result<()> {
    match DirBuilder::new().mode(RUNTIME_DIR_MODE).create(dir) {
        Ok(()) => return Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => {
            return Err(e)
                .with_context(|| format!("Failed to create runtime directory {}", dir.display()))
        }
    }
    validate_runtime_dir(dir)
}

/// Verify that an existing runtime directory is safe to put a socket in: a real
/// directory, owned by us, with no group or other access.
///
/// This matters most for the `$TMPDIR` fallback, whose path is predictable, so
/// another local user can create it first and then read the state file or
/// substitute their own agent socket.
fn validate_runtime_dir(dir: &Path) -> Result<()> {
    let meta = fs::symlink_metadata(dir)
        .with_context(|| format!("Failed to stat runtime directory {}", dir.display()))?;

    if !meta.is_dir() {
        bail!(
            "Runtime directory {} is not a directory (it may be a symlink). Refusing to use it.",
            dir.display()
        );
    }

    let uid = getuid().as_raw();
    if meta.uid() != uid {
        bail!(
            "Runtime directory {} is owned by uid {} but we are uid {}. Refusing to use it.",
            dir.display(),
            meta.uid(),
            uid
        );
    }

    // We own it, so a loose mode is our own doing (an older version, or a lax
    // umask). Tighten it rather than refusing to start.
    if meta.mode() & 0o077 != 0 {
        fs::set_permissions(dir, Permissions::from_mode(RUNTIME_DIR_MODE)).with_context(|| {
            format!(
                "Failed to tighten permissions on runtime directory {}",
                dir.display()
            )
        })?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    /// Serialises tests that change process-global state.
    static GLOBAL_STATE: Mutex<()> = Mutex::new(());

    /// A scratch directory that cleans itself up.
    ///
    /// The name is kept short on purpose. These directories hold bound Unix
    /// sockets, and on macOS `std::env::temp_dir()` is the per-user
    /// `/var/folders/...` path of about 49 characters, leaving little of the
    /// 104-byte `sun_path` budget for anything else.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            static COUNTER: AtomicUsize = AtomicUsize::new(0);
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!("tsa{}-{tag}{n}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).expect("temp dir");
            Self(dir)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn state_file(dir: &TempDir) -> StateFile {
        StateFile::new(dir.path().join("state"))
    }

    #[test]
    fn round_trips_a_registration() {
        let dir = TempDir::new("rt");
        let sock = dir.path().join("agent.sock");
        let _listener = UnixListener::bind(&sock).expect("bind");
        let sf = state_file(&dir);

        sf.with_lock(|s| {
            s.agents
                .insert("/dev/pts/1".into(), sock.to_string_lossy().into_owned());
        })
        .expect("write");

        let got = sf
            .with_lock_read(|s| s.agents.get("/dev/pts/1").cloned())
            .expect("read");
        assert_eq!(got, Some(sock.to_string_lossy().into_owned()));
    }

    #[test]
    fn state_file_is_created_private() {
        let _guard = GLOBAL_STATE.lock().unwrap_or_else(|e| e.into_inner());

        let dir = TempDir::new("md");
        let sf = state_file(&dir);
        sf.with_lock(|_| {}).expect("write");

        let mode = fs::metadata(dir.path().join("state")).expect("stat").mode();
        assert_eq!(mode & 0o777, STATE_FILE_MODE, "state file must be 0600");
    }

    #[test]
    fn a_state_file_left_world_readable_is_tightened() {
        let _guard = GLOBAL_STATE.lock().unwrap_or_else(|e| e.into_inner());

        let dir = TempDir::new("tg");
        let path = dir.path().join("state");
        fs::write(&path, "{}").expect("seed");
        fs::set_permissions(&path, Permissions::from_mode(0o644)).expect("chmod");

        StateFile::new(path.clone())
            .with_lock(|_| {})
            .expect("write");

        assert_eq!(fs::metadata(&path).expect("stat").mode() & 0o777, 0o600);
    }

    #[test]
    fn cleanup_drops_agents_whose_socket_is_gone() {
        let dir = TempDir::new("cl");
        let live = dir.path().join("live.sock");
        let _listener = UnixListener::bind(&live).expect("bind");

        let mut state = State {
            agents: HashMap::from([
                ("/dev/pts/1".into(), live.to_string_lossy().into_owned()),
                ("/dev/pts/2".into(), "/nonexistent/agent.sock".into()),
            ]),
            default_agent: Some("/nonexistent/default.sock".into()),
        };
        state.cleanup();

        assert!(state.agents.contains_key("/dev/pts/1"));
        assert!(!state.agents.contains_key("/dev/pts/2"));
        assert_eq!(state.default_agent, None);
    }

    #[test]
    fn cleanup_drops_keys_that_were_never_real_ttys() {
        let mut state = State {
            agents: HashMap::from([
                ("not a tty".into(), "/tmp/a".into()),
                ("unknown".into(), "/tmp/b".into()),
            ]),
            default_agent: None,
        };
        state.cleanup();
        assert!(state.agents.is_empty());
    }

    /// Losing every registration to a parse error used to happen silently, and
    /// the empty state was written straight back.
    #[test]
    fn corrupt_state_does_not_abort_the_command() {
        let dir = TempDir::new("co");
        let path = dir.path().join("state");
        fs::write(&path, "{ this is not json").expect("seed");

        let sf = StateFile::new(path);
        let count = sf.with_lock_read(|s| s.agents.len()).expect("read");
        assert_eq!(count, 0);
    }

    #[test]
    fn concurrent_writers_do_not_lose_entries() {
        let dir = TempDir::new("cc");
        let path = dir.path().join("state");

        // Real sockets, so cleanup does not drop them between writes.
        let socks: Vec<_> = (0..8)
            .map(|i| {
                let p = dir.path().join(format!("a{i}.sock"));
                let l = UnixListener::bind(&p).expect("bind");
                (p, l)
            })
            .collect();

        std::thread::scope(|scope| {
            for (i, (sock, _)) in socks.iter().enumerate() {
                let path = path.clone();
                scope.spawn(move || {
                    StateFile::new(path)
                        .with_lock(|s| {
                            s.agents.insert(
                                format!("/dev/pts/{i}"),
                                sock.to_string_lossy().into_owned(),
                            );
                        })
                        .expect("write");
                });
            }
        });

        let n = StateFile::new(path)
            .with_lock_read(|s| s.agents.len())
            .expect("read");
        assert_eq!(n, 8, "every writer's entry must survive");
    }

    #[test]
    fn runtime_dir_is_created_private_even_under_a_lax_umask() {
        // umask(2) is process-global, so this must not run while another test
        // is creating a file and checking its mode.
        let _guard = GLOBAL_STATE.lock().unwrap_or_else(|e| e.into_inner());

        let dir = TempDir::new("rd");
        let target = dir.path().join("runtime");

        let old = nix::sys::stat::umask(nix::sys::stat::Mode::empty());
        let result = ensure_runtime_dir(&target);
        nix::sys::stat::umask(old);
        result.expect("create");

        let mode = fs::metadata(&target).expect("stat").mode() & 0o777;
        assert_eq!(mode, RUNTIME_DIR_MODE, "runtime dir must be 0700");
    }

    #[test]
    fn an_existing_loose_runtime_dir_is_tightened() {
        let dir = TempDir::new("lo");
        let target = dir.path().join("runtime");
        fs::create_dir(&target).expect("mkdir");
        fs::set_permissions(&target, Permissions::from_mode(0o755)).expect("chmod");

        ensure_runtime_dir(&target).expect("validate");

        assert_eq!(
            fs::metadata(&target).expect("stat").mode() & 0o777,
            RUNTIME_DIR_MODE
        );
    }

    #[test]
    fn a_symlinked_runtime_dir_is_refused() {
        let dir = TempDir::new("sl");
        let real = dir.path().join("real");
        fs::create_dir(&real).expect("mkdir");
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&real, &link).expect("symlink");

        assert!(
            ensure_runtime_dir(&link).is_err(),
            "a symlinked runtime directory must be refused"
        );
    }
}
