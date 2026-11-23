use crate::config_edit::{self, Removal, Shell};
use crate::proxy::ProxyAgent;
use crate::state::{State, StateFile};
use anyhow::{Context, Result};
use nix::errno::Errno;
use nix::fcntl::{fcntl, FcntlArg};
use nix::poll::{poll, PollFd, PollFlags, PollTimeout};
use nix::unistd::{dup2, fork, pipe, setsid, ForkResult};
use std::io::Write;
use std::os::unix::io::{AsRawFd, FromRawFd, OwnedFd};
use std::path::{Path, PathBuf};

/// Name the help text uses for the program. Fixed rather than taken from
/// `argv[0]`, which is often a full path.
const PROGRAM: &str = env!("CARGO_PKG_NAME");

/// Help for one subcommand. `COMMANDS` is the single source of all help text.
struct CommandHelp {
    name: &'static str,
    aliases: &'static [&'static str],
    /// Arguments, without the program or command name.
    synopsis: &'static str,
    /// One line for the overview.
    summary: &'static str,
    /// Full description for `help <command>` and `<command> --help`.
    details: &'static str,
}

const COMMANDS: &[CommandHelp] = &[
    CommandHelp {
        name: "proxy",
        aliases: &[],
        synopsis: "[--no-daemon]",
        summary: "Start the proxy for this tmux server (run from tmux.conf)",
        details: "\
Starts the forwarding socket for the current tmux server and publishes it as
SSH_AUTH_SOCK in the tmux global environment. It also takes SSH_AUTH_SOCK out
of tmux's update-environment option and clears it from existing sessions, so
that attaching later cannot hide the proxy socket again.

The proxy runs in the background for as long as the tmux server does. Only one
runs per tmux server: if one is already running, this publishes its socket
again and exits.

Must be run inside tmux, normally from ~/.tmux.conf:
  run 'tmux-ssh-agent proxy'

Options:
  --no-daemon   Stay in the foreground and print errors to the terminal.
                Use this for debugging: the daemon discards its output.
",
    },
    CommandHelp {
        name: "install",
        aliases: &[],
        synopsis: "[--shell]",
        summary: "Add the proxy to ~/.tmux.conf (and the login snippet with --shell)",
        details: "\
Adds a block to ~/.tmux.conf that starts the proxy, backing the file up first.
Reload tmux afterwards with:
  tmux source-file ~/.tmux.conf
Running install again changes nothing.

Options:
  --shell   Also add the agent registration snippet to your shell profile,
            chosen from $SHELL: ~/.bashrc for bash, ~/.zshrc for zsh, or
            ~/.config/fish/config.fish (in fish syntax) for fish.

Everything written sits between the lines
  # Added by tmux-ssh-agent install
  # End tmux-ssh-agent
which is exactly what uninstall removes.
",
    },
    CommandHelp {
        name: "uninstall",
        aliases: &[],
        synopsis: "[--shell]",
        summary: "Remove what install added",
        details: "\
Removes the block install added to ~/.tmux.conf, backing the file up first.
Only the lines between the install markers are removed. If the end marker was
deleted by hand, the block is left alone and you are told to remove it
yourself, rather than guessing where it ends.

Options:
  --shell   Also remove the registration snippet from every shell profile
            that has one: ~/.bashrc, ~/.zshrc, ~/.config/fish/config.fish.
",
    },
    CommandHelp {
        name: "register-agent",
        aliases: &[],
        synopsis: "[--tty TTY] [SOCKET]",
        summary: "Register an SSH agent for a terminal (run from your login shell)",
        details: "\
Records that tmux clients attached from terminal TTY should use the SSH agent
at SOCKET. Panes in a session use the agent of the client attached to it.
Prints nothing on success. If TTY already had a live agent, it is replaced
and the old one is reported on stderr.

Arguments:
  SOCKET      The agent socket. Defaults to $SSH_AUTH_SOCK. It must be an
              existing socket owned by you, and not the proxy's own socket.

Options:
  --tty TTY   The terminal to register for, as shown by list-agents, e.g.
              /dev/pts/3. Defaults to the terminal you run this from
              ($SSH_TTY, or the terminal on standard input).

Run it from your login shell, before tmux starts, typically from your
shell profile:
  if [ -z \"$TMUX\" ] && [ -n \"$SSH_AUTH_SOCK\" ]; then
    tmux-ssh-agent register-agent
  fi

Inside tmux it will not guess the terminal: a pane has its own terminal, which
is not the one the proxy routes by, so pass --tty and SOCKET explicitly. The
first line of list-agents, run in the pane, names the terminal to use.
",
    },
    CommandHelp {
        name: "switch-agent",
        aliases: &[],
        synopsis: "[--tty TTY] (SOCKET | --reset | --menu)",
        summary: "Switch the agent a tmux client uses, from inside tmux",
        details: "\
Makes the tmux client this pane belongs to use the SSH agent at SOCKET, for
every pane routed by that client. The client's login registration is kept: the
switch sits on top of it until --reset, or until register-agent runs for that
terminal again (a new login).

Arguments:
  SOCKET      The agent socket. It must be an existing socket owned by you,
              and not the proxy's own socket.

Options:
  --reset     Drop the switch, going back to the client's registration, or
              else the default agent.
  --menu      Pick the agent from a tmux menu listing every agent known from
              registrations, switches and the default. Needs tmux 3.0.
  --tty TTY   The client terminal to act on, as shown by list-agents.
              Defaults to the client of the pane this runs in. Required when
              not run from a pane, e.g. from run-shell or a key binding.

install binds the menu to prefix + A:
  bind-key A run-shell -b 'tmux-ssh-agent switch-agent --menu --tty \"#{client_tty}\"'
",
    },
    CommandHelp {
        name: "register-default-agent",
        aliases: &[],
        synopsis: "[--override] [SOCKET]",
        summary: "Set the agent used by tmux clients with no registration",
        details: "\
Sets the fallback agent, used for a tmux client whose terminal has no
registration of its own. It is never used when no tmux client can be
identified at all.

Arguments:
  SOCKET       The agent socket. Defaults to $SSH_AUTH_SOCK. It must be an
               existing socket owned by you, and not the proxy's own socket.
               Inside tmux, $SSH_AUTH_SOCK is the proxy, so pass SOCKET.

Options:
  --override   Replace an existing default. Without it, an existing default
               is kept and this does nothing.
",
    },
    CommandHelp {
        name: "remove-agent",
        aliases: &[],
        synopsis: "[--tty TTY | TTY]",
        summary: "Remove the agent registered for a terminal",
        details: "\
Removes the registration for terminal TTY, and any switch-agent switch for
it. The default agent is not touched; clear-agents removes that.

Arguments:
  TTY, --tty TTY   The terminal whose registration to remove, as shown by
                   list-agents. Defaults to the terminal you run this from.
                   Inside tmux there is no safe default, so name it.
",
    },
    CommandHelp {
        name: "clear-agents",
        aliases: &[],
        synopsis: "[--yes]",
        summary: "Remove every registration and the default agent",
        details: "\
Removes every per-terminal registration, every switch-agent switch and the
default agent. Asks for
confirmation first.

Options:
  -y, --yes   Do not ask. Required when not running interactively.
",
    },
    CommandHelp {
        name: "list-agents",
        aliases: &["list"],
        synopsis: "",
        summary: "List registrations and mark the one in use with '*'",
        details: "\
Lists the default agent, each registered terminal with its agent, and each
terminal switched to another agent with switch-agent, marked '(switched)'.
Registrations whose agent socket no longer exists are dropped as a side
effect.

Outside tmux, '*' marks the registration of the terminal you run it from.

Inside tmux, the first line names the tmux client this pane belongs to, which
is what the proxy chooses an agent by, and '*' marks the agent the proxy
would use for this pane: that client's switched agent, else its registration,
else the default.
No '*' means this pane gets no agent: register one for that client terminal
with 'register-agent --tty', or set a default.
",
    },
];

/// The help entry for a command name or alias.
fn find_command(name: &str) -> Option<&'static CommandHelp> {
    COMMANDS
        .iter()
        .find(|c| c.name == name || c.aliases.contains(&name))
}

