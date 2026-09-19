//! Pure text transforms for the blocks this tool adds to a user's config files,
//! plus the file I/O that applies them safely.
//!
//! The editing logic is kept free of I/O so it can be tested directly. That
//! matters here more than anywhere else in the crate: these functions rewrite
//! `~/.tmux.conf` and the user's shell profile, and an earlier version deleted
//! everything between its marker comment and the next line that happened to say
//! `fi`.

use anyhow::{Context, Result};
use std::io::Write;
use std::path::{Path, PathBuf};

/// Opening marker for a block owned by this tool.
pub const MARKER_BEGIN: &str = "# Added by tmux-ssh-agent install";
/// Closing marker. Without it, removal has to guess where the block ends.
pub const MARKER_END: &str = "# End tmux-ssh-agent";

/// The line added to `~/.tmux.conf`.
pub const TMUX_RUN_LINE: &str = "run 'tmux-ssh-agent proxy'";

/// Which syntax a shell profile wants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shell {
    /// bash, zsh, dash, ksh: `if [ ... ]; then ... fi`.
    Posix,
    /// fish, whose `if` syntax is entirely different.
    Fish,
}

/// The login snippet body for a given shell, without the markers.
pub fn shell_snippet(shell: Shell) -> Vec<String> {
    match shell {
        Shell::Posix => vec![
            r#"if [ -z "$TMUX" ] && [ -n "$SSH_AUTH_SOCK" ]; then"#.to_string(),
            "  tmux-ssh-agent register-agent".to_string(),
            "fi".to_string(),
        ],
        Shell::Fish => vec![
            r#"if test -z "$TMUX"; and test -n "$SSH_AUTH_SOCK""#.to_string(),
            "  tmux-ssh-agent register-agent".to_string(),
            "end".to_string(),
        ],
    }
}

/// The body added to `~/.tmux.conf`.
pub fn tmux_snippet() -> Vec<String> {
    vec![TMUX_RUN_LINE.to_string()]
}

/// Whether `contents` already carries one of our blocks, or a hand-written
/// equivalent of a single-line body.
pub fn has_block(contents: &str, body: &[String]) -> bool {
    if contents.lines().any(|l| l.trim() == MARKER_BEGIN) {
        return true;
    }
    body.len() == 1 && contents.lines().any(|l| l.trim() == body[0].trim())
}

/// Append a marked block to `contents`.
/// Returns `None` when the block is already there, so callers stay idempotent.
pub fn add_block(contents: &str, body: &[String]) -> Option<String> {
    if has_block(contents, body) {
        return None;
    }

    let mut out = String::from(contents);
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    if !out.is_empty() {
        out.push('\n');
    }
    out.push_str(MARKER_BEGIN);
    out.push('\n');
    for line in body {
        out.push_str(line);
        out.push('\n');
    }
    out.push_str(MARKER_END);
    out.push('\n');
    Some(out)
}

/// What `remove_block` did.
#[derive(Debug, PartialEq, Eq)]
pub enum Removal {
    /// The block was found and removed; here is the new file content.
    Removed(String),
    /// Nothing of ours is in this file.
    NotPresent,
    /// A begin marker is present, but the block is not bounded by an end
    /// marker and does not match the body we know how to remove. Deleting a
    /// guessed range here is how a user's shell profile gets destroyed, so the
    /// caller must ask the user to edit the file by hand.
    NeedsManual,
}

/// Remove our block from `contents`.
///
/// Prefers the marker pair. Falls back to an exact match of `legacy_body` for
/// files written by versions that emitted no end marker, and for a block the
/// user added by hand from the README.
pub fn remove_block(contents: &str, legacy_body: &[String]) -> Removal {
    let lines: Vec<&str> = contents.lines().collect();
    let trailing_newline = contents.is_empty() || contents.ends_with('\n');

    let begin = lines.iter().position(|l| l.trim() == MARKER_BEGIN);

    let range = match begin {
        Some(start) => {
            match lines
                .iter()
                .skip(start + 1)
                .position(|l| l.trim() == MARKER_END)
            {
                // Bounded by markers: unambiguous.
                Some(offset) => Some((start, start + 1 + offset)),
                // No end marker, so only remove what we can positively identify.
                None => match matches_body(&lines, start + 1, legacy_body) {
                    true => Some((start, start + legacy_body.len())),
                    false => return Removal::NeedsManual,
                },
            }
        }
        None => find_body(&lines, legacy_body).map(|s| (s, s + legacy_body.len() - 1)),
    };

    let Some((start, end)) = range else {
        return Removal::NotPresent;
    };

    let mut kept: Vec<&str> = Vec::with_capacity(lines.len());
    kept.extend_from_slice(&lines[..start]);
    // Drop one blank line ahead of the block, which is the one we inserted.
    if kept.last().is_some_and(|l| l.trim().is_empty()) {
        kept.pop();
    }
    kept.extend_from_slice(&lines[(end + 1).min(lines.len())..]);

    let mut out = kept.join("\n");
    if trailing_newline && !out.is_empty() {
        out.push('\n');
    }
    Removal::Removed(out)
}

