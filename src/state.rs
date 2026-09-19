use anyhow::{Context, Result};
use fs2::FileExt;
use nix::unistd::getuid;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::{Read, Seek};
use std::path::{Path, PathBuf};

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
    /// Remove entries whose agent socket no longer exists on the filesystem.
    /// Prevents stale registrations from accumulating.
    fn cleanup(&mut self) {
        self.agents.retain(|_, path| Path::new(path).exists());
        if let Some(ref default) = self.default_agent {
            if !Path::new(default).exists() {
                self.default_agent = None;
            }
        }
    }
}

/// File-backed state store with exclusive flock(2) locking.
/// Thread-safe across processes — only one writer at a time.
pub struct StateFile {
    path: PathBuf,
}

impl StateFile {
    /// Create a state file handle for the given path.
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    /// Resolve the runtime directory (created if missing).
    /// Priority: `$RUNTIME_DIRECTORY` > `$XDG_RUNTIME_DIR/tmux-ssh-agent-proxy` > `/tmp/tmux-ssh-agent-proxy-{uid}`.
    pub fn runtime_dir() -> PathBuf {
        if let Some(dir) = std::env::var_os("RUNTIME_DIRECTORY") {
            return PathBuf::from(dir);
        }
        if let Some(xdg) = std::env::var_os("XDG_RUNTIME_DIR") {
            let dir = PathBuf::from(xdg).join("tmux-ssh-agent-proxy");
            fs::create_dir_all(&dir).ok();
            return dir;
        }
        let dir = PathBuf::from("/tmp").join(format!("tmux-ssh-agent-proxy-{}", getuid().as_raw()));
        fs::create_dir_all(&dir).ok();
        dir
    }

    /// Acquire an exclusive lock on the state file, read+parse it, run `f`
    /// on the mutable `State`, then write the result back atomically.
    /// The lock is released when the method returns (file closed on drop).
    pub fn with_lock<F, T>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&mut State) -> T,
    {
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(&self.path)
            .with_context(|| format!("Failed to open state file {:?}", self.path))?;

        file.lock_exclusive()
            .with_context(|| "Failed to lock state file")?;

        let mut contents = String::new();
        file.read_to_string(&mut contents)
            .with_context(|| "Failed to read state file")?;

        let mut state: State = if contents.is_empty() {
            State::default()
        } else {
            serde_json::from_str(&contents).unwrap_or_default()
        };

        state.cleanup();

        let result = f(&mut state);

        file.set_len(0)
            .with_context(|| "Failed to truncate state file")?;
        file.rewind()
            .with_context(|| "Failed to rewind state file")?;
        serde_json::to_writer(&file, &state).with_context(|| "Failed to write state file")?;

        Ok(result)
    }
}
