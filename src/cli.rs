use crate::proxy::ProxyAgent;
use crate::state::StateFile;
use anyhow::{Context, Result};
use nix::unistd::{dup2, fork, pipe, setsid, ForkResult};
use std::os::unix::io::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd};

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
    let write_fd_raw = write_end.as_raw_fd();
    let read_fd_raw = read_end.as_raw_fd();

    match unsafe { fork().context("first fork failed")? } {
        // Original parent: wait for ready signal, then exit.
        ForkResult::Parent { .. } => {
            drop(write_end);
            let _ = read_end.into_raw_fd();
            let mut buf = [0u8; 2];
            let n = nix::unistd::read(read_fd_raw, &mut buf).context("read ready signal")?;
            nix::unistd::close(read_fd_raw)?;
            if n == 2 && &buf == b"ok" {
                std::process::exit(0);
            }
            std::process::exit(1);
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
                    drop(write_end);
                    redirect_stdio().context("redirect stdio failed")?;
                    let write_fd = unsafe { OwnedFd::from_raw_fd(write_fd_raw) };

                    let agent = ProxyAgent::new()?;
                    agent.run(Some(Box::new(move || {
                        let _ = nix::unistd::write(&write_fd, b"ok");
                    })))?;

                    std::process::exit(0);
                }
            }
        }
    }
}

/// Redirect stdin/stdout/stderr to /dev/null.
/// Required for daemon processes to detach from the controlling terminal.
fn redirect_stdio() -> Result<()> {
    let devnull = std::fs::File::open("/dev/null").context("open /dev/null for stdin")?;
    let devnull_out =
        std::fs::File::create("/dev/null").context("open /dev/null for stdout/stderr")?;

    dup2(devnull.as_raw_fd(), 0).context("dup2 stdin")?;
    dup2(devnull_out.as_raw_fd(), 1).context("dup2 stdout")?;
    dup2(devnull_out.as_raw_fd(), 2).context("dup2 stderr")?;

    Ok(())
}

/// Handle the `install` subcommand.
/// Adds `run 'tmux-ssh-agent proxy'` to `~/.tmux.conf` if not already present.
/// Backs up the original file before modifying.
/// With `--shell`, also adds the login registration snippet to `~/.bashrc`.
fn cmd_install(args: &[String]) -> Result<()> {
    let add_shell = args.contains(&"--shell".to_string());

    install_tmux_config()?;
    if add_shell {
        install_shell_profile()?;
    }

    println!("Install complete. Reload tmux: tmux source-file ~/.tmux.conf");
    if !add_shell {
        println!("To auto-register agents on login, run with --shell or add the snippet manually:");
        println!("  if [ -z \"$TMUX\" ] && [ -n \"$SSH_AUTH_SOCK\" ]; then");
        println!("    tmux-ssh-agent register-agent");
        println!("  fi");
    }
    Ok(())
}

/// Add the `run 'tmux-ssh-agent proxy'` line to `~/.tmux.conf`.
/// Creates the file if it doesn't exist, backs it up otherwise.
fn install_tmux_config() -> Result<()> {
    use std::io::{Read, Write};

    let home = std::env::var("HOME").context("HOME not set")?;
    let path = std::path::PathBuf::from(home).join(".tmux.conf");
    let line = "run 'tmux-ssh-agent proxy'";

    let mut contents = String::new();
    let exists = path.exists();

    if exists {
        let mut file = std::fs::File::open(&path)?;
        file.read_to_string(&mut contents)?;

        if contents.lines().any(|l| l.trim() == line) {
            println!("Already present in {}: {}", path.display(), line);
            return Ok(());
        }

        // Back up
        let backup = path.with_extension("tmux.conf.bak");
        std::fs::copy(&path, &backup)
            .with_context(|| format!("Failed to back up {} to {:?}", path.display(), backup))?;
        println!("Backed up {} -> {:?}", path.display(), backup);
    }

    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;

    if !exists {
        writeln!(file, "{}", line)?;
    } else {
        writeln!(file)?;
        writeln!(file, "# Added by tmux-ssh-agent install")?;
        writeln!(file, "{}", line)?;
    }

    println!("Added to {}: {}", path.display(), line);
    Ok(())
}

