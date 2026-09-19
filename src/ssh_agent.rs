use anyhow::{Context, Result};
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;

/// SSH_AGENT_FAILURE response code (first byte of a failure payload).
pub const SSH_AGENT_FAILURE: u8 = 5;

/// Largest agent message we will accept, matching OpenSSH's own limit.
/// The length prefix is attacker controlled, so without a cap a four byte
/// header can ask us to allocate 4 GiB.
pub const MAX_PACKET_LEN: usize = 256 * 1024;

/// Read one SSH agent protocol packet from `reader`.
/// Format: 4-byte big-endian length prefix, then payload bytes.
/// Returns `Ok(None)` on EOF (connection closed), `Ok(Some(payload))` on success.
pub fn recv_packet<R: Read>(reader: &mut R) -> Result<Option<Vec<u8>>> {
    let mut len_buf = [0u8; 4];
    match reader.read_exact(&mut len_buf) {
        Ok(()) => {}
        // Only a clean EOF means "peer went away"; anything else is a real
        // error and reporting it as EOF hides genuine failures.
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e).context("Failed to read agent packet length"),
    }

    let len = u32::from_be_bytes(len_buf) as usize;
    if len == 0 {
        anyhow::bail!("Agent packet has zero length");
    }
    if len > MAX_PACKET_LEN {
        anyhow::bail!("Agent packet of {len} bytes exceeds the {MAX_PACKET_LEN} byte limit");
    }

    let mut payload = vec![0u8; len];
    reader
        .read_exact(&mut payload)
        .context("Failed to read agent packet payload")?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn packets_round_trip() {
        let payload = b"\x0bhello agent".to_vec();
        let mut buf = Vec::new();
        send_packet(&mut buf, &payload).expect("send");

        assert_eq!(&buf[..4], &(payload.len() as u32).to_be_bytes());

        let mut cursor = Cursor::new(buf);
        let got = recv_packet(&mut cursor).expect("recv").expect("a packet");
        assert_eq!(got, payload);
    }

    #[test]
    fn clean_eof_is_not_an_error() {
        let mut empty = Cursor::new(Vec::new());
        assert_eq!(recv_packet(&mut empty).expect("eof is ok"), None);
    }

    /// The reason for the cap: the length prefix comes from the peer, and
    /// allocating it unchecked let four bytes ask for 4 GiB.
    #[test]
    fn oversized_length_is_rejected_without_allocating() {
        let mut hostile = Cursor::new(vec![0xff, 0xff, 0xff, 0xff]);
        let err = recv_packet(&mut hostile).expect_err("must be rejected");
        assert!(
            err.to_string().contains("exceeds"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn length_at_the_limit_is_accepted() {
        let payload = vec![0u8; MAX_PACKET_LEN];
        let mut buf = Vec::new();
        send_packet(&mut buf, &payload).expect("send");
        let mut cursor = Cursor::new(buf);
        assert_eq!(
            recv_packet(&mut cursor)
                .expect("recv")
                .expect("packet")
                .len(),
            MAX_PACKET_LEN
        );
    }

    #[test]
    fn one_byte_over_the_limit_is_rejected() {
        let len = (MAX_PACKET_LEN as u32 + 1).to_be_bytes();
        let mut cursor = Cursor::new(len.to_vec());
        assert!(recv_packet(&mut cursor).is_err());
    }

    #[test]
    fn zero_length_is_rejected() {
        let mut cursor = Cursor::new(vec![0, 0, 0, 0]);
        assert!(recv_packet(&mut cursor).is_err());
    }

    #[test]
    fn truncated_payload_is_an_error_not_a_silent_eof() {
        // Claims four bytes, supplies two.
        let mut cursor = Cursor::new(vec![0, 0, 0, 4, 1, 2]);
        assert!(recv_packet(&mut cursor).is_err());
    }
}
