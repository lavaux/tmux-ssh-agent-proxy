use anyhow::{Context, Result};
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;

/// SSH_AGENT_FAILURE response code (first byte of a failure payload).
pub const SSH_AGENT_FAILURE: u8 = 5;

/// Read one SSH agent protocol packet from `reader`.
/// Format: 4-byte big-endian length prefix, then payload bytes.
/// Returns `Ok(None)` on EOF (connection closed), `Ok(Some(payload))` on success.
pub fn recv_packet<R: Read>(reader: &mut R) -> Result<Option<Vec<u8>>> {
    let mut len_buf = [0u8; 4];
    if reader.read_exact(&mut len_buf).is_err() {
        return Ok(None);
    }
    let len = u32::from_be_bytes(len_buf) as usize;
    let mut payload = vec![0u8; len];
    reader.read_exact(&mut payload)?;
    Ok(Some(payload))
}

/// Write one SSH agent protocol packet to `writer`.
/// Prepends 4-byte big-endian length to `payload`.
pub fn send_packet<W: Write>(writer: &mut W, payload: &[u8]) -> Result<()> {
    let len = (payload.len() as u32).to_be_bytes();
    writer.write_all(&len)?;
    writer.write_all(payload)?;
    Ok(())
}

/// Send a request packet to `stream` and return the response packet.
/// Shortcut for `send_packet` followed by `recv_packet` on the same stream.
pub fn request(stream: &mut UnixStream, payload: &[u8]) -> Result<Option<Vec<u8>>> {
    send_packet(stream, payload)?;
    recv_packet(stream)
}

/// Validate that `path` is usable as an upstream SSH agent socket, and return
/// it in canonical form.
///
/// The daemon forwards the full agent protocol to whatever this path names, so
/// an unvalidated path is a way to make the proxy hand the user's keys to
/// someone else's socket. Requiring current-user ownership makes that a
/// privilege the attacker would already need to have.
pub fn validate_agent_socket(path: &str) -> Result<String> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};

    if path.is_empty() {
        anyhow::bail!("Agent socket path is empty");
    }
    if path.starts_with('-') {
        anyhow::bail!("Refusing to treat {path:?} as an agent socket path (it looks like a flag)");
    }

    let canonical = std::fs::canonicalize(path)
        .with_context(|| format!("Agent socket {path:?} does not exist"))?;

    let meta = std::fs::metadata(&canonical)
        .with_context(|| format!("Failed to stat agent socket {canonical:?}"))?;

    if !meta.file_type().is_socket() {
        anyhow::bail!("Agent path {canonical:?} is not a socket");
    }

    let uid = nix::unistd::getuid().as_raw();
    if meta.uid() != uid {
        anyhow::bail!(
            "Agent socket {canonical:?} is owned by uid {} but we are uid {uid}. Refusing to use it.",
            meta.uid()
        );
    }

    Ok(canonical.to_string_lossy().into_owned())
}
