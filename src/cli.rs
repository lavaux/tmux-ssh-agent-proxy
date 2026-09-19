use crate::config_edit::{self, Removal, Shell};
use crate::proxy::ProxyAgent;
use crate::state::StateFile;
use anyhow::{Context, Result};
use nix::errno::Errno;
use nix::fcntl::{fcntl, FcntlArg};
use nix::poll::{poll, PollFd, PollFlags, PollTimeout};
use nix::unistd::{dup2, fork, pipe, setsid, ForkResult};
use std::os::unix::io::{AsRawFd, FromRawFd, OwnedFd};
use std::path::{Path, PathBuf};

/// Parse CLI arguments and dispatch to the matching subcommand handler.
/// Exits with code 1 on unknown subcommand or missing args.
pub fn run() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        print_usage();
        std::process::exit(1);
    }

    match args[1].as_str() {
        "proxy" => cmd_proxy(&args[2..]),
        "register-agent" => cmd_register_agent(&args[2..]),
        "register-default-agent" => cmd_register_default_agent(&args[2..]),
        "install" => cmd_install(&args[2..]),
        "uninstall" => cmd_uninstall(&args[2..]),
        "list" | "list-agents" => cmd_list_agents(),
        "remove-agent" => cmd_remove_agent(&args[2..]),
        "clear-agents" => cmd_clear_agents(),
        _ => {
            print_usage();
            std::process::exit(1);
        }
    }
}

/// Print usage information to stderr.
fn print_usage() {
    let name = std::env::args()
        .next()
        .unwrap_or_else(|| "tmux-ssh-agent".to_string());
    eprintln!("Usage:");
    eprintln!("  {name} proxy");
    eprintln!("  {name} proxy --no-daemon");
    eprintln!("  {name} install [--shell]");
    eprintln!("  {name} uninstall [--shell]");
    eprintln!("  {name} register-agent [SSH_AUTH_SOCK]");
    eprintln!("  {name} register-default-agent [--override] [SSH_AUTH_SOCK]");
    eprintln!("  {name} remove-agent [TTY]");
    eprintln!("  {name} clear-agents");
    eprintln!("  {name} list-agents");
}

/// Handle the `proxy` subcommand.
/// Requires `TMUX` environment variable to be set (verifies we're inside tmux).
/// Without `--no-daemon`, forks and daemonizes; with it, runs in the foreground.
fn cmd_proxy(args: &[String]) -> Result<()> {
    if std::env::var("TMUX").is_err() {
        anyhow::bail!("Not running under tmux server");
    }

    if args.contains(&"--no-daemon".to_string()) {
        let agent = ProxyAgent::new()?;
        agent.run(None)
    } else {
        start_daemonized_proxy()
    }
}

/// Fork into a background daemon process using double-fork + setsid.
/// Uses a pipe for the ready signal: the grandchild (actual daemon) writes "ok"
/// once the socket is created and tmux env is set; the original parent reads
/// this and exits with code 0 on success, 1 on failure.
fn start_daemonized_proxy() -> Result<()> {
    let (read_end, write_end) = pipe().context("pipe failed")?;

    match unsafe { fork().context("first fork failed")? } {
        // Original parent: wait for ready signal, then exit.
        ForkResult::Parent { .. } => {
            drop(write_end);
            let ok = read_ready_signal(read_end.as_raw_fd());
            drop(read_end);
            std::process::exit(if ok { 0 } else { 1 });
        }
        // First child: create new session, then double-fork to fully detach.
        ForkResult::Child => {
            drop(read_end);

            setsid().context("setsid failed")?;

            match unsafe { fork().context("second fork failed")? } {
                // Intermediate process (parent of second fork): exits immediately.
                ForkResult::Parent { .. } => {
                    drop(write_end);
                    std::process::exit(0);
                }
                // Grandchild = actual daemon: redirect stdio, run proxy, signal ready.
                ForkResult::Child => {
                    // `write_end` stays owned across redirect_stdio. Closing it
                    // first would free its number for one of the /dev/null
                    // opens below to take, and the ready signal would then be
                    // written to /dev/null (or to a descriptor we do not own).
                    // Move it above fd 2 so the dup2 calls cannot clobber it in
                    // the rare case that stdio was already closed.
                    let write_end = move_fd_above_stdio(write_end)?;
                    redirect_stdio().context("redirect stdio failed")?;

                    let agent = ProxyAgent::new()?;
                    agent.run(Some(Box::new(move || {
                        let _ = nix::unistd::write(&write_end, b"ok");
                    })))?;

                    std::process::exit(0);
                }
            }
        }
    }
}