/// Full help text for one command, or `None` if there is no such command.
fn command_help(name: &str) -> Option<String> {
    let c = find_command(name)?;
    let mut out = format!("Usage: {PROGRAM} {} {}", c.name, c.synopsis)
        .trim_end()
        .to_string();
    out.push_str("\n\n");
    out.push_str(c.summary);
    out.push_str(".\n\n");
    out.push_str(c.details);
    if !c.aliases.is_empty() {
        out.push_str(&format!("\nAlias: {}\n", c.aliases.join(", ")));
    }
    Some(out)
}

/// The overview: every command's synopsis and summary.
fn usage() -> String {
    let mut out = format!("{PROGRAM}: give each tmux client its own SSH agent.\n\nUsage:\n");
    for c in COMMANDS {
        let line = format!("{} {}", c.name, c.synopsis);
        out.push_str(&format!("  {PROGRAM} {}\n", line.trim_end()));
        out.push_str(&format!("      {}\n", c.summary));
    }
    out.push_str(&format!("  {PROGRAM} --help | --version\n"));
    out.push_str(&format!(
        "\nRun '{PROGRAM} help <command>' or '{PROGRAM} <command> --help' for details.\n"
    ));
    out
}

/// Report an unknown command with the overview, as an error.
fn unknown_command(name: &str) -> Result<()> {
    eprint!("{}", usage());
    anyhow::bail!("Unknown command {name:?}")
}

