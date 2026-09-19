use anyhow::{Context, Result};
use std::process::Command;

pub fn ping() -> bool {
    Command::new("tmux")
        .args(["display-message", "-p", "1"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim() == "1")
        .unwrap_or(false)
}

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

pub fn current_client() -> Option<String> {
    let output = Command::new("tmux")
        .args(["list-client", "-F", "#{client_activity}:#{client_tty}"])
        .output()
        .ok()?;

    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let (activity_str, tty) = line.split_once(':')?;
            let activity: u64 = activity_str.parse().ok()?;
            Some((activity, tty.to_string()))
        })
        .max_by_key(|(activity, _)| *activity)
        .map(|(_, tty)| tty)
}