/// Add the login registration snippet to `~/.bashrc`.
/// Detects which shell config file exists (~/.bashrc > ~/.zshrc > ~/.config/fish/config.fish).
fn install_shell_profile() -> Result<()> {
    use std::io::{Read, Write};

    let home = std::env::var("HOME").context("HOME not set")?;

    let candidates = [".bashrc", ".zshrc", ".config/fish/config.fish"];

    let mut profile = None;
    for cand in &candidates {
        let p = std::path::PathBuf::from(&home).join(cand);
        if p.exists() {
            profile = Some(p);
            break;
        }
    }

    let path = match profile {
        Some(p) => p,
        None => {
            // Default to .bashrc
            let p = std::path::PathBuf::from(&home).join(".bashrc");
            std::fs::File::create(&p)?;
            p
        }
    };

    let snippet_start = "# Added by tmux-ssh-agent install";
    let snippet = format!(
        "{}\nif [ -z \"$TMUX\" ] && [ -n \"$SSH_AUTH_SOCK\" ]; then\n  tmux-ssh-agent register-agent\nfi\n",
        snippet_start
    );

    let mut contents = String::new();
    if path.exists() {
        let mut file = std::fs::File::open(&path)?;
        file.read_to_string(&mut contents)?;

        if contents.contains(snippet_start) {
            println!(
                "Already present in {}: registration snippet",
                path.display()
            );
            return Ok(());
        }

        // Back up
        let backup = path.with_extension("bak");
        std::fs::copy(&path, &backup)
            .with_context(|| format!("Failed to back up {} to {:?}", path.display(), backup))?;
        println!("Backed up {} -> {:?}", path.display(), backup);
    }

    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;

    writeln!(file)?;
    write!(file, "{}", snippet)?;

    println!("Added registration snippet to {}", path.display());
    Ok(())
}

/// Handle the `uninstall` subcommand.
/// Removes `run 'tmux-ssh-agent proxy'` from `~/.tmux.conf`.
/// Backs up the original file before modifying.
/// With `--shell`, also removes the login registration snippet from shell profile.
fn cmd_uninstall(args: &[String]) -> Result<()> {
    let remove_shell = args.contains(&"--shell".to_string());

    uninstall_tmux_config()?;
    if remove_shell {
        uninstall_shell_profile()?;
    }

    println!("Uninstall complete. Reload tmux: tmux source-file ~/.tmux.conf");
    Ok(())
}

/// Remove the `run 'tmux-ssh-agent proxy'` line (and its preceding comment) from `~/.tmux.conf`.
fn uninstall_tmux_config() -> Result<()> {
    use std::io::{Read, Write};

    let home = std::env::var("HOME").context("HOME not set")?;
    let path = std::path::PathBuf::from(home).join(".tmux.conf");
    let run_line = "run 'tmux-ssh-agent proxy'";
    let marker = "# Added by tmux-ssh-agent install";

    if !path.exists() {
        println!("Not found: {}", path.display());
        return Ok(());
    }

    let mut contents = String::new();
    std::fs::File::open(&path)?.read_to_string(&mut contents)?;

    let has_run = contents.lines().any(|l| l.trim() == run_line);
    if !has_run {
        println!("Not found in {}: {}", path.display(), run_line);
        return Ok(());
    }

    // Back up
    let backup = path.with_extension("tmux.conf.bak");
    std::fs::copy(&path, &backup)
        .with_context(|| format!("Failed to back up {} to {:?}", path.display(), backup))?;
    println!("Backed up {} -> {:?}", path.display(), backup);

    // Filter out lines: remove run_line, marker, and adjacent blank lines around the block.
    let mut result: Vec<&str> = Vec::new();
    let lines: Vec<&str> = contents.lines().collect();
    let mut i = 0;

    while i < lines.len() {
        let trimmed = lines[i].trim();

        if trimmed == run_line {
            // Remove a preceding blank line if it exists
            if let Some(&prev) = result.last() {
                if prev.trim().is_empty() {
                    result.pop();
                }
            }
            i += 1;
            continue;
        }

        if trimmed == marker {
            // Remove a preceding blank line if it exists
            if let Some(&prev) = result.last() {
                if prev.trim().is_empty() {
                    result.pop();
                }
            }
            // Skip marker line and the next line (run line)
            i += 2;
            continue;
        }

        result.push(lines[i]);
        i += 1;
    }

    let mut file = std::fs::File::create(&path)?;
    for line in &result {
        writeln!(file, "{}", line)?;
    }

    println!("Removed from {}: {}", path.display(), run_line);
    Ok(())
}