/// Read the two-byte ready signal, tolerating short reads and `EINTR`.
/// Returns false on EOF (the daemon died before signalling), on error, or if
/// the daemon does not answer within `READY_TIMEOUT_MS`.
fn read_ready_signal(fd: std::os::unix::io::RawFd) -> bool {
    const READY_TIMEOUT_MS: u16 = 10_000;

    let mut buf = [0u8; 2];
    let mut got = 0;

    while got < buf.len() {
        // Bound the wait so a daemon that wedges during startup cannot hang
        // the shell (or tmux) that invoked us.
        let borrowed = unsafe { std::os::unix::io::BorrowedFd::borrow_raw(fd) };
        let mut fds = [PollFd::new(borrowed, PollFlags::POLLIN)];
        match poll(&mut fds, PollTimeout::from(READY_TIMEOUT_MS)) {
            Ok(0) => return false,
            Ok(_) => {}
            Err(Errno::EINTR) => continue,
            Err(_) => return false,
        }

        match nix::unistd::read(fd, &mut buf[got..]) {
            Ok(0) => return false,
            Ok(n) => got += n,
            Err(Errno::EINTR) => continue,
            Err(_) => return false,
        }
    }

    &buf == b"ok"
}

/// Move `fd` to a number above stderr so that redirecting stdio cannot
/// overwrite it. Returns the original if it is already above fd 2.
fn move_fd_above_stdio(fd: OwnedFd) -> Result<OwnedFd> {
    if fd.as_raw_fd() > 2 {
        return Ok(fd);
    }
    let moved = fcntl(fd.as_raw_fd(), FcntlArg::F_DUPFD_CLOEXEC(3)).context("F_DUPFD_CLOEXEC")?;
    Ok(unsafe { OwnedFd::from_raw_fd(moved) })
}

/// Redirect stdin/stdout/stderr to /dev/null.
/// Required for daemon processes to detach from the controlling terminal.
fn redirect_stdio() -> Result<()> {
    let devnull = std::fs::OpenOptions::new()
        .read(true)
        .open("/dev/null")
        .context("open /dev/null for stdin")?;
    let devnull_out = std::fs::OpenOptions::new()
        .write(true)
        .open("/dev/null")
        .context("open /dev/null for stdout/stderr")?;

    dup2(devnull.as_raw_fd(), 0).context("dup2 stdin")?;
    dup2(devnull_out.as_raw_fd(), 1).context("dup2 stdout")?;
    dup2(devnull_out.as_raw_fd(), 2).context("dup2 stderr")?;

    Ok(())
}

/// Shell profiles we know about, in the order we fall back through them.
const PROFILE_CANDIDATES: [(&str, Shell); 3] = [
    (".bashrc", Shell::Posix),
    (".zshrc", Shell::Posix),
    (".config/fish/config.fish", Shell::Fish),
];

/// The user's home directory.
fn home() -> Result<PathBuf> {
    Ok(PathBuf::from(
        std::env::var("HOME").context("HOME not set")?,
    ))
}

/// Pick the shell profile to edit, and the syntax it needs.
///
/// `$SHELL` decides, because picking the first file that happens to exist gives
/// a zsh user with a leftover `.bashrc` a snippet their shell never reads.
fn detect_profile() -> Result<(PathBuf, Shell)> {
    let home = home()?;

    if let Some(shell) = std::env::var_os("SHELL") {
        let name = Path::new(&shell)
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        match name.as_str() {
            "fish" => return Ok((home.join(".config/fish/config.fish"), Shell::Fish)),
            "zsh" => return Ok((home.join(".zshrc"), Shell::Posix)),
            "bash" | "sh" | "dash" | "ksh" => return Ok((home.join(".bashrc"), Shell::Posix)),
            _ => {}
        }
    }

    for (rel, shell) in PROFILE_CANDIDATES {
        let path = home.join(rel);
        if path.exists() {
            return Ok((path, shell));
        }
    }

    Ok((home.join(".bashrc"), Shell::Posix))
}