/// Do the lines starting at `from` equal `body`?
fn matches_body(lines: &[&str], from: usize, body: &[String]) -> bool {
    if body.is_empty() || from + body.len() > lines.len() {
        return false;
    }
    body.iter()
        .enumerate()
        .all(|(i, want)| lines[from + i].trim() == want.trim())
}

/// Index of the first contiguous run of `body` in `lines`.
fn find_body(lines: &[&str], body: &[String]) -> Option<usize> {
    if body.is_empty() {
        return None;
    }
    (0..lines.len()).find(|&i| matches_body(lines, i, body))
}

/// Backup path for `path`: the whole file name with `.bak` appended.
///
/// Note that `Path::with_extension` is wrong for this. On `~/.tmux.conf` it
/// sees stem `.tmux` and extension `conf`, so it used to produce
/// `~/.tmux.tmux.conf.bak`, and on `config.fish` it replaced `.fish` outright.
pub fn backup_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".bak");
    path.with_file_name(name)
}

/// A backup path that does not already exist, so repeated installs cannot
/// overwrite the pristine copy with an already-modified one.
pub fn unique_backup_path(path: &Path) -> PathBuf {
    let base = backup_path(path);
    if !base.exists() {
        return base;
    }
    for n in 1..1000 {
        let mut name = base.file_name().unwrap_or_default().to_os_string();
        name.push(format!(".{n}"));
        let candidate = base.with_file_name(name);
        if !candidate.exists() {
            return candidate;
        }
    }
    base
}

/// Copy `path` aside before modifying it. Returns the backup path.
pub fn backup(path: &Path) -> Result<PathBuf> {
    let backup = unique_backup_path(path);
    std::fs::copy(path, &backup)
        .with_context(|| format!("Failed to back up {} to {:?}", path.display(), backup))?;
    Ok(backup)
}