/// Parse CLI arguments and dispatch to the matching subcommand handler.
/// Exits with code 1 when no command is given; unknown commands are errors.
pub fn run() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let Some(cmd) = args.get(1) else {
        eprint!("{}", usage());
        std::process::exit(1);
    };
    let rest = &args[2..];

    match cmd.as_str() {
        "--help" | "-h" => {
            print!("{}", usage());
            return Ok(());
        }
        "help" => {
            return match rest.first() {
                None => {
                    print!("{}", usage());
                    Ok(())
                }
                Some(name) => match command_help(name) {
                    Some(text) => {
                        print!("{text}");
                        Ok(())
                    }
                    None => unknown_command(name),
                },
            };
        }
        "--version" | "-V" => {
            println!("{} {}", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
        _ => {}
    }

    let Some(command) = find_command(cmd) else {
        return unknown_command(cmd);
    };

    // Answer --help before the command parses its arguments, so it is never
    // rejected as an unknown option or, worse, ignored.
    if wants_help(rest) {
        if let Some(text) = command_help(command.name) {
            print!("{text}");
        }
        return Ok(());
    }

    match command.name {
        "proxy" => cmd_proxy(rest),
        "register-agent" => cmd_register_agent(rest),
        "register-default-agent" => cmd_register_default_agent(rest),
        "switch-agent" => cmd_switch_agent(rest),
        "install" => cmd_install(rest),
        "uninstall" => cmd_uninstall(rest),
        "list-agents" => cmd_list_agents(rest),
        "remove-agent" => cmd_remove_agent(rest),
        "clear-agents" => cmd_clear_agents(rest),
        other => unreachable!("command {other:?} has help but no handler"),
    }
}

/// Whether the arguments ask for help.
fn wants_help(args: &[String]) -> bool {
    args.iter().any(|a| a == "--help" || a == "-h")
}

/// Reject any option this subcommand does not understand.
///
/// Flags used to be matched with `args.contains(..)`, so a typo such as
/// `--shel` was accepted and silently did nothing.
fn reject_unknown_flags(args: &[String], allowed: &[&str]) -> Result<()> {
    for arg in args {
        if arg.starts_with('-') && !allowed.contains(&arg.as_str()) {
            anyhow::bail!("Unknown option {arg:?}. Run with --help for usage.");
        }
    }
    Ok(())
}

/// Take `name VALUE` or `name=VALUE` out of `args`, returning the value and
/// the remaining arguments in order. Errors if the value is missing or the
/// option is given twice.
fn take_option(args: &[String], name: &str) -> Result<(Option<String>, Vec<String>)> {
    let prefix = format!("{name}=");
    let mut value = None;
    let mut rest = Vec::new();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        let found = if arg == name {
            match iter.next() {
                Some(v) if !v.is_empty() && !v.starts_with('-') => v.clone(),
                _ => anyhow::bail!("{name} needs a value"),
            }
        } else if let Some(v) = arg.strip_prefix(&prefix) {
            if v.is_empty() {
                anyhow::bail!("{name} needs a value");
            }
            v.to_string()
        } else {
            rest.push(arg.clone());
            continue;
        };
        if value.replace(found).is_some() {
            anyhow::bail!("{name} given more than once");
        }
    }
    Ok((value, rest))
}