/// Handle the `install` subcommand.
/// Adds the proxy block to `~/.tmux.conf`, backing the file up first.
/// With `--shell`, also adds the login registration snippet to the profile
/// matching `$SHELL`.
fn cmd_install(args: &[String]) -> Result<()> {
    let add_shell = args.contains(&"--shell".to_string());

    apply_install(&home()?.join(".tmux.conf"), &config_edit::tmux_snippet())?;

    if add_shell {
        let (path, shell) = detect_profile()?;
        apply_install(&path, &config_edit::shell_snippet(shell))?;
    }

    println!("Install complete. Reload tmux: tmux source-file ~/.tmux.conf");
    if !add_shell {
        println!("To auto-register agents on login, run with --shell or add the snippet manually:");
        for line in config_edit::shell_snippet(detect_profile()?.1) {
            println!("  {line}");
        }
    }
    Ok(())
}

/// Add our block to `path`, backing the file up first if it has content.
fn apply_install(path: &Path, body: &[String]) -> Result<()> {
    let contents = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e).with_context(|| format!("Failed to read {}", path.display())),
    };

    let Some(updated) = config_edit::add_block(&contents, body) else {
        println!("Already present in {}", path.display());
        return Ok(());
    };

    if !contents.is_empty() {
        let backup = config_edit::backup(path)?;
        println!("Backed up {} -> {}", path.display(), backup.display());
    }

    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("Failed to create {}", dir.display()))?;
    }
    config_edit::write_atomic(path, &updated)?;

    println!("Added tmux-ssh-agent block to {}", path.display());
    Ok(())
}

/// Handle the `uninstall` subcommand.
/// Removes the proxy block from `~/.tmux.conf`, and with `--shell` the login
/// snippet from every shell profile that carries one.
fn cmd_uninstall(args: &[String]) -> Result<()> {
    let remove_shell = args.contains(&"--shell".to_string());

    apply_uninstall(&home()?.join(".tmux.conf"), &config_edit::tmux_snippet())?;

    if remove_shell {
        let home = home()?;
        let mut removed_any = false;
        // Check every candidate, not just the one install would pick now: the
        // snippet may have gone into a different file under an older version or
        // a different $SHELL.
        for (rel, shell) in PROFILE_CANDIDATES {
            let path = home.join(rel);
            if !path.exists() {
                continue;
            }
            removed_any |= apply_uninstall(&path, &config_edit::shell_snippet(shell))?;
        }
        if !removed_any {
            println!("No registration snippet found in any shell profile");
        }
    }

    println!("Uninstall complete. Reload tmux: tmux source-file ~/.tmux.conf");
    Ok(())
}

/// Remove our block from `path`. Returns whether anything was removed.
fn apply_uninstall(path: &Path, body: &[String]) -> Result<bool> {
    let contents = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            println!("Not found: {}", path.display());
            return Ok(false);
        }
        Err(e) => return Err(e).with_context(|| format!("Failed to read {}", path.display())),
    };

    match config_edit::remove_block(&contents, body) {
        Removal::NotPresent => Ok(false),
        Removal::NeedsManual => {
            println!(
                "{} has a tmux-ssh-agent block that has been edited and is not \
                 bounded by an end marker, so its extent cannot be determined. \
                 Leaving it alone; remove the lines under {:?} by hand.",
                path.display(),
                config_edit::MARKER_BEGIN
            );
            Ok(false)
        }
        Removal::Removed(updated) => {
            let backup = config_edit::backup(path)?;
            println!("Backed up {} -> {}", path.display(), backup.display());
            config_edit::write_atomic(path, &updated)?;
            println!("Removed tmux-ssh-agent block from {}", path.display());
            Ok(true)
        }
    }
}

