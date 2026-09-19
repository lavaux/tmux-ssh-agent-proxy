//! Work out which tmux client a connection belongs to.
//!
//! The naive answer, and what this tool did before, is "the most recently
//! active client on the server". That is exact with one client attached, but
//! with several it means whichever client you typed in last wins for every
//! pane, so a request from one machine's pane can be signed by another
//! machine's agent.
//!
//! This module answers the question properly. The kernel reports the pid on the
//! other end of the socket; walking that pid up the process tree until it meets
//! a pane's process identifies the pane, and therefore the session, the request
//! actually came from. Clients attached to that session are then the only
//! candidates.
//!
//! Both steps are platform specific. Linux reads peer credentials with
//! `SO_PEERCRED` and parents from `/proc`; Darwin uses `LOCAL_PEERPID` plus
//! `getpeereid(3)`, and libproc for parents. Everything between and above those
//! two primitives is shared.
//!
//! Every step can fail (the peer may not be inside tmux at all), and the caller
//! falls back to the server-wide heuristic when it does.

use crate::tmux;
use std::os::unix::net::UnixStream;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Give up walking the process tree after this many steps. The tree is a tree,
/// so this is belt and braces against the kernel reporting something absurd.
const MAX_ANCESTRY_DEPTH: usize = 64;

/// How long a pane listing stays usable before it is re-read.
const PANE_CACHE_TTL: Duration = Duration::from_secs(2);

/// Floor on how often a cache miss may re-read the pane list. Without this,
/// every connection from outside tmux (which never matches) would fork a tmux
/// process.
const PANE_CACHE_MIN_REFRESH: Duration = Duration::from_millis(250);

/// A `pane_pid -> session_name` listing.
pub type PaneSessions = Vec<(i32, String)>;

/// Such a listing together with the time it was read.
type CachedPanes = (Instant, PaneSessions);

/// Cached pane listing, refreshed on demand.
static PANE_CACHE: Mutex<Option<CachedPanes>> = Mutex::new(None);

/// The TTY of the tmux client that owns the process on the other end of
/// `stream`, or `None` if it cannot be determined.
pub fn client_tty_for_peer(stream: &UnixStream) -> Option<String> {
    let pid = peer_pid(stream)?;
    let ancestry = ancestry(pid);
    let session = lookup_session(&ancestry)?;
    tmux::active_client_in_session(&session)
}

/// The uid on the other end of `stream`.
///
/// The socket is 0600, so a different uid should not be able to connect at all.
/// Checking makes that guarantee explicit rather than inherited from the mode.
pub fn peer_uid(stream: &UnixStream) -> Option<u32> {
    peer_credentials(stream).map(|(_, uid)| uid)
}

/// The pid on the other end of `stream`.
fn peer_pid(stream: &UnixStream) -> Option<i32> {
    peer_credentials(stream).map(|(pid, _)| pid)
}

/// Linux hands back the pid, uid and gid together in one `SO_PEERCRED` call.
#[cfg(target_os = "linux")]
fn peer_credentials(stream: &UnixStream) -> Option<(i32, u32)> {
    use nix::sys::socket::{getsockopt, sockopt::PeerCredentials};

    let cred = getsockopt(stream, PeerCredentials).ok()?;
    Some((cred.pid(), cred.uid()))
}

/// Darwin has no `SO_PEERCRED`, and its `LOCAL_PEERCRED` yields an `xucred`
/// carrying the uid but no pid, so the two halves come from separate calls.
///
/// The uid comes from `getpeereid(3)` rather than `LOCAL_PEERCRED` because it
/// returns the uid directly. nix's `XuCred::groups()` ignores `cr_ngroups` and
/// hands back all 16 array slots including the unset ones, which is a trap not
/// worth stepping near when the uid is all that is wanted.
#[cfg(target_vendor = "apple")]
fn peer_credentials(stream: &UnixStream) -> Option<(i32, u32)> {
    use nix::sys::socket::{getsockopt, sockopt::LocalPeerPid};

    let pid = getsockopt(stream, LocalPeerPid).ok()?;
    let (uid, _gid) = nix::unistd::getpeereid(stream).ok()?;
    Some((pid, uid.as_raw()))
}

/// Other unices expose peer credentials differently (or not at all), so the
/// caller falls back to the server-wide heuristic there.
#[cfg(not(any(target_os = "linux", target_vendor = "apple")))]
fn peer_credentials(_stream: &UnixStream) -> Option<(i32, u32)> {
    None
}

/// `pid` and its ancestors, nearest first, stopping at init.
fn ancestry(pid: i32) -> Vec<i32> {
    let mut out = Vec::new();
    let mut current = pid;

    for _ in 0..MAX_ANCESTRY_DEPTH {
        if current <= 1 {
            break;
        }
        out.push(current);
        match parent_of(current) {
            // A process cannot be its own parent; if the kernel says otherwise, stop.
            Some(parent) if parent != current => current = parent,
            _ => break,
        }
    }

    out
}

/// Read a process's parent pid from `/proc`.
#[cfg(target_os = "linux")]
fn parent_of(pid: i32) -> Option<i32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    parse_ppid(&stat)
}