/// Handle the `proxy` subcommand.
/// Requires `TMUX` environment variable to be set (verifies we're inside tmux).
/// Without `--no-daemon`, forks and daemonizes; with it, runs in the foreground.
fn cmd_proxy(args: &[String]) -> Result<()> {
    reject_unknown_flags(args, &["--no-daemon"])?;

    if std::env::var("TMUX").is_err() {
        anyhow::bail!("Not running under tmux server");
    }

    // Check that tmux is actually reachable while stderr still goes somewhere
    // the user will see it. Once we daemonise, stdio is /dev/null and any
    // failure here would be completely silent.
    match crate::tmux::probe() {
        Ok(true) => {}
        Ok(false) => anyhow::bail!("tmux is installed but its server did not respond"),
        Err(e) => anyhow::bail!("{e}"),
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
        //
        // Nothing between here and the daemon's main loop may call into the
        // higher-level system libraries. Darwin aborts a forked-but-not-exec'd
        // process that touches CoreFoundation, the Objective-C runtime or
        // Directory Services, so no getpwuid, no getaddrinfo, no NSUserName.
        // The syscalls used below (setsid, fork, fcntl, open, dup2, getuid) and
        // plain file I/O are all safe; Command::new is safe because it execs.
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
    reject_unknown_flags(args, &["--shell"])?;
    let add_shell = args.contains(&"--shell".to_string());

    apply_install(
        &home()?.join(".tmux.conf"),
        &config_edit::tmux_block(),
        &config_edit::tmux_snippet(),
    )?;

    if add_shell {
        let (path, shell) = detect_profile()?;
        let body = config_edit::shell_snippet(shell);
        apply_install(&path, &body, &body)?;
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
/// Nothing is added if `legacy`, an older or hand-written form of the block,
/// is already there.
fn apply_install(path: &Path, body: &[String], legacy: &[String]) -> Result<()> {
    let contents = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e).with_context(|| format!("Failed to read {}", path.display())),
    };

    let updated = match config_edit::has_block(&contents, legacy) {
        true => None,
        false => config_edit::add_block(&contents, body),
    };
    let Some(updated) = updated else {
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
    reject_unknown_flags(args, &["--shell"])?;
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
/// Registers the given agent socket (or `$SSH_AUTH_SOCK`) for a TTY: the one
/// named by `--tty`, else the current one, which inside tmux is refused.
fn cmd_register_agent(args: &[String]) -> Result<()> {
    let (tty, rest) = take_option(args, "--tty")?;
    reject_unknown_flags(&rest, &[])?;
    if rest.len() > 1 {
        anyhow::bail!("Too many arguments. Run with --help for usage.");
    }
    let tty = target_tty(tty, "register-agent")?;
    let agent = rest
        .first()
        .cloned()
        .or_else(|| std::env::var("SSH_AUTH_SOCK").ok())
        .ok_or_else(|| anyhow::anyhow!("No SSH_AUTH_SOCK specified"))?;
    let agent = crate::ssh_agent::validate_agent_socket(&agent)?;
    refuse_proxy_socket(&agent)?;

    let state_file = StateFile::open_default()?;
    // A new registration is a new login on that terminal, so a switch made
    // under the previous one no longer applies.
    let (previous, switched) = state_file.with_lock(|state| {
        (
            state.agents.insert(tty.clone(), agent.clone()),
            state.overrides.remove(&tty),
        )
    })?;
    // Silent otherwise: the login snippet runs this on every login. Dead
    // agents were already dropped by the state cleanup, so this only fires
    // when a live registration is overwritten, which is worth knowing.
    if let Some(old) = previous.filter(|old| *old != agent) {
        eprintln!("Replaced the agent registered for {tty}: was {old}, now {agent}");
    }
    if let Some(old) = switched {
        eprintln!("Dropped the agent {tty} had been switched to: {old}");
    }
    Ok(())
}

/// Handle the `register-default-agent` subcommand.
/// Sets a default agent used when no per-TTY registration exists.
/// `--override` replaces an existing default; without it the first registration wins.
/// Args: optional explicit socket path; env var `SSH_AUTH_SOCK` is used as fallback.
fn cmd_register_default_agent(args: &[String]) -> Result<()> {
    reject_unknown_flags(args, &["--override"])?;
    let override_flag = args.contains(&"--override".to_string());
    let agent = args
        .iter()
        .find(|a| *a != "--override")
        .cloned()
        .or_else(|| std::env::var("SSH_AUTH_SOCK").ok())
        .ok_or_else(|| anyhow::anyhow!("No SSH_AUTH_SOCK specified"))?;
    let agent = crate::ssh_agent::validate_agent_socket(&agent)?;
    refuse_proxy_socket(&agent)?;

    let state_file = StateFile::open_default()?;
    state_file.with_lock(|state| {
        if override_flag || state.default_agent.is_none() {
            state.default_agent = Some(agent);
        }
    })
}

/// Refuse to register the proxy's own socket as an upstream agent.
///
/// Inside tmux `$SSH_AUTH_SOCK` names the proxy, so taking it as the default
/// would make the proxy forward to itself. Both sides are canonicalised: on
/// macOS `$TMPDIR` lives under `/var`, which resolves to `/private/var`.
fn refuse_proxy_socket(agent: &str) -> Result<()> {
    let runtime_dir = StateFile::runtime_dir()?;
    let runtime_dir = std::fs::canonicalize(&runtime_dir).unwrap_or(runtime_dir);
    if Path::new(agent).parent() == Some(runtime_dir.as_path()) {
        anyhow::bail!(
            "{agent} is the tmux-ssh-agent proxy socket, not an SSH agent. \
             Inside tmux $SSH_AUTH_SOCK points at the proxy; pass the real agent socket."
        );
    }
    Ok(())
}

/// What `switch-agent` was asked to do.
#[derive(Debug, PartialEq, Eq)]
enum SwitchAction {
    To(String),
    Reset,
    Menu,
}

/// Parse `switch-agent`'s arguments other than `--tty`.
fn parse_switch_action(args: &[String]) -> Result<SwitchAction> {
    reject_unknown_flags(args, &["--reset", "--menu"])?;
    match args {
        [] => anyhow::bail!("Name an agent SOCKET, or pass --reset or --menu"),
        [a] if a == "--reset" => Ok(SwitchAction::Reset),
        [a] if a == "--menu" => Ok(SwitchAction::Menu),
        [a] if !a.starts_with('-') => Ok(SwitchAction::To(a.clone())),
        _ => anyhow::bail!("Give exactly one of SOCKET, --reset or --menu"),
    }
}

/// Handle the `switch-agent` subcommand.
/// Sets (or with `--reset` drops) the agent a tmux client is switched to,
/// leaving its login registration alone; `--menu` lets tmux ask which one.
fn cmd_switch_agent(args: &[String]) -> Result<()> {
    let (tty, rest) = take_option(args, "--tty")?;
    let action = parse_switch_action(&rest)?;
    let tty = switch_target_tty(tty)?;
    let state_file = StateFile::open_default()?;

    match action {
        SwitchAction::To(agent) => {
            let agent = crate::ssh_agent::validate_agent_socket(&agent)?;
            refuse_proxy_socket(&agent)?;
            state_file.with_lock(|state| state.overrides.insert(tty.clone(), agent.clone()))?;
            println!("Switched {tty} to {agent}");
        }
        SwitchAction::Reset => {
            let (removed, now) = state_file.with_lock(|state| {
                let removed = state.overrides.remove(&tty);
                (removed, state.agent_for(&tty).cloned())
            })?;
            let now = match &now {
                Some(agent) => agent.as_str(),
                None => "no agent",
            };
            match removed {
                Some(_) => println!("Reset {tty}; it now uses {now}"),
                None => println!("{tty} was not switched; it uses {now}"),
            }
        }
        SwitchAction::Menu => show_switch_menu(&state_file, &tty)?,
    }
    Ok(())
}

/// The tmux client `switch-agent` acts on: `explicit` if given, else the
/// client of the pane this runs in.
///
/// Unlike `target_tty`, a pane's own client is a fine default here: a switch
/// does not touch the login registration, and switching the client you are
/// sitting at is the point of the command. Outside a pane there is still
/// nothing to go by, and the most recently active client could be someone
/// else's.
fn switch_target_tty(explicit: Option<String>) -> Result<String> {
    if let Some(tty) = explicit {
        return Ok(tty);
    }
    match lookup_client() {
        ClientLookup::Pane(client) => Ok(client),
        ClientLookup::NotInTmux => anyhow::bail!(
            "Not inside tmux, so there is no tmux client to switch. Outside tmux, \
             use {PROGRAM} register-agent, or name the client with --tty TTY"
        ),
        ClientLookup::Fallback(_) | ClientLookup::Unknown => anyhow::bail!(
            "Inside tmux, and this process is not in a pane, so the tmux client cannot \
             be identified. Pass it explicitly: {PROGRAM} switch-agent --tty TTY \
             (from a key binding: --tty '#{{client_tty}}')"
        ),
    }
}

/// One agent the menu offers.
#[derive(Debug, PartialEq, Eq)]
struct MenuChoice {
    label: String,
    socket: String,
}

/// Every distinct agent the state knows of, in menu order: registrations by
/// terminal, the default, then agents only reachable through a switch.
fn menu_choices(state: &State) -> Vec<MenuChoice> {
    let mut agents: Vec<_> = state.agents.iter().collect();
    agents.sort();
    let mut switched: Vec<_> = state.overrides.iter().collect();
    switched.sort();

    let mut out: Vec<MenuChoice> = Vec::new();
    let mut push = |label: String, socket: &String| {
        if !out.iter().any(|c| c.socket == *socket) {
            out.push(MenuChoice {
                label,
                socket: socket.clone(),
            });
        }
    };
    for (tty, socket) in agents {
        push(format!("{tty}: {socket}"), socket);
    }
    if let Some(socket) = &state.default_agent {
        push(format!("default: {socket}"), socket);
    }
    for (tty, socket) in switched {
        push(format!("{tty} (switched): {socket}"), socket);
    }
    out
}

/// Whether `s` can go into a tmux menu command as is.
///
/// Each item's command is parsed again by tmux, which also expands `#`
/// formats, and then by `sh` under run-shell. Rather than escape for both
/// layers, anything outside a conservative set is left out of the menu. Agent
/// sockets and terminals are ordinarily plain paths.
fn menu_safe(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "/._+:@-".contains(c))
}

/// display-menu's item arguments (name, key, command triples) for client
/// `tty`. `current` is the agent the client uses now, starred; a reset item
/// is offered when the client is switched. Choices that are not `menu_safe`
/// are skipped.
fn menu_items(
    exe: &str,
    tty: &str,
    choices: &[MenuChoice],
    current: Option<&str>,
    switched: bool,
) -> Vec<String> {
    let keys = "123456789";
    let mut items = Vec::new();
    for (i, choice) in choices.iter().filter(|c| menu_safe(&c.socket)).enumerate() {
        let mark = if current == Some(choice.socket.as_str()) {
            "*"
        } else {
            " "
        };
        items.push(format!("{mark} {}", choice.label));
        items.push(keys.get(i..i + 1).unwrap_or("").to_string());
        items.push(format!(
            "run-shell \"{exe} switch-agent --tty {tty} {} >/dev/null\"",
            choice.socket
        ));
    }
    if switched {
        items.push(String::new()); // separator
        items.push("Reset to the registered agent".to_string());
        items.push("r".to_string());
        items.push(format!(
            "run-shell \"{exe} switch-agent --tty {tty} --reset >/dev/null\""
        ));
    }
    items
}

/// Show the `switch-agent --menu` menu on client `tty`.
fn show_switch_menu(state_file: &StateFile, tty: &str) -> Result<()> {
    if !menu_safe(tty) {
        anyhow::bail!("Cannot put terminal {tty:?} in a tmux menu command");
    }
    // The menu re-runs this binary. Prefer its full path, which works even
    // when tmux's run-shell has a different $PATH.
    let exe = std::env::current_exe()
        .ok()
        .map(|p| p.to_string_lossy().into_owned())
        .filter(|p| menu_safe(p))
        .unwrap_or_else(|| PROGRAM.to_string());

    let (choices, current, switched) = state_file.with_lock_read(|state| {
        (
            menu_choices(state),
            state.agent_for(tty).cloned(),
            state.overrides.contains_key(tty),
        )
    })?;
    if choices.is_empty() {
        anyhow::bail!("No agents registered, so there is nothing to switch to");
    }
    for c in choices.iter().filter(|c| !menu_safe(&c.socket)) {
        eprintln!(
            "Left out of the menu, its path has unusual characters: {}",
            c.socket
        );
    }

    let items = menu_items(&exe, tty, &choices, current.as_deref(), switched);
    if items.is_empty() {
        anyhow::bail!("No agent can be offered in a menu; use {PROGRAM} switch-agent SOCKET");
    }
    crate::tmux::display_menu(tty, &format!("SSH agent for {tty}"), &items)
}

/// What `list-agents` marks with `*`.
#[derive(Debug, PartialEq, Eq)]
enum Marker {
    /// The agent this terminal was switched to (`switch-agent`).
    Switched(String),
    /// The registration of this terminal.
    Entry(String),
    /// The default agent.
    Default,
    Nothing,
}

/// Decide which line `list-agents` stars.
///
/// `client` is the terminal to look up, `None` if it is unknown. A switch
/// beats the registration, as in the proxy (`State::agent_for`). With
/// `use_default`, a client with no registration of its own stars the default,
/// which is what the proxy hands it (`proxy::get_agent_for_client`). Outside
/// tmux the CLI is not talking to the proxy, so the default means nothing
/// there and is never starred.
fn starred(state: &State, client: Option<&str>, use_default: bool) -> Marker {
    let Some(client) = client else {
        return Marker::Nothing;
    };
    if state.overrides.contains_key(client) {
        Marker::Switched(client.to_string())
    } else if state.agents.contains_key(client) {
        Marker::Entry(client.to_string())
    } else if use_default && state.default_agent.is_some() {
        Marker::Default
    } else {
        Marker::Nothing
    }
}

/// Handle the `list-agents` / `list` subcommand.
/// Prints the default agent (if set) and all per-TTY registrations, starring
/// the one in use: see `starred`. Inside tmux, first names the client.
fn cmd_list_agents(args: &[String]) -> Result<()> {
    reject_unknown_flags(args, &[])?;

    // Resolve before taking the state lock: this runs tmux.
    let lookup = lookup_client();
    let (client, in_tmux) = match &lookup {
        ClientLookup::NotInTmux => (get_tty(), false),
        ClientLookup::Pane(tty) => {
            println!("client: {tty}");
            (Some(tty.clone()), true)
        }
        ClientLookup::Fallback(tty) => {
            println!("client: {tty} (most recently active; this process is not in a pane)");
            (Some(tty.clone()), true)
        }
        ClientLookup::Unknown => {
            println!("client: unknown (no tmux client found)");
            (None, true)
        }
    };

    let state_file = StateFile::open_default()?;
    state_file.with_lock(|state| {
        let marker = starred(state, client.as_deref(), in_tmux);
        let mark = |on: bool| if on { "*" } else { " " };
        if let Some(ref default) = state.default_agent {
            println!("{} default: {}", mark(marker == Marker::Default), default);
        }
        let mut agents: Vec<_> = state.agents.iter().collect();
        agents.sort_by(|a, b| a.0.cmp(b.0));
        for (tty, agent) in agents {
            let on = matches!(&marker, Marker::Entry(t) if t == tty);
            println!("{} {}: {}", mark(on), tty, agent);
        }
        let mut switched: Vec<_> = state.overrides.iter().collect();
        switched.sort_by(|a, b| a.0.cmp(b.0));
        for (tty, agent) in switched {
            let on = matches!(&marker, Marker::Switched(t) if t == tty);
            println!("{} {} (switched): {}", mark(on), tty, agent);
        }
    })
}

/// Handle the `remove-agent` subcommand.
/// Removes the registration for the TTY given positionally or with `--tty`,
/// else for the current one, which inside tmux is refused.
fn cmd_remove_agent(args: &[String]) -> Result<()> {
    let (flag_tty, rest) = take_option(args, "--tty")?;
    reject_unknown_flags(&rest, &[])?;
    let explicit = match (flag_tty, rest.as_slice()) {
        (t, []) => t,
        (None, [t]) => Some(t.clone()),
        (Some(_), [_]) => anyhow::bail!("Give the TTY either positionally or with --tty, not both"),
        _ => anyhow::bail!("Too many arguments. Run with --help for usage."),
    };
    let tty = target_tty(explicit, "remove-agent")?;
    let state_file = StateFile::open_default()?;
    let (removed, switched) =
        state_file.with_lock(|state| (state.agents.remove(&tty), state.overrides.remove(&tty)))?;
    if removed.is_some() {
        println!("Removed agent for {}", tty);
    } else {
        println!("No agent registered for {}", tty);
    }
    if switched.is_some() {
        println!("Removed the agent {} was switched to", tty);
    }
    Ok(())
}

/// Handle the `clear-agents` subcommand.
/// Removes all per-TTY agent registrations and the default agent.
/// Asks for confirmation unless `--yes` is given.
fn cmd_clear_agents(args: &[String]) -> Result<()> {
    reject_unknown_flags(args, &["--yes", "-y"])?;

    let assume_yes = args.iter().any(|a| a == "--yes" || a == "-y");
    if !assume_yes && !confirm("Remove all agent registrations?")? {
        println!("Aborted");
        return Ok(());
    }

    let state_file = StateFile::open_default()?;
    state_file.with_lock(|state| {
        state.agents.clear();
        state.overrides.clear();
        state.default_agent = None;
    })?;
    println!("Cleared all agent registrations");
    Ok(())
}

/// Ask the user to confirm a destructive action.
/// With no terminal to ask on, refuse rather than assume yes.
fn confirm(question: &str) -> Result<bool> {
    use std::io::IsTerminal;

    if !std::io::stdin().is_terminal() {
        anyhow::bail!("{question} Not running interactively, so pass --yes to confirm.");
    }

    print!("{question} [y/N] ");
    std::io::stdout().flush().ok();

    let mut answer = String::new();
    std::io::stdin()
        .read_line(&mut answer)
        .context("Failed to read confirmation")?;

    Ok(matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

/// Which tmux client this process belongs to.
enum ClientLookup {
    /// `$TMUX` is unset: this is not running under tmux.
    NotInTmux,
    /// Found by walking up to the pane this process runs in, exactly as the
    /// proxy routes a connection.
    Pane(String),
    /// Not inside a pane (e.g. `tmux run-shell`); the most recently active
    /// client, which is the proxy's own fallback.
    Fallback(String),
    /// Under tmux, but no client could be found.
    Unknown,
}

/// Find the tmux client this process belongs to, in the same order the proxy
/// uses (`proxy::resolve_client_tty`).
fn lookup_client() -> ClientLookup {
    if std::env::var_os("TMUX").map_or(true, |v| v.is_empty()) {
        return ClientLookup::NotInTmux;
    }
    if let Some(tty) = crate::routing::client_tty_for_pid(std::process::id() as i32) {
        return ClientLookup::Pane(tty);
    }
    match crate::tmux::current_client() {
        Some(tty) => ClientLookup::Fallback(tty),
        None => ClientLookup::Unknown,
    }
}

/// Get the current TTY path, if there is one.
///
/// Checks `$SSH_TTY` first, then asks the kernel via `ttyname(3)` on stdin.
/// The previous implementation shelled out to `tty` and compared its output
/// against the English string "not a tty", which does not match under any other
/// locale, and fell back to the literal key "unknown" that every TTY-less
/// session then shared.
///
/// Only meaningful outside tmux. In a pane this is the pane's own terminal, or
/// a `$SSH_TTY` the tmux server inherited when it started, neither of which is
/// the client terminal the proxy routes by.
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

/// The TTY a registration command acts on: `explicit` if given, else the
/// current terminal.
///
/// Inside tmux there is no safe default. The pane's terminal is never a
/// routing key, and silently using the client's would overwrite or delete the
/// registration the login shell made for every session on that client.
fn target_tty(explicit: Option<String>, command: &str) -> Result<String> {
    if let Some(tty) = explicit {
        return Ok(tty);
    }
    match lookup_client() {
        ClientLookup::NotInTmux => get_tty().ok_or_else(|| {
            anyhow::anyhow!(
                "No TTY for this session, so there is nothing to act on. \
                 Pass the terminal explicitly: {PROGRAM} {command} --tty TTY"
            )
        }),
        ClientLookup::Pane(client) => anyhow::bail!(
            "Inside tmux this would act on the registration of client {client}, which \
             every session attached from it shares. Registrations normally come from the \
             login shell, outside tmux. To do it anyway, say so: \
             {PROGRAM} {command} --tty {client}"
        ),
        ClientLookup::Fallback(_) | ClientLookup::Unknown => anyhow::bail!(
            "Inside tmux, and this process is not in a pane, so the tmux client cannot \
             be identified. Pass the terminal explicitly: {PROGRAM} {command} --tty TTY \
             (see {PROGRAM} list-agents)"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
    }

    fn state(agents: &[(&str, &str)], default: Option<&str>) -> State {
        State {
            agents: agents
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            default_agent: default.map(str::to_string),
            overrides: Default::default(),
        }
    }

    /// Every name `run` dispatches, aliases included. Kept by hand on purpose:
    /// a command added to the dispatcher without help text fails here.
    const DISPATCHED: &[&str] = &[
        "proxy",
        "install",
        "uninstall",
        "register-agent",
        "register-default-agent",
        "switch-agent",
        "remove-agent",
        "clear-agents",
        "list-agents",
        "list",
    ];

    #[test]
    fn every_command_has_help() {
        for name in DISPATCHED {
            let text = command_help(name).unwrap_or_else(|| panic!("no help for {name}"));
            assert!(text.starts_with("Usage: tmux-ssh-agent "), "{text}");
            assert!(text.len() > 200, "help for {name} is too thin");
        }
        assert_eq!(command_help("list"), command_help("list-agents"));
        assert!(command_help("bogus").is_none());
    }

    #[test]
    fn overview_lists_every_command_with_a_summary() {
        let text = usage();
        for c in COMMANDS {
            assert!(
                text.contains(&format!("tmux-ssh-agent {}", c.name)),
                "{}",
                c.name
            );
            assert!(text.contains(c.summary), "{}", c.name);
        }
        assert!(text.contains("help <command>"));
    }

    #[test]
    fn help_flags_are_recognised_anywhere() {
        assert!(wants_help(&strings(&["--help"])));
        assert!(wants_help(&strings(&["--tty", "/dev/pts/1", "-h"])));
        assert!(!wants_help(&strings(&["/tmp/agent.sock"])));
    }

    #[test]
    fn install_help_names_the_real_markers() {
        let text = command_help("install").unwrap();
        assert!(text.contains(config_edit::MARKER_BEGIN));
        assert!(text.contains(config_edit::MARKER_END));
    }

    #[test]
    fn take_option_separates_value_and_rest() {
        let (v, rest) = take_option(&strings(&["--tty", "/dev/pts/3", "/s"]), "--tty").unwrap();
        assert_eq!(v.as_deref(), Some("/dev/pts/3"));
        assert_eq!(rest, strings(&["/s"]));

        let (v, rest) = take_option(&strings(&["/s", "--tty=/dev/pts/4"]), "--tty").unwrap();
        assert_eq!(v.as_deref(), Some("/dev/pts/4"));
        assert_eq!(rest, strings(&["/s"]));

        let (v, rest) = take_option(&strings(&["/s"]), "--tty").unwrap();
        assert_eq!(v, None);
        assert_eq!(rest, strings(&["/s"]));
    }

    #[test]
    fn take_option_rejects_missing_or_repeated_values() {
        assert!(take_option(&strings(&["--tty"]), "--tty").is_err());
        assert!(take_option(&strings(&["--tty", "--help"]), "--tty").is_err());
        assert!(take_option(&strings(&["--tty="]), "--tty").is_err());
        assert!(take_option(&strings(&["--tty", "a", "--tty", "b"]), "--tty").is_err());
    }

    #[test]
    fn star_outside_tmux_marks_own_entry_only() {
        let s = state(&[("/dev/pts/1", "/a")], Some("/d"));
        assert_eq!(
            starred(&s, Some("/dev/pts/1"), false),
            Marker::Entry("/dev/pts/1".into())
        );
        assert_eq!(starred(&s, Some("/dev/pts/9"), false), Marker::Nothing);
    }

    #[test]
    fn star_inside_tmux_marks_what_the_proxy_uses() {
        let s = state(&[("/dev/pts/1", "/a")], Some("/d"));
        assert_eq!(
            starred(&s, Some("/dev/pts/1"), true),
            Marker::Entry("/dev/pts/1".into())
        );
        assert_eq!(starred(&s, Some("/dev/pts/9"), true), Marker::Default);
        assert_eq!(starred(&s, None, true), Marker::Nothing);

        let no_default = state(&[("/dev/pts/1", "/a")], None);
        assert_eq!(
            starred(&no_default, Some("/dev/pts/9"), true),
            Marker::Nothing
        );
    }

    #[test]
    fn star_prefers_a_switch() {
        let mut s = state(&[("/dev/pts/1", "/a")], Some("/d"));
        s.overrides.insert("/dev/pts/1".into(), "/s".into());
        s.overrides.insert("/dev/pts/2".into(), "/s".into());
        assert_eq!(
            starred(&s, Some("/dev/pts/1"), true),
            Marker::Switched("/dev/pts/1".into())
        );
        assert_eq!(
            starred(&s, Some("/dev/pts/2"), false),
            Marker::Switched("/dev/pts/2".into())
        );
    }

    #[test]
    fn switch_action_parsing() {
        assert_eq!(
            parse_switch_action(&strings(&["/s"])).unwrap(),
            SwitchAction::To("/s".into())
        );
        assert_eq!(
            parse_switch_action(&strings(&["--reset"])).unwrap(),
            SwitchAction::Reset
        );
        assert_eq!(
            parse_switch_action(&strings(&["--menu"])).unwrap(),
            SwitchAction::Menu
        );
        assert!(parse_switch_action(&[]).is_err());
        assert!(parse_switch_action(&strings(&["/s", "--reset"])).is_err());
        assert!(parse_switch_action(&strings(&["/s", "/t"])).is_err());
        assert!(parse_switch_action(&strings(&["--bogus"])).is_err());
    }

    #[test]
    fn menu_lists_each_agent_once() {
        let mut s = state(&[("/dev/pts/2", "/b"), ("/dev/pts/1", "/a")], Some("/a"));
        s.overrides.insert("/dev/pts/3".into(), "/s".into());
        s.overrides.insert("/dev/pts/4".into(), "/b".into());
        let got: Vec<_> = menu_choices(&s).into_iter().map(|c| c.label).collect();
        assert_eq!(
            got,
            [
                "/dev/pts/1: /a",
                "/dev/pts/2: /b",
                "/dev/pts/3 (switched): /s"
            ]
        );
    }

    #[test]
    fn menu_items_star_current_and_offer_reset() {
        let choices = [
            MenuChoice {
                label: "/dev/pts/1: /a".into(),
                socket: "/a".into(),
            },
            MenuChoice {
                label: "bad".into(),
                socket: "/x y;rm".into(),
            },
            MenuChoice {
                label: "default: /d".into(),
                socket: "/d".into(),
            },
        ];
        let items = menu_items("/bin/tsa", "/dev/pts/5", &choices, Some("/d"), true);
        assert_eq!(
            items,
            strings(&[
                "  /dev/pts/1: /a",
                "1",
                "run-shell \"/bin/tsa switch-agent --tty /dev/pts/5 /a >/dev/null\"",
                "* default: /d",
                "2",
                "run-shell \"/bin/tsa switch-agent --tty /dev/pts/5 /d >/dev/null\"",
                "",
                "Reset to the registered agent",
                "r",
                "run-shell \"/bin/tsa switch-agent --tty /dev/pts/5 --reset >/dev/null\"",
            ])
        );
    }

    #[test]
    fn menu_safe_rejects_what_tmux_or_sh_would_reinterpret() {
        assert!(menu_safe("/run/user/1000/ssh-agent.sock"));
        assert!(menu_safe(
            "/private/var/folders/2z/_0y8k/T/ssh-Ab3/agent.12"
        ));
        for bad in [
            "", "/a b", "/a;b", "/a#{x}", "/a'b", "/a\"b", "/a$b", "/a`b",
        ] {
            assert!(!menu_safe(bad), "{bad}");
        }
    }
}
