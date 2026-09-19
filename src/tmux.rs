use anyhow::{Context, Result};
use std::process::Command;

/// Check whether the tmux server is still alive.
/// Returns `false` when tmux has exited or is unreachable.
pub fn ping() -> bool {
    matches!(probe(), Ok(true))
}

/// Why a tmux probe did not succeed.
#[derive(Debug)]
pub enum ProbeError {
    /// No `tmux` binary on `$PATH`.
    NotFound,
    /// The binary is there but could not be run.
    Failed(std::io::Error),
}

impl std::fmt::Display for ProbeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            // Worth spelling out: on macOS tmux comes from Homebrew, in
            // /opt/homebrew/bin or /usr/local/bin, and neither is on the
            // default PATH a launchd-started process inherits.
            Self::NotFound => write!(
                f,
                "no `tmux` binary found on $PATH (PATH={})",
                std::env::var("PATH").unwrap_or_default()
            ),
            Self::Failed(e) => write!(f, "could not run `tmux`: {e}"),
        }
    }
}

/// Run `tmux display-message` and report what happened.
///
/// `ping` collapses this to a bool for the accept loop, but startup wants the
/// distinction: "tmux is not installed" and "the server has exited" are very
/// different problems and used to be indistinguishable.
pub fn probe() -> std::result::Result<bool, ProbeError> {
    match Command::new("tmux")
        .args(["display-message", "-p", "1"])
        .output()
    {
        Ok(o) => Ok(String::from_utf8_lossy(&o.stdout).trim() == "1"),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(ProbeError::NotFound),
        Err(e) => Err(ProbeError::Failed(e)),
    }
}

/// Set a global tmux environment variable (`tmux set-environment -g`).
pub fn set_environment(name: &str, value: &str) -> Result<()> {
    let status = Command::new("tmux")
        .args(["set-environment", "-g", name, value])
        .status()
        .with_context(|| "Failed to execute tmux set-environment")?;
    if !status.success() {
        anyhow::bail!("tmux set-environment failed");
    }
    Ok(())
}

/// Return the TTY of the most recently active tmux client, if any.
/// Parses `tmux list-client -F '#{client_activity}:#{client_tty}'`
/// and picks the client with the highest activity timestamp.
pub fn current_client() -> Option<String> {
    let output = Command::new("tmux")
        .args(["list-clients", "-F", "#{client_activity}:#{client_tty}"])
        .output()
        .ok()?;

    parse_active_client(&String::from_utf8_lossy(&output.stdout))
}

/// Pick the TTY with the highest activity timestamp out of `tmux list-clients`
/// output. Split out from the subprocess call so it can be tested directly.
///
/// Note that this is server-wide: with several clients attached it names the
/// most recently used one, not the one that owns the pane a request came from.
fn parse_active_client(output: &str) -> Option<String> {
    output
        .lines()
        .filter_map(|line| {
            let (activity_str, tty) = line.split_once(':')?;
            let activity: u64 = activity_str.trim().parse().ok()?;
            if tty.is_empty() {
                return None;
            }
            Some((activity, tty.to_string()))
        })
        .max_by_key(|(activity, _)| *activity)
        .map(|(_, tty)| tty)
}