/// Remove the login registration snippet from shell profile.
fn uninstall_shell_profile() -> Result<()> {
    use std::io::{Read, Write};

    let home = std::env::var("HOME").context("HOME not set")?;
    let candidates = [".bashrc", ".zshrc", ".config/fish/config.fish"];

    let mut removed = false;
    for cand in &candidates {
        let path = std::path::PathBuf::from(&home).join(cand);
        if !path.exists() {
            continue;
        }

        let mut contents = String::new();
        std::fs::File::open(&path)?.read_to_string(&mut contents)?;

        let marker = "# Added by tmux-ssh-agent install";
        if !contents.contains(marker) {
            continue;
        }

        // Back up
        let backup = path.with_extension("bak");
        std::fs::copy(&path, &backup)
            .with_context(|| format!("Failed to back up {} to {:?}", path.display(), backup))?;
        println!("Backed up {} -> {:?}", path.display(), backup);

        // Remove lines from marker to the closing 'fi'
        let mut result: Vec<&str> = Vec::new();
        let lines: Vec<&str> = contents.lines().collect();
        let mut i = 0;

        while i < lines.len() {
            if lines[i].trim() == marker {
                // Skip lines until we hit 'fi'
                i += 1;
                while i < lines.len() && lines[i].trim() != "fi" {
                    i += 1;
                }
                i += 1; // skip 'fi'
                        // Remove a preceding blank line if it exists
                if let Some(&prev) = result.last() {
                    if prev.trim().is_empty() {
                        result.pop();
                    }
                }
                continue;
            }
            result.push(lines[i]);
            i += 1;
        }

        let mut file = std::fs::File::create(&path)?;
        for line in &result {
            writeln!(file, "{}", line)?;
        }

        println!("Removed registration snippet from {}", path.display());
        removed = true;
    }

    if !removed {
        println!("No registration snippet found in shell profile");
    }

    Ok(())
}

/// Handle the `register-agent` subcommand.
/// Registers the given agent socket (or `$SSH_AUTH_SOCK`) for the current TTY.
/// Args: optional explicit socket path; env var `SSH_AUTH_SOCK` is used as fallback.
fn cmd_register_agent(args: &[String]) -> Result<()> {
    let tty = get_tty();
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
            let marker = if *client == tty { "*" } else { " " };
            println!("{} {}: {}", marker, client, agent);
        }
    })
}

/// Handle the `remove-agent` subcommand.
/// Removes the registration for the given TTY, or current TTY if none specified.
fn cmd_remove_agent(args: &[String]) -> Result<()> {
    let tty = args.first().cloned().unwrap_or_else(get_tty);
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

/// Get the current TTY path.
/// Checks `$SSH_TTY` first; if empty or unset, runs the `tty` command.
/// Returns "unknown" if no TTY can be determined.
/// Filters "not a tty" / empty from the `tty` command — these are never
/// meaningful registration keys and pollute the agent listing.
fn get_tty() -> String {
    if let Ok(val) = std::env::var("SSH_TTY") {
        if !val.is_empty() {
            return val;
        }
    }
    std::process::Command::new("tty")
        .output()
        .ok()
        .and_then(|o| {
            let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
            if s.is_empty() || s == "not a tty" {
                None
            } else {
                Some(s)
            }
        })
        .unwrap_or_else(|| "unknown".to_string())
}
