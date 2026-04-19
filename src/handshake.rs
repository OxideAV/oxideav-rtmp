//! RTMP plain handshake (protocol version 3).
//!
//! The "simple" handshake — three fixed-size messages each way,
//! client goes first:
//!
//! ```text
//!   C → S: C0 (1 B, version byte) + C1 (1536 B, 4 time + 4 zero + 1528 random)
//!   S → C: S0 (1 B, version byte) + S1 (1536 B, 4 time + 4 zero + 1528 random)
//!                                 + S2 (1536 B = echo of C1)
//!   C → S: C2 (1536 B = echo of S1)
//! ```
//!
//! Most servers / clients we talk to (OBS, libavformat, nginx-rtmp,
//! node-rtmp-server) accept this simple variant. The Adobe
//! "complex" handshake embeds HMAC-SHA256 digests inside the 1528
//! random bytes and is only actually validated by retired Flash
//! clients + a few DRM boxes — skip for now.
//!
//! We don't check the peer's timestamp field and we don't treat a
//! non-zero "zero" field as fatal — some implementations fill it with
//! junk and carry on. We only verify:
//! * C0 / S0 == `0x03`
//! * each side receives back exactly 1536 bytes of "C2" / "S2"
//!
//! Random payload bytes: we fill with a cheap deterministic PRNG
//! seeded from the system clock — no `rand` dep needed.

use std::io::{Read, Write};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::error::{Error, Result};

pub const RTMP_VERSION: u8 = 0x03;
pub const HANDSHAKE_PAYLOAD_LEN: usize = 1536;

/// Perform the client side of the handshake on an arbitrary
/// `Read + Write`. Blocking.
pub fn client_handshake<S: Read + Write>(stream: &mut S) -> Result<()> {
    let mut c0c1 = [0u8; 1 + HANDSHAKE_PAYLOAD_LEN];
    c0c1[0] = RTMP_VERSION;
    fill_outgoing_payload(&mut c0c1[1..]);
    stream.write_all(&c0c1)?;

    // S0 + S1
    let mut s0 = [0u8; 1];
    stream.read_exact(&mut s0)?;
    if s0[0] != RTMP_VERSION {
        return Err(Error::UnsupportedHandshakeVersion(s0[0]));
    }
    let mut s1 = [0u8; HANDSHAKE_PAYLOAD_LEN];
    stream.read_exact(&mut s1)?;

    // C2 = echo of S1
    stream.write_all(&s1)?;

    // S2 = server's echo of C1 — we don't verify, just drain.
    let mut s2 = [0u8; HANDSHAKE_PAYLOAD_LEN];
    stream.read_exact(&mut s2)?;
    Ok(())
}

/// Perform the server side of the handshake.
pub fn server_handshake<S: Read + Write>(stream: &mut S) -> Result<()> {
    // C0 + C1
    let mut c0 = [0u8; 1];
    stream.read_exact(&mut c0)?;
    if c0[0] != RTMP_VERSION {
        return Err(Error::UnsupportedHandshakeVersion(c0[0]));
    }
    let mut c1 = [0u8; HANDSHAKE_PAYLOAD_LEN];
    stream.read_exact(&mut c1)?;

    // S0 + S1 + S2 (S2 = echo of C1)
    let mut s0s1s2 = [0u8; 1 + HANDSHAKE_PAYLOAD_LEN * 2];
    s0s1s2[0] = RTMP_VERSION;
    fill_outgoing_payload(&mut s0s1s2[1..1 + HANDSHAKE_PAYLOAD_LEN]);
    s0s1s2[1 + HANDSHAKE_PAYLOAD_LEN..].copy_from_slice(&c1);
    stream.write_all(&s0s1s2)?;

    // C2 = client's echo of S1 — drain.
    let mut c2 = [0u8; HANDSHAKE_PAYLOAD_LEN];
    stream.read_exact(&mut c2)?;
    Ok(())
}

/// Fill the first 4 bytes with the current epoch seconds, next 4 with
/// zero, and the remaining 1528 bytes with cheap pseudo-random data.
/// RTMP has never cared about the quality of this randomness for the
/// plain handshake — Adobe's spec literally says "random 1528 bytes".
fn fill_outgoing_payload(buf: &mut [u8]) {
    assert_eq!(buf.len(), HANDSHAKE_PAYLOAD_LEN);
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as u32)
        .unwrap_or(0);
    buf[0..4].copy_from_slice(&secs.to_be_bytes());
    buf[4..8].copy_from_slice(&[0u8; 4]);

    // xorshift64* seeded from the wall clock — deterministic behaviour
    // under clock stutter is fine; RTMP uses these bytes as an opaque
    // nonce echoed back.
    let mut state: u64 = secs as u64 | ((secs as u64) << 32) | 0x9E37_79B9_7F4A_7C15;
    for chunk in buf[8..].chunks_mut(8) {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        let bytes = state.wrapping_mul(0x2545_F491_4F6C_DD1D).to_be_bytes();
        let n = chunk.len();
        chunk.copy_from_slice(&bytes[..n]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// Feed a synthetic "server" reply directly from an in-memory
    /// buffer and verify the client walks the whole handshake.
    #[test]
    fn client_handshake_completes_against_trivial_server() {
        // Build what a dumb echo-server would send:
        // S0 (0x03) + S1 (1536 bytes) + S2 (1536 bytes, echo of C1 — we
        // don't verify so contents don't matter for the test).
        let mut server_to_client = Vec::new();
        server_to_client.push(RTMP_VERSION);
        server_to_client.extend_from_slice(&[0u8; HANDSHAKE_PAYLOAD_LEN]);
        server_to_client.extend_from_slice(&[0u8; HANDSHAKE_PAYLOAD_LEN]);

        let mut duplex = DuplexBuf {
            read: Cursor::new(server_to_client),
            write: Vec::new(),
        };
        client_handshake(&mut duplex).expect("handshake");

        // Client should have written C0 + C1 + C2 (= 1 + 1536 + 1536).
        assert_eq!(
            duplex.write.len(),
            1 + HANDSHAKE_PAYLOAD_LEN + HANDSHAKE_PAYLOAD_LEN
        );
        assert_eq!(duplex.write[0], RTMP_VERSION);
    }

    struct DuplexBuf {
        read: Cursor<Vec<u8>>,
        write: Vec<u8>,
    }
    impl std::io::Read for DuplexBuf {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            self.read.read(buf)
        }
    }
    impl std::io::Write for DuplexBuf {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.write.write(buf)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
}
