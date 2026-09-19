use crate::proxy::ProxyAgent;
use crate::state::StateFile;
use anyhow::{Context, Result};
use nix::unistd::{fork, pipe, setsid, dup2, ForkResult};
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
        "list" | "list-agents" => cmd_list_agents(),
        _ => {
            print_usage();
            std::process::exit(1);
        }
    }
}

/// Print usage information to stderr.
fn print_usage() {
    let name = std::env::args().next().unwrap_or_else(|| "tmux-ssh-agent".to_string());
    eprintln!("Usage:");
    eprintln!("  {name} proxy");
    eprintln!("  {name} proxy --no-daemon");
    eprintln!("  {name} register-agent [SSH_AUTH_SOCK]");
    eprintln!("  {name} register-default-agent [--override] [SSH_AUTH_SOCK]");
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
        let agent = ProxyAgent::new();
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
            let n = nix::unistd::read(read_fd_raw, &mut buf)
                .context("read ready signal")?;
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

                    let agent = ProxyAgent::new();
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
    let devnull = std::fs::File::open("/dev/null")
        .context("open /dev/null for stdin")?;
    let devnull_out = std::fs::File::create("/dev/null")
        .context("open /dev/null for stdout/stderr")?;

    dup2(devnull.as_raw_fd(), 0)
        .context("dup2 stdin")?;
    dup2(devnull_out.as_raw_fd(), 1)
        .context("dup2 stdout")?;
    dup2(devnull_out.as_raw_fd(), 2)
        .context("dup2 stderr")?;

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

    let runtime_dir = StateFile::runtime_dir();
    let state_file = StateFile::new(runtime_dir.join("state"));
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
        .filter(|a| *a != "--override")
        .next()
        .cloned()
        .or_else(|| std::env::var("SSH_AUTH_SOCK").ok())
        .ok_or_else(|| anyhow::anyhow!("No SSH_AUTH_SOCK specified"))?;

    let runtime_dir = StateFile::runtime_dir();
    let state_file = StateFile::new(runtime_dir.join("state"));
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
    let runtime_dir = StateFile::runtime_dir();
    let state_file = StateFile::new(runtime_dir.join("state"));
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

/// Get the current TTY path.
/// Prefers `$SSH_TTY` if set, otherwise runs the `tty` command.
/// Returns "unknown" if neither yields a result.
fn get_tty() -> String {
    std::env::var("SSH_TTY").unwrap_or_else(|_| {
        std::process::Command::new("tty")
            .output()
            .ok()
            .and_then(|o| {
                let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
                if s.is_empty() { None } else { Some(s) }
            })
            .unwrap_or_else(|| "unknown".to_string())
    })
}
