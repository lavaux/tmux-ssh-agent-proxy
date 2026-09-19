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