/// The variables tmux copies from an attaching client into the session
/// environment (`update-environment`).
///
/// This matters because the session environment shadows the global one, so
/// anything on this list cannot be controlled with `set-environment -g`.
pub fn update_environment() -> Option<Vec<String>> {
    let output = Command::new("tmux")
        .args(["show-options", "-g", "update-environment"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(parse_update_environment(&String::from_utf8_lossy(
        &output.stdout,
    )))
}

/// Parse `show-options -g update-environment`, whose lines look like
/// `update-environment[0] DISPLAY`.
fn parse_update_environment(output: &str) -> Vec<String> {
    output
        .lines()
        .filter_map(|line| {
            let (_option, value) = line.split_once(' ')?;
            let value = value.trim().trim_matches('"');
            if value.is_empty() {
                None
            } else {
                Some(value.to_string())
            }
        })
        .collect()
}

/// Replace `update-environment` wholesale. tmux splits a space-separated value
/// back into array elements.
pub fn set_update_environment(vars: &[String]) -> Result<()> {
    let status = Command::new("tmux")
        .args(["set-option", "-g", "update-environment", &vars.join(" ")])
        .status()
        .with_context(|| "Failed to execute tmux set-option")?;
    if !status.success() {
        anyhow::bail!("tmux set-option update-environment failed");
    }
    Ok(())
}

/// Names of all sessions on the server.
pub fn sessions() -> Option<Vec<String>> {
    let output = Command::new("tmux")
        .args(["list-sessions", "-F", "#{session_name}"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(str::to_string)
            .filter(|s| !s.is_empty())
            .collect(),
    )
}

/// Remove a variable from one session's environment, so the global value shows
/// through again. Failure is not interesting: usually it just was not set.
pub fn unset_session_environment(session: &str, name: &str) {
    let _ = Command::new("tmux")
        .args(["set-environment", "-u", "-t", &format!("={session}"), name])
        .status();
}

/// Map each pane's process to the session it belongs to, via
/// `tmux list-panes -a`.
///
/// The pane pid is the process tmux started in that pane, usually a shell, so
/// anything run in the pane is one of its descendants.
pub fn pane_sessions() -> Option<crate::routing::PaneSessions> {
    let output = Command::new("tmux")
        .args(["list-panes", "-a", "-F", "#{pane_pid}:#{session_name}"])
        .output()
        .ok()?;

    Some(parse_pane_sessions(&String::from_utf8_lossy(
        &output.stdout,
    )))
}

/// Parse `tmux list-panes` output. Split out from the subprocess call so it can
/// be tested directly.
fn parse_pane_sessions(output: &str) -> crate::routing::PaneSessions {
    output
        .lines()
        .filter_map(|line| {
            // Split on the first colon only: the pid cannot contain one, but a
            // session name can.
            let (pid, session) = line.split_once(':')?;
            let pid: i32 = pid.trim().parse().ok()?;
            if session.is_empty() {
                return None;
            }
            Some((pid, session.to_string()))
        })
        .collect()
}

/// The TTY of the most recently active client attached to `session`.
///
/// Unlike `current_client`, this only considers clients of one session, which
/// is what makes per-pane routing possible.
pub fn active_client_in_session(session: &str) -> Option<String> {
    let output = Command::new("tmux")
        .args([
            "list-clients",
            // "=" asks tmux for an exact name match rather than its usual
            // prefix and pattern matching, so an odd session name cannot
            // select a different session.
            "-t",
            &format!("={session}"),
            "-F",
            "#{client_activity}:#{client_tty}",
        ])
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    parse_active_client(&String::from_utf8_lossy(&output.stdout))
}

#[cfg(test)]
mod tests {
    use super::{parse_active_client, parse_pane_sessions, parse_update_environment};

    #[test]
    fn picks_the_most_recently_active_client() {
        let out = "1700000001:/dev/pts/0\n1700000009:/dev/pts/3\n1700000005:/dev/pts/1\n";
        assert_eq!(parse_active_client(out).as_deref(), Some("/dev/pts/3"));
    }

    #[test]
    fn handles_a_single_client() {
        assert_eq!(
            parse_active_client("1700000001:/dev/pts/0\n").as_deref(),
            Some("/dev/pts/0")
        );
    }

    #[test]
    fn no_attached_clients_yields_none() {
        assert_eq!(parse_active_client(""), None);
        assert_eq!(parse_active_client("\n"), None);
    }

    #[test]
    fn parses_the_update_environment_array() {
        let out = "update-environment[0] DISPLAY\n\
                   update-environment[1] SSH_AUTH_SOCK\n\
                   update-environment[2] XAUTHORITY\n";
        assert_eq!(
            parse_update_environment(out),
            vec!["DISPLAY", "SSH_AUTH_SOCK", "XAUTHORITY"]
        );
    }

    #[test]
    fn an_empty_update_environment_parses_to_nothing() {
        assert!(parse_update_environment("").is_empty());
        assert!(parse_update_environment("update-environment[0] \n").is_empty());
    }

    #[test]
    fn parses_pane_to_session_mapping() {
        let out = "1234:work\n5678:personal\n";
        assert_eq!(
            parse_pane_sessions(out),
            vec![(1234, "work".to_string()), (5678, "personal".to_string()),]
        );
    }

    #[test]
    fn a_session_name_containing_a_colon_survives() {
        assert_eq!(
            parse_pane_sessions("42:my:weird:session\n"),
            vec![(42, "my:weird:session".to_string())]
        );
    }

    #[test]
    fn malformed_pane_lines_are_skipped() {
        let out = "garbage\n99:ok\nnotapid:session\n7:\n";
        assert_eq!(parse_pane_sessions(out), vec![(99, "ok".to_string())]);
    }

    #[test]
    fn malformed_lines_are_skipped_not_fatal() {
        let out = "garbage\n1700000002:/dev/pts/7\n:no-activity\n999:\n";
        assert_eq!(parse_active_client(out).as_deref(), Some("/dev/pts/7"));
    }
}