/// Replace `path`'s contents by writing a temporary file alongside it and
/// renaming over the target.
///
/// The previous code truncated the real file and then wrote it back line by
/// line, so a failure partway through left the user with a truncated shell
/// profile.
pub fn write_atomic(path: &Path, contents: &str) -> Result<()> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let tmp = dir.join(format!(".{name}.tmp{}", std::process::id()));

    let mode = path
        .metadata()
        .ok()
        .map(|m| std::os::unix::fs::PermissionsExt::mode(&m.permissions()));

    let result = (|| -> Result<()> {
        let mut file = std::fs::File::create(&tmp)
            .with_context(|| format!("Failed to create {}", tmp.display()))?;
        file.write_all(contents.as_bytes())
            .with_context(|| format!("Failed to write {}", tmp.display()))?;
        file.sync_all()
            .with_context(|| format!("Failed to flush {}", tmp.display()))?;
        if let Some(mode) = mode {
            file.set_permissions(std::os::unix::fs::PermissionsExt::from_mode(mode))
                .with_context(|| format!("Failed to set mode on {}", tmp.display()))?;
        }
        drop(file);
        std::fs::rename(&tmp, path)
            .with_context(|| format!("Failed to replace {}", path.display()))?;
        Ok(())
    })();

    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn posix() -> Vec<String> {
        shell_snippet(Shell::Posix)
    }

    #[test]
    fn add_then_remove_is_the_identity() {
        let original = "# my tmux config\nset -g mouse on\n";
        let added = add_block(original, &tmux_snippet()).expect("block added");
        assert!(added.contains(MARKER_BEGIN));
        assert!(added.contains(MARKER_END));
        assert!(added.contains(TMUX_RUN_LINE));

        match remove_block(&added, &tmux_snippet()) {
            Removal::Removed(back) => assert_eq!(back, original),
            other => panic!("expected removal, got {other:?}"),
        }
    }

    #[test]
    fn add_then_remove_is_the_identity_for_shell_profiles() {
        let original = "export PATH=$PATH:/opt/bin\n\nalias ll='ls -l'\n";
        let added = add_block(original, &posix()).expect("block added");
        match remove_block(&added, &posix()) {
            Removal::Removed(back) => assert_eq!(back, original),
            other => panic!("expected removal, got {other:?}"),
        }
    }

    #[test]
    fn add_is_idempotent() {
        let once = add_block("set -g mouse on\n", &tmux_snippet()).expect("added");
        assert_eq!(add_block(&once, &tmux_snippet()), None);
    }

    #[test]
    fn add_does_not_duplicate_a_hand_written_run_line() {
        let manual = format!("# set up by hand\n{TMUX_RUN_LINE}\n");
        assert_eq!(add_block(&manual, &tmux_snippet()), None);
    }

    #[test]
    fn remove_takes_a_hand_written_run_line() {
        let manual = format!("set -g mouse on\n{TMUX_RUN_LINE}\nset -g status on\n");
        match remove_block(&manual, &tmux_snippet()) {
            Removal::Removed(out) => {
                assert_eq!(out, "set -g mouse on\nset -g status on\n");
            }
            other => panic!("expected removal, got {other:?}"),
        }
    }

    #[test]
    fn remove_reports_when_there_is_nothing_to_do() {
        assert_eq!(
            remove_block("set -g mouse on\n", &tmux_snippet()),
            Removal::NotPresent
        );
    }

    /// The bug that motivated the end marker: an unterminated legacy block used
    /// to be removed by deleting everything up to the next line saying `fi`,
    /// which swallowed whatever the user had written in between.
    #[test]
    fn edited_legacy_block_is_refused_rather_than_guessed() {
        let hand_edited = format!(
            "{MARKER_BEGIN}\n\
             if [ -z \"$TMUX\" ]; then\n\
             \x20 tmux-ssh-agent register-agent --with-my-own-flag\n\
             fi\n\
             \n\
             # something the user cares about\n\
             if [ -n \"$EDITOR\" ]; then\n\
             \x20 export VISUAL=$EDITOR\n\
             fi\n"
        );
        assert_eq!(remove_block(&hand_edited, &posix()), Removal::NeedsManual);
    }

    #[test]
    fn untouched_legacy_block_is_still_removed() {
        let mut legacy = String::from("alias ll='ls -l'\n\n");
        legacy.push_str(MARKER_BEGIN);
        legacy.push('\n');
        for line in posix() {
            legacy.push_str(&line);
            legacy.push('\n');
        }
        legacy.push_str("\n# later config\n");

        match remove_block(&legacy, &posix()) {
            Removal::Removed(out) => {
                assert_eq!(out, "alias ll='ls -l'\n\n# later config\n");
                assert!(!out.contains(MARKER_BEGIN));
            }
            other => panic!("expected removal, got {other:?}"),
        }
    }

    /// The old code did `i += 2` on the marker, so whatever followed it was
    /// deleted unread.
    #[test]
    fn a_stray_line_after_the_marker_is_not_eaten() {
        let mut contents = String::new();
        contents.push_str(MARKER_BEGIN);
        contents.push_str("\n# a note the user added\n");
        contents.push_str(TMUX_RUN_LINE);
        contents.push('\n');
        contents.push_str(MARKER_END);
        contents.push('\n');

        match remove_block(&contents, &tmux_snippet()) {
            Removal::Removed(out) => assert_eq!(out, ""),
            other => panic!("expected removal, got {other:?}"),
        }

        // Without the end marker the same input is ambiguous and must be refused.
        let unterminated = format!("{MARKER_BEGIN}\n# a note the user added\n{TMUX_RUN_LINE}\n");
        assert_eq!(
            remove_block(&unterminated, &tmux_snippet()),
            Removal::NeedsManual
        );
    }

    #[test]
    fn fish_snippet_uses_fish_syntax() {
        let fish = shell_snippet(Shell::Fish);
        assert!(fish[0].starts_with("if test "));
        assert_eq!(fish[2], "end");
        assert!(!fish.iter().any(|l| l.contains("; then")));
        assert!(!fish.iter().any(|l| l == "fi"));
    }

    #[test]
    fn file_without_trailing_newline_keeps_not_having_one() {
        let original = "set -g mouse on";
        let added = add_block(original, &tmux_snippet()).expect("added");
        assert!(added.starts_with("set -g mouse on\n"));
        match remove_block(&added, &tmux_snippet()) {
            // add_block normalises the missing newline; removal must not add more.
            Removal::Removed(back) => assert_eq!(back, "set -g mouse on\n"),
            other => panic!("expected removal, got {other:?}"),
        }
    }

    #[test]
    fn backup_path_appends_rather_than_replacing_the_extension() {
        assert_eq!(
            backup_path(Path::new("/home/u/.tmux.conf")),
            Path::new("/home/u/.tmux.conf.bak")
        );
        assert_eq!(
            backup_path(Path::new("/home/u/.bashrc")),
            Path::new("/home/u/.bashrc.bak")
        );
        assert_eq!(
            backup_path(Path::new("/home/u/.config/fish/config.fish")),
            Path::new("/home/u/.config/fish/config.fish.bak")
        );
    }
}