/// Read a process's parent pid through libproc.
///
/// Darwin has no `/proc`. The sysctl route would want a `struct kinfo_proc`,
/// which the `libc` crate does not define for Apple targets, so it would have
/// to be hand-transcribed and kept in step with the SDK. `proc_pidinfo` is
/// declared for Apple and needs no extra linking, because libproc is a stub
/// inside libSystem.
#[cfg(target_vendor = "apple")]
fn parent_of(pid: i32) -> Option<i32> {
    use nix::libc::{proc_bsdinfo, proc_pidinfo, PROC_PIDTBSDINFO};

    let size = std::mem::size_of::<proc_bsdinfo>();

    // SAFETY: `proc_bsdinfo` is a plain C struct of integers and byte arrays,
    // so an all-zero value is a valid instance. `proc_pidinfo` writes at most
    // the buffer size it is given, which is passed explicitly below.
    let mut info: proc_bsdinfo = unsafe { std::mem::zeroed() };
    let written = unsafe {
        proc_pidinfo(
            pid,
            PROC_PIDTBSDINFO,
            0,
            &mut info as *mut proc_bsdinfo as *mut nix::libc::c_void,
            size as nix::libc::c_int,
        )
    };

    // libproc returns the number of bytes written, or 0 on failure. A short
    // write means we did not get the struct we asked for, so trust nothing.
    if written != size as nix::libc::c_int {
        return None;
    }

    match info.pbi_ppid {
        0 => None,
        ppid => Some(ppid as i32),
    }
}

#[cfg(not(any(target_os = "linux", target_vendor = "apple")))]
fn parent_of(_pid: i32) -> Option<i32> {
    None
}

/// Pull the parent pid out of a `/proc/<pid>/stat` line.
///
/// The second field is the executable name in parentheses and may itself
/// contain spaces and parentheses, so the fixed-position fields are only
/// reliable after the *last* `)`.
#[cfg(target_os = "linux")]
fn parse_ppid(stat: &str) -> Option<i32> {
    let close = stat.rfind(')')?;
    let mut fields = stat[close + 1..].split_whitespace();
    let _state = fields.next()?;
    fields.next()?.parse().ok()
}

/// The session owning the nearest ancestor that is a tmux pane process.
fn lookup_session(ancestry: &[i32]) -> Option<String> {
    let mut cache = PANE_CACHE.lock().ok()?;

    if let Some((taken_at, panes)) = cache.as_ref() {
        if taken_at.elapsed() < PANE_CACHE_TTL {
            if let Some(session) = match_session(panes, ancestry) {
                return Some(session);
            }
            // A miss may just mean the pane is newer than the cache, so fall
            // through and re-read. Not too often, though.
            if taken_at.elapsed() < PANE_CACHE_MIN_REFRESH {
                return None;
            }
        }
    }

    let panes = tmux::pane_sessions()?;
    let result = match_session(&panes, ancestry);
    *cache = Some((Instant::now(), panes));
    result
}

/// First entry of `ancestry` that is a pane process, resolved to its session.
/// Ancestry is ordered nearest first, so the innermost pane wins when tmux is
/// nested inside tmux.
fn match_session(panes: &[(i32, String)], ancestry: &[i32]) -> Option<String> {
    ancestry.iter().find_map(|pid| {
        panes
            .iter()
            .find(|(pane_pid, _)| pane_pid == pid)
            .map(|(_, session)| session.clone())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(target_os = "linux")]
    fn reads_the_parent_pid_from_a_stat_line() {
        let stat = "1819367 (cat) R 1819343 1819367 1819343 0 -1 4194304 503 0 0";
        assert_eq!(parse_ppid(stat), Some(1819343));
    }

    /// The comm field is attacker-influenced: a process can name itself
    /// anything, including something with spaces and parentheses in it.
    #[test]
    #[cfg(target_os = "linux")]
    fn a_process_name_containing_spaces_and_parens_does_not_confuse_the_parse() {
        let stat = "4242 (weird ) name (x) S 99 4242 99 0 -1 0 0";
        assert_eq!(parse_ppid(stat), Some(99));
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn malformed_stat_lines_yield_nothing() {
        assert_eq!(parse_ppid(""), None);
        assert_eq!(parse_ppid("no parens here"), None);
        assert_eq!(parse_ppid("123 (sh) S"), None);
        assert_eq!(parse_ppid("123 (sh) S notanumber"), None);
    }

    /// Requires a real `parent_of`, which exists on Linux and Apple targets.
    /// Elsewhere `ancestry` deliberately stops at the process itself.
    #[test]
    #[cfg(any(target_os = "linux", target_vendor = "apple"))]
    fn ancestry_starts_at_the_process_itself_and_climbs() {
        let me = std::process::id() as i32;
        let chain = ancestry(me);

        assert_eq!(chain.first(), Some(&me), "the peer itself must be first");
        assert!(chain.len() > 1, "the test runner has a parent: {chain:?}");
        // Strictly climbing, and never revisiting a pid.
        let mut seen = chain.clone();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), chain.len(), "ancestry must not loop: {chain:?}");
    }

    #[test]
    fn ancestry_of_init_is_empty() {
        assert!(ancestry(1).is_empty());
        assert!(ancestry(0).is_empty());
    }

    #[test]
    fn the_nearest_pane_ancestor_wins() {
        // An outer tmux pane (100) running an inner tmux whose pane is 200.
        let panes = vec![(100, "outer".to_string()), (200, "inner".to_string())];
        // ssh(400) -> shell(300) -> inner pane(200) -> ... -> outer pane(100)
        let chain = vec![400, 300, 200, 150, 100];
        assert_eq!(match_session(&panes, &chain).as_deref(), Some("inner"));
    }

    #[test]
    fn a_peer_outside_tmux_matches_nothing() {
        let panes = vec![(100, "work".to_string())];
        assert_eq!(match_session(&panes, &[900, 901, 902]), None);
    }

    #[test]
    fn no_panes_matches_nothing() {
        assert_eq!(match_session(&[], &[1, 2, 3]), None);
    }
}