/// Handle the `register-agent` subcommand.
/// Registers the given agent socket (or `$SSH_AUTH_SOCK`) for the current TTY.
/// Args: optional explicit socket path; env var `SSH_AUTH_SOCK` is used as fallback.
fn cmd_register_agent(args: &[String]) -> Result<()> {
    let tty = require_tty()?;
    let agent = args
        .first()
        .cloned()
        .or_else(|| std::env::var("SSH_AUTH_SOCK").ok())
        .ok_or_else(|| anyhow::anyhow!("No SSH_AUTH_SOCK specified"))?;
    let agent = crate::ssh_agent::validate_agent_socket(&agent)?;

    let state_file = StateFile::open_default()?;
    state_file.with_lock(|state| {
        state.agents.insert(tty, agent);
    })
}

/// Handle the `register-default-agent` subcommand.
/// Sets a default agent used when no per-TTY registration exists.
/// `--override` replaces an existing default; without it the first registration wins.
/// Args: optional explicit socket path; env var `SSH_AUTH_SOCK` is used as fallback.
fn cmd_register_default_agent(args: &[String]) -> Result<()> {
    let override_flag = args.contains(&"--override".to_string());
    let agent = args
        .iter()
        .find(|a| *a != "--override")
        .cloned()
        .or_else(|| std::env::var("SSH_AUTH_SOCK").ok())
        .ok_or_else(|| anyhow::anyhow!("No SSH_AUTH_SOCK specified"))?;
    let agent = crate::ssh_agent::validate_agent_socket(&agent)?;

    let state_file = StateFile::open_default()?;
    state_file.with_lock(|state| {
        if override_flag || state.default_agent.is_none() {
            state.default_agent = Some(agent);
        }
    })
}

/// Handle the `list-agents` / `list` subcommand.
/// Prints the default agent (if set) and all per-TTY registrations.
/// The current TTY is marked with `*`.
fn cmd_list_agents() -> Result<()> {
    let tty = get_tty();

    let state_file = StateFile::open_default()?;
    state_file.with_lock(|state| {
        if let Some(ref default) = state.default_agent {
            println!("  default: {}", default);
        }
        let mut agents: Vec<_> = state.agents.iter().collect();
        agents.sort_by(|a, b| a.0.cmp(b.0));
        for (client, agent) in agents {
            let marker = if Some(client.as_str()) == tty.as_deref() {
                "*"
            } else {
                " "
            };
            println!("{} {}: {}", marker, client, agent);
        }
    })
}

/// Handle the `remove-agent` subcommand.
/// Removes the registration for the given TTY, or current TTY if none specified.
fn cmd_remove_agent(args: &[String]) -> Result<()> {
    let tty = match args.first() {
        Some(t) => t.clone(),
        None => require_tty()?,
    };
    let state_file = StateFile::open_default()?;
    let removed = state_file.with_lock(|state| state.agents.remove(&tty))?;
    if removed.is_some() {
        println!("Removed agent for {}", tty);
    } else {
        println!("No agent registered for {}", tty);
    }
    Ok(())
}

/// Handle the `clear-agents` subcommand.
/// Removes all per-TTY agent registrations and the default agent.
fn cmd_clear_agents() -> Result<()> {
    let state_file = StateFile::open_default()?;
    state_file.with_lock(|state| {
        state.agents.clear();
        state.default_agent = None;
    })?;
    println!("Cleared all agent registrations");
    Ok(())
}

/// Get the current TTY path, if there is one.
///
/// Checks `$SSH_TTY` first, then asks the kernel via `ttyname(3)` on stdin.
/// The previous implementation shelled out to `tty` and compared its output
/// against the English string "not a tty", which does not match under any other
/// locale, and fell back to the literal key "unknown" that every TTY-less
/// session then shared.
fn get_tty() -> Option<String> {
    if let Ok(val) = std::env::var("SSH_TTY") {
        if !val.is_empty() {
            return Some(val);
        }
    }
    nix::unistd::ttyname(std::io::stdin())
        .ok()
        .map(|p| p.to_string_lossy().into_owned())
}

/// Get the current TTY path or fail with an actionable message.
fn require_tty() -> Result<String> {
    get_tty().ok_or_else(|| {
        anyhow::anyhow!(
            "No TTY for this session, so there is nothing to register an agent against. \
             Pass the TTY explicitly, or set $SSH_TTY."
        )
    })
}
