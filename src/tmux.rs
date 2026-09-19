use anyhow::{Context, Result};
use std::process::Command;

/// Check whether the tmux server is still alive.
/// Returns `false` when tmux has exited or is unreachable.
pub fn ping() -> bool {
    Command::new("tmux")
        .args(["display-message", "-p", "1"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim() == "1")
        .unwrap_or(false)
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

#[cfg(test)]
mod tests {
    use super::parse_active_client;

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
    fn malformed_lines_are_skipped_not_fatal() {
        let out = "garbage\n1700000002:/dev/pts/7\n:no-activity\n999:\n";
        assert_eq!(parse_active_client(out).as_deref(), Some("/dev/pts/7"));
    }
}
