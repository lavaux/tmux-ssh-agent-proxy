use anyhow::Result;
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
