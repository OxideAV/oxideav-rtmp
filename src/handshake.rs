//! RTMP handshakes (protocol version 3): plain echo and digest.
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
//! Most commodity servers / clients we have interoperated with accept
//! this simple variant.
//!
//! The **digest** ("complex") handshake reuses the same packet sizes
//! but authenticates them: a non-zero *version* field (bytes 4..8 of
//! C1/S1) signals that the 1528 trailing bytes are structured as a
//! 764-byte key block plus a 764-byte digest block carrying an
//! HMAC-SHA256 digest, and C2/S2 stop being echoes — each carries a
//! trailing 32-byte response digest chained from the peer's C1/S1
//! digest. See `docs/streaming/rtmp/rtmp-so-dataframe-digest-handshake.md`
//! §3 for the byte-level derivation. Entry points:
//!
//! * [`client_handshake_digest`] — digest C1 with automatic fallback
//!   to the simple echo exchange when the server doesn't answer with a
//!   digested S1.
//! * [`server_handshake_negotiated`] — auto-detects whether the client
//!   sent a simple or digest C1 and answers in kind. The plain
//!   [`server_handshake`] wraps it, so [`crate::RtmpServer`] accepts
//!   both client flavours transparently.
//!
//! For the simple exchange we don't check the peer's timestamp field
//! and we don't treat a non-zero "zero" field as fatal — some
//! implementations fill it with junk and carry on (a junk version
//! field that fails digest detection falls back to the echo path). We
//! only verify:
//! * C0 / S0 == `0x03`
//! * each side receives back exactly 1536 bytes of "C2" / "S2"
//!
//! Random payload bytes: we fill with a cheap deterministic PRNG
//! seeded from the system clock — no `rand` dep needed. The digest
//! scheme's security comes from the HMAC over fixed published keys
//! (it is an obfuscation gate, not a secrecy mechanism), so nonce
//! quality is equally irrelevant there.

use std::io::{Read, Write};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::error::{Error, Result};
use crate::hmac::hmac_sha256;

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

/// Perform the server side of the handshake, auto-detecting whether
/// the client speaks the simple or the digest variant (see
/// [`server_handshake_negotiated`], which this wraps).
pub fn server_handshake<S: Read + Write>(stream: &mut S) -> Result<()> {
    server_handshake_negotiated(stream).map(|_| ())
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

    fill_random(&mut buf[8..], secs as u64);
}

/// xorshift64* filler seeded from the wall clock — deterministic
/// behaviour under clock stutter is fine; RTMP uses these bytes as an
/// opaque nonce (echoed back in the simple exchange, HMAC'd in the
/// digest exchange).
fn fill_random(buf: &mut [u8], seed: u64) {
    let mut state: u64 = seed | (seed << 32) | 0x9E37_79B9_7F4A_7C15;
    for chunk in buf.chunks_mut(8) {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        let bytes = state.wrapping_mul(0x2545_F491_4F6C_DD1D).to_be_bytes();
        let n = chunk.len();
        chunk.copy_from_slice(&bytes[..n]);
    }
}

fn now_secs() -> u32 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as u32)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Digest ("complex") handshake
// ---------------------------------------------------------------------------
// Byte-level layout per
// docs/streaming/rtmp/rtmp-so-dataframe-digest-handshake.md §3: the
// 1528 bytes after `time(4) | version(4)` split into a 764-byte key
// block and a 764-byte digest block whose order is the "schema"; the
// digest block hides a 32-byte HMAC-SHA256 at a self-described offset,
// and C2/S2 answer with a chained response digest in their last 32
// bytes.

/// Length of the embedded HMAC-SHA256 digest.
pub const HANDSHAKE_DIGEST_LEN: usize = 32;
/// Length of the Diffie-Hellman public key carried by the key block
/// (used by the encrypted-transport variant; random filler for
/// plaintext RTMP but the slot always exists).
pub const HANDSHAKE_DH_KEY_LEN: usize = 128;
/// Each of the two blocks following the 8-byte packet header.
const BLOCK_LEN: usize = 764;
/// Modulus for the digest block's self-described offset
/// (`764 - 4 offset bytes - 32 digest bytes = 728`).
const DIGEST_OFFSET_MOD: usize = 728;
/// Modulus for the key block's self-described offset
/// (`764 - 4 offset bytes - 128 key bytes = 632`).
const KEY_OFFSET_MOD: usize = 632;

/// Version field a digest-handshake client advertises in C1 bytes
/// 4..8 (a Flash-Player-era version number; any non-zero value signals
/// the digest scheme).
pub const HANDSHAKE_CLIENT_VERSION: u32 = 0x8000_0702;
/// Version field a digest-handshake server advertises in S1 bytes 4..8
/// (a media-server-era version number).
pub const HANDSHAKE_SERVER_VERSION: u32 = 0x0D0E_0A0D;

/// 32-byte suffix shared by both handshake key constants.
const KEY_SUFFIX: [u8; 32] = [
    0xF0, 0xEE, 0xC2, 0x4A, 0x80, 0x68, 0xBE, 0xE8, 0x2E, 0x00, 0xD0, 0xD1, 0x02, 0x9E, 0x7E, 0x57,
    0x6E, 0xEC, 0x5D, 0x2D, 0x29, 0x80, 0x6F, 0xAB, 0x93, 0xB8, 0xE6, 0x36, 0xCF, 0xEB, 0x31, 0xAE,
];

/// Number of label bytes of [`GENUINE_FP_KEY`] used to key the C1
/// digest.
pub const FP_LABEL_LEN: usize = 30;
/// Number of label bytes of [`GENUINE_FMS_KEY`] used to key the S1
/// digest.
pub const FMS_LABEL_LEN: usize = 36;

/// Client-side handshake key: the 30-byte ASCII label
/// `"Genuine Adobe Flash Player 001"` + the common 32-byte suffix.
/// C1 digests are keyed by the label alone (`[..FP_LABEL_LEN]`); C2
/// response digests chain through the full 62 bytes.
pub const GENUINE_FP_KEY: [u8; FP_LABEL_LEN + 32] = {
    let mut k = [0u8; FP_LABEL_LEN + 32];
    let label = *b"Genuine Adobe Flash Player 001";
    let mut i = 0;
    while i < FP_LABEL_LEN {
        k[i] = label[i];
        i += 1;
    }
    let mut j = 0;
    while j < 32 {
        k[FP_LABEL_LEN + j] = KEY_SUFFIX[j];
        j += 1;
    }
    k
};

/// Server-side handshake key: the 36-byte ASCII label
/// `"Genuine Adobe Flash Media Server 001"` + the common 32-byte
/// suffix. S1 digests are keyed by the label alone
/// (`[..FMS_LABEL_LEN]`); S2 response digests chain through the full
/// 68 bytes.
pub const GENUINE_FMS_KEY: [u8; FMS_LABEL_LEN + 32] = {
    let mut k = [0u8; FMS_LABEL_LEN + 32];
    let label = *b"Genuine Adobe Flash Media Server 001";
    let mut i = 0;
    while i < FMS_LABEL_LEN {
        k[i] = label[i];
        i += 1;
    }
    let mut j = 0;
    while j < 32 {
        k[FMS_LABEL_LEN + j] = KEY_SUFFIX[j];
        j += 1;
    }
    k
};

/// Which of the two block orders a digested C1/S1 uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DigestScheme {
    /// Key block first (bytes 8..772), digest block second (772..1536).
    Schema0,
    /// Digest block first (bytes 8..772), key block second (772..1536).
    Schema1,
}

impl DigestScheme {
    /// Absolute offset of this scheme's 764-byte digest block within
    /// the 1536-byte packet.
    pub fn digest_block_start(self) -> usize {
        match self {
            DigestScheme::Schema0 => 8 + BLOCK_LEN,
            DigestScheme::Schema1 => 8,
        }
    }

    /// Absolute offset of this scheme's 764-byte key block within the
    /// 1536-byte packet.
    pub fn key_block_start(self) -> usize {
        match self {
            DigestScheme::Schema0 => 8,
            DigestScheme::Schema1 => 8 + BLOCK_LEN,
        }
    }
}

/// Which handshake flavour a negotiated exchange settled on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandshakeKind {
    /// Plain echo handshake — C2/S2 mirror the peer's first packet.
    Simple,
    /// Digest handshake.
    Digest {
        /// Block order of the digested packet this side *received*.
        scheme: DigestScheme,
        /// Whether the peer's C2/S2 response digest chained correctly
        /// from the digest we embedded in our own C1/S1. `false` means
        /// the exchange still completed byte-count-wise but the peer
        /// did not (or could not) prove it ran the digest derivation —
        /// callers doing authentication gating should treat it as
        /// simple-handshake trust.
        peer_response_verified: bool,
    },
}

/// Absolute offset of the 32-byte digest inside a 1536-byte C1/S1
/// under `scheme`: the digest block's first 4 bytes sum to an offset
/// mod 728, and the digest follows those 4 bytes + that many random
/// bytes.
pub fn digest_offset(packet: &[u8], scheme: DigestScheme) -> usize {
    assert_eq!(packet.len(), HANDSHAKE_PAYLOAD_LEN);
    let bs = scheme.digest_block_start();
    let sum = packet[bs] as usize
        + packet[bs + 1] as usize
        + packet[bs + 2] as usize
        + packet[bs + 3] as usize;
    bs + 4 + (sum % DIGEST_OFFSET_MOD)
}

/// Absolute offset of the 128-byte DH public key inside a 1536-byte
/// C1/S1 under `scheme`: the key block's *last* 4 bytes sum to an
/// offset mod 632 from the block start.
pub fn key_offset(packet: &[u8], scheme: DigestScheme) -> usize {
    assert_eq!(packet.len(), HANDSHAKE_PAYLOAD_LEN);
    let bs = scheme.key_block_start();
    let tail = bs + BLOCK_LEN - 4;
    let sum = packet[tail] as usize
        + packet[tail + 1] as usize
        + packet[tail + 2] as usize
        + packet[tail + 3] as usize;
    bs + (sum % KEY_OFFSET_MOD)
}

/// The 1504-byte HMAC message for a digest at `pos`: the packet with
/// its 32 digest bytes excised.
fn digest_message(packet: &[u8], pos: usize) -> Vec<u8> {
    let mut m = Vec::with_capacity(HANDSHAKE_PAYLOAD_LEN - HANDSHAKE_DIGEST_LEN);
    m.extend_from_slice(&packet[..pos]);
    m.extend_from_slice(&packet[pos + HANDSHAKE_DIGEST_LEN..]);
    m
}

/// Compute and write the digest for an outgoing C1/S1. `key` is the
/// label prefix — `&GENUINE_FP_KEY[..FP_LABEL_LEN]` for C1,
/// `&GENUINE_FMS_KEY[..FMS_LABEL_LEN]` for S1. Returns the 32 digest
/// bytes (needed later to verify the peer's response packet).
pub fn install_digest(
    packet: &mut [u8],
    scheme: DigestScheme,
    key: &[u8],
) -> [u8; HANDSHAKE_DIGEST_LEN] {
    let pos = digest_offset(packet, scheme);
    let digest = hmac_sha256(key, &digest_message(packet, pos));
    packet[pos..pos + HANDSHAKE_DIGEST_LEN].copy_from_slice(&digest);
    digest
}

/// Validate the digest of an incoming C1/S1 under one specific
/// scheme. Returns the digest bytes when they match the recomputed
/// HMAC, `None` otherwise.
pub fn verify_digest(
    packet: &[u8],
    scheme: DigestScheme,
    key: &[u8],
) -> Option<[u8; HANDSHAKE_DIGEST_LEN]> {
    let pos = digest_offset(packet, scheme);
    let expect = hmac_sha256(key, &digest_message(packet, pos));
    if packet[pos..pos + HANDSHAKE_DIGEST_LEN] == expect {
        Some(expect)
    } else {
        None
    }
}

/// Detect which scheme an incoming digested C1/S1 uses by trying
/// schema 0 first, then schema 1 (the documented peer-side detection
/// order). `None` = neither validates (peer is doing the simple
/// handshake, or keyed differently).
pub fn find_digest(
    packet: &[u8],
    key: &[u8],
) -> Option<(DigestScheme, [u8; HANDSHAKE_DIGEST_LEN])> {
    for scheme in [DigestScheme::Schema0, DigestScheme::Schema1] {
        if let Some(d) = verify_digest(packet, scheme, key) {
            return Some((scheme, d));
        }
    }
    None
}

/// Turn a random-filled 1536-byte C2/S2 into a digest response: the
/// last 32 bytes become
/// `HMAC(key = HMAC(full_key, peer_digest), message = packet[..1504])`.
/// `full_key` is all 62 bytes of [`GENUINE_FP_KEY`] when building C2,
/// all 68 bytes of [`GENUINE_FMS_KEY`] when building S2.
pub fn install_response_digest(
    packet: &mut [u8],
    peer_digest: &[u8; HANDSHAKE_DIGEST_LEN],
    full_key: &[u8],
) {
    let tmp = hmac_sha256(full_key, peer_digest);
    let tail = HANDSHAKE_PAYLOAD_LEN - HANDSHAKE_DIGEST_LEN;
    let d = hmac_sha256(&tmp, &packet[..tail]);
    packet[tail..].copy_from_slice(&d);
}

/// Verify the peer's C2/S2 against the digest we embedded in our own
/// C1/S1. `full_key` is the *peer's* full key constant — all 68 bytes
/// of [`GENUINE_FMS_KEY`] when checking an S2, all 62 bytes of
/// [`GENUINE_FP_KEY`] when checking a C2.
pub fn verify_response_digest(
    packet: &[u8],
    own_digest: &[u8; HANDSHAKE_DIGEST_LEN],
    full_key: &[u8],
) -> bool {
    let tmp = hmac_sha256(full_key, own_digest);
    let tail = HANDSHAKE_PAYLOAD_LEN - HANDSHAKE_DIGEST_LEN;
    let expect = hmac_sha256(&tmp, &packet[..tail]);
    packet[tail..] == expect
}

/// Build a digested C1/S1: `time | version | random`, then the digest
/// installed per `scheme` and `key` (a label prefix).
fn build_digest_packet(
    version: u32,
    scheme: DigestScheme,
    key: &[u8],
    seed: u64,
) -> ([u8; HANDSHAKE_PAYLOAD_LEN], [u8; HANDSHAKE_DIGEST_LEN]) {
    let mut pkt = [0u8; HANDSHAKE_PAYLOAD_LEN];
    pkt[0..4].copy_from_slice(&now_secs().to_be_bytes());
    pkt[4..8].copy_from_slice(&version.to_be_bytes());
    fill_random(&mut pkt[8..], seed);
    let digest = install_digest(&mut pkt, scheme, key);
    (pkt, digest)
}

/// Client side of the digest handshake, with automatic fallback: we
/// always emit a digested C1 (schema per `scheme`; peers detect either
/// order); if the server's S1 carries a valid digest we complete the
/// digest exchange and verify its S2 response, otherwise we degrade to
/// the plain echo exchange. Returns which flavour was completed.
pub fn client_handshake_digest<S: Read + Write>(
    stream: &mut S,
    scheme: DigestScheme,
) -> Result<HandshakeKind> {
    let seed = now_secs() as u64;
    let (c1, c1_digest) = build_digest_packet(
        HANDSHAKE_CLIENT_VERSION,
        scheme,
        &GENUINE_FP_KEY[..FP_LABEL_LEN],
        seed,
    );
    let mut c0c1 = [0u8; 1 + HANDSHAKE_PAYLOAD_LEN];
    c0c1[0] = RTMP_VERSION;
    c0c1[1..].copy_from_slice(&c1);
    stream.write_all(&c0c1)?;

    let mut s0 = [0u8; 1];
    stream.read_exact(&mut s0)?;
    if s0[0] != RTMP_VERSION {
        return Err(Error::UnsupportedHandshakeVersion(s0[0]));
    }
    let mut s1 = [0u8; HANDSHAKE_PAYLOAD_LEN];
    stream.read_exact(&mut s1)?;

    match find_digest(&s1, &GENUINE_FMS_KEY[..FMS_LABEL_LEN]) {
        Some((s1_scheme, s1_digest)) => {
            // Digest server: C2 = fresh random + chained response
            // digest derived from the S1 digest.
            let mut c2 = [0u8; HANDSHAKE_PAYLOAD_LEN];
            fill_random(&mut c2, seed ^ 0x00C2_00C2_00C2_00C2);
            install_response_digest(&mut c2, &s1_digest, &GENUINE_FP_KEY);
            stream.write_all(&c2)?;

            let mut s2 = [0u8; HANDSHAKE_PAYLOAD_LEN];
            stream.read_exact(&mut s2)?;
            let verified = verify_response_digest(&s2, &c1_digest, &GENUINE_FMS_KEY);
            Ok(HandshakeKind::Digest {
                scheme: s1_scheme,
                peer_response_verified: verified,
            })
        }
        None => {
            // Simple server: echo S1 back, drain S2.
            stream.write_all(&s1)?;
            let mut s2 = [0u8; HANDSHAKE_PAYLOAD_LEN];
            stream.read_exact(&mut s2)?;
            Ok(HandshakeKind::Simple)
        }
    }
}

/// Server side of the handshake with digest auto-detection.
///
/// * C1 with a zero version field (or a version field that fails
///   digest detection under both schemas — e.g. junk filler) → plain
///   echo exchange, returns [`HandshakeKind::Simple`].
/// * C1 with a validating digest → digested S1 (same schema the client
///   picked) + chained-response S2, then the client's C2 response
///   digest is checked (a plain-echo C2 completes the exchange but is
///   reported through the `peer_response_verified` field of
///   [`HandshakeKind::Digest`] as unverified).
pub fn server_handshake_negotiated<S: Read + Write>(stream: &mut S) -> Result<HandshakeKind> {
    let mut c0 = [0u8; 1];
    stream.read_exact(&mut c0)?;
    if c0[0] != RTMP_VERSION {
        return Err(Error::UnsupportedHandshakeVersion(c0[0]));
    }
    let mut c1 = [0u8; HANDSHAKE_PAYLOAD_LEN];
    stream.read_exact(&mut c1)?;

    let client_version = u32::from_be_bytes([c1[4], c1[5], c1[6], c1[7]]);
    let detected = if client_version == 0 {
        None
    } else {
        find_digest(&c1, &GENUINE_FP_KEY[..FP_LABEL_LEN])
    };

    match detected {
        None => {
            // Simple path: S0 + random S1 + S2 = echo of C1.
            let mut s0s1s2 = [0u8; 1 + HANDSHAKE_PAYLOAD_LEN * 2];
            s0s1s2[0] = RTMP_VERSION;
            fill_outgoing_payload(&mut s0s1s2[1..1 + HANDSHAKE_PAYLOAD_LEN]);
            s0s1s2[1 + HANDSHAKE_PAYLOAD_LEN..].copy_from_slice(&c1);
            stream.write_all(&s0s1s2)?;

            let mut c2 = [0u8; HANDSHAKE_PAYLOAD_LEN];
            stream.read_exact(&mut c2)?;
            Ok(HandshakeKind::Simple)
        }
        Some((scheme, c1_digest)) => {
            let seed = (now_secs() as u64) ^ 0x5EED_0000_0000_5EED;
            let (s1, s1_digest) = build_digest_packet(
                HANDSHAKE_SERVER_VERSION,
                scheme,
                &GENUINE_FMS_KEY[..FMS_LABEL_LEN],
                seed,
            );
            let mut s2 = [0u8; HANDSHAKE_PAYLOAD_LEN];
            fill_random(&mut s2, seed ^ 0x0052_0052_0052_0052);
            install_response_digest(&mut s2, &c1_digest, &GENUINE_FMS_KEY);

            let mut out = [0u8; 1 + HANDSHAKE_PAYLOAD_LEN * 2];
            out[0] = RTMP_VERSION;
            out[1..1 + HANDSHAKE_PAYLOAD_LEN].copy_from_slice(&s1);
            out[1 + HANDSHAKE_PAYLOAD_LEN..].copy_from_slice(&s2);
            stream.write_all(&out)?;

            let mut c2 = [0u8; HANDSHAKE_PAYLOAD_LEN];
            stream.read_exact(&mut c2)?;
            let verified = verify_response_digest(&c2, &s1_digest, &GENUINE_FP_KEY);
            Ok(HandshakeKind::Digest {
                scheme,
                peer_response_verified: verified,
            })
        }
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

    // ---------------- digest handshake unit coverage ----------------

    /// The key constants must be exactly label + shared 32-byte suffix.
    #[test]
    fn digest_key_constants_shape() {
        assert_eq!(GENUINE_FP_KEY.len(), 62);
        assert_eq!(GENUINE_FMS_KEY.len(), 68);
        assert_eq!(
            &GENUINE_FP_KEY[..FP_LABEL_LEN],
            b"Genuine Adobe Flash Player 001"
        );
        assert_eq!(
            &GENUINE_FMS_KEY[..FMS_LABEL_LEN],
            b"Genuine Adobe Flash Media Server 001"
        );
        assert_eq!(
            GENUINE_FP_KEY[FP_LABEL_LEN..],
            GENUINE_FMS_KEY[FMS_LABEL_LEN..]
        );
    }

    /// Worst-case offset bytes must keep both slots inside their
    /// 764-byte block: digest max 4 + 727 + 32 = 763, key max
    /// 631 + 128 = 759 (+ trailing random + 4 offset bytes).
    #[test]
    fn digest_and_key_offsets_stay_in_bounds() {
        for scheme in [DigestScheme::Schema0, DigestScheme::Schema1] {
            let mut pkt = [0xFFu8; HANDSHAKE_PAYLOAD_LEN];
            let dpos = digest_offset(&pkt, scheme);
            let dblock = scheme.digest_block_start();
            assert!(dpos >= dblock + 4);
            assert!(dpos + HANDSHAKE_DIGEST_LEN <= dblock + 764);

            let kpos = key_offset(&pkt, scheme);
            let kblock = scheme.key_block_start();
            assert!(kpos >= kblock);
            assert!(kpos + HANDSHAKE_DH_KEY_LEN <= kblock + 764 - 4);

            // Zero offsets land at the block starts.
            pkt.fill(0);
            assert_eq!(digest_offset(&pkt, scheme), dblock + 4);
            assert_eq!(key_offset(&pkt, scheme), kblock);
        }
    }

    /// The documented modulus arithmetic: offset bytes summing past
    /// the modulus wrap.
    #[test]
    fn digest_offset_wraps_mod_728() {
        let scheme = DigestScheme::Schema1;
        let bs = scheme.digest_block_start();
        let mut pkt = [0u8; HANDSHAKE_PAYLOAD_LEN];
        // Sum = 200 + 200 + 200 + 200 = 800 → 800 mod 728 = 72.
        pkt[bs..bs + 4].copy_from_slice(&[200, 200, 200, 200]);
        assert_eq!(digest_offset(&pkt, scheme), bs + 4 + 72);
    }

    #[test]
    fn install_then_verify_round_trips_both_schemas() {
        for scheme in [DigestScheme::Schema0, DigestScheme::Schema1] {
            let (pkt, digest) = build_digest_packet(
                HANDSHAKE_CLIENT_VERSION,
                scheme,
                &GENUINE_FP_KEY[..FP_LABEL_LEN],
                0x1234,
            );
            let (found_scheme, found) =
                find_digest(&pkt, &GENUINE_FP_KEY[..FP_LABEL_LEN]).expect("digest detected");
            assert_eq!(found_scheme, scheme);
            assert_eq!(found, digest);
            // The wrong label key must not validate.
            assert!(find_digest(&pkt, &GENUINE_FMS_KEY[..FMS_LABEL_LEN]).is_none());
        }
    }

    #[test]
    fn tampered_packet_fails_digest_detection() {
        let (mut pkt, _) = build_digest_packet(
            HANDSHAKE_CLIENT_VERSION,
            DigestScheme::Schema1,
            &GENUINE_FP_KEY[..FP_LABEL_LEN],
            0x5678,
        );
        // Flip one byte outside the digest slot (the timestamp).
        pkt[0] ^= 0x80;
        assert!(find_digest(&pkt, &GENUINE_FP_KEY[..FP_LABEL_LEN]).is_none());
    }

    #[test]
    fn response_digest_round_trips_and_rejects_wrong_chain() {
        let (_, c1_digest) = build_digest_packet(
            HANDSHAKE_CLIENT_VERSION,
            DigestScheme::Schema1,
            &GENUINE_FP_KEY[..FP_LABEL_LEN],
            0x9abc,
        );
        let mut s2 = [0u8; HANDSHAKE_PAYLOAD_LEN];
        fill_random(&mut s2, 0xdef0);
        install_response_digest(&mut s2, &c1_digest, &GENUINE_FMS_KEY);
        assert!(verify_response_digest(&s2, &c1_digest, &GENUINE_FMS_KEY));
        // Chained from a different digest → must fail.
        let other = [0u8; HANDSHAKE_DIGEST_LEN];
        assert!(!verify_response_digest(&s2, &other, &GENUINE_FMS_KEY));
        // Verified with the wrong side's key → must fail.
        assert!(!verify_response_digest(&s2, &c1_digest, &GENUINE_FP_KEY));
    }

    /// A digest client facing a plain echo server must fall back to
    /// the simple exchange (drive the client against a synthetic
    /// echo-server transcript).
    #[test]
    fn digest_client_falls_back_against_echo_server() {
        // The echo server sends S0 + a random zero-version S1 + S2
        // (echo of C1 — content irrelevant, the client only drains it).
        let mut s1 = vec![0u8; HANDSHAKE_PAYLOAD_LEN];
        fill_random(&mut s1[8..], 0x4242);
        let mut server_to_client = Vec::new();
        server_to_client.push(RTMP_VERSION);
        server_to_client.extend_from_slice(&s1);
        server_to_client.extend_from_slice(&[0u8; HANDSHAKE_PAYLOAD_LEN]);

        let mut duplex = DuplexBuf {
            read: Cursor::new(server_to_client),
            write: Vec::new(),
        };
        let kind = client_handshake_digest(&mut duplex, DigestScheme::Schema1).expect("handshake");
        assert_eq!(kind, HandshakeKind::Simple);
        // C0 + C1 + C2, C2 = echo of S1.
        assert_eq!(duplex.write.len(), 1 + 2 * HANDSHAKE_PAYLOAD_LEN);
        assert_eq!(&duplex.write[1 + HANDSHAKE_PAYLOAD_LEN..], &s1[..]);
        // C1 carries the digest-client version field and a validating
        // digest.
        let c1 = &duplex.write[1..1 + HANDSHAKE_PAYLOAD_LEN];
        assert_eq!(
            u32::from_be_bytes([c1[4], c1[5], c1[6], c1[7]]),
            HANDSHAKE_CLIENT_VERSION
        );
        let (scheme, _) =
            find_digest(c1, &GENUINE_FP_KEY[..FP_LABEL_LEN]).expect("C1 digest present");
        assert_eq!(scheme, DigestScheme::Schema1);
    }

    /// A simple (zero-version) C1 through the negotiated server takes
    /// the echo path.
    #[test]
    fn negotiated_server_answers_simple_client_with_echo() {
        let mut c1 = vec![0u8; HANDSHAKE_PAYLOAD_LEN];
        fill_random(&mut c1[8..], 0x7777);
        let mut client_to_server = Vec::new();
        client_to_server.push(RTMP_VERSION);
        client_to_server.extend_from_slice(&c1);
        client_to_server.extend_from_slice(&[0u8; HANDSHAKE_PAYLOAD_LEN]); // C2 drain

        let mut duplex = DuplexBuf {
            read: Cursor::new(client_to_server),
            write: Vec::new(),
        };
        let kind = server_handshake_negotiated(&mut duplex).expect("handshake");
        assert_eq!(kind, HandshakeKind::Simple);
        // S0 + S1 + S2, S2 = echo of C1.
        assert_eq!(duplex.write.len(), 1 + 2 * HANDSHAKE_PAYLOAD_LEN);
        assert_eq!(&duplex.write[1 + HANDSHAKE_PAYLOAD_LEN..], &c1[..]);
    }

    /// A junk (non-zero, non-digest) version field must also fall back
    /// to the echo path rather than erroring out.
    #[test]
    fn negotiated_server_tolerates_junk_version_field() {
        let mut c1 = vec![0u8; HANDSHAKE_PAYLOAD_LEN];
        fill_random(&mut c1[8..], 0x8888);
        c1[4..8].copy_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]);
        let mut client_to_server = Vec::new();
        client_to_server.push(RTMP_VERSION);
        client_to_server.extend_from_slice(&c1);
        client_to_server.extend_from_slice(&[0u8; HANDSHAKE_PAYLOAD_LEN]);

        let mut duplex = DuplexBuf {
            read: Cursor::new(client_to_server),
            write: Vec::new(),
        };
        let kind = server_handshake_negotiated(&mut duplex).expect("handshake");
        assert_eq!(kind, HandshakeKind::Simple);
        assert_eq!(&duplex.write[1 + HANDSHAKE_PAYLOAD_LEN..], &c1[..]);
    }

    /// Digest C1 through the negotiated server: S1 digested under the
    /// same schema, S2 chained from the C1 digest, and a proper C2
    /// response reported as verified.
    #[test]
    fn negotiated_server_digest_exchange_verifies() {
        for scheme in [DigestScheme::Schema0, DigestScheme::Schema1] {
            let (c1, c1_digest) = build_digest_packet(
                HANDSHAKE_CLIENT_VERSION,
                scheme,
                &GENUINE_FP_KEY[..FP_LABEL_LEN],
                0xAAAA,
            );

            // First pass: capture S1 so we can chain a genuine C2.
            let mut probe = Vec::new();
            probe.push(RTMP_VERSION);
            probe.extend_from_slice(&c1);
            probe.extend_from_slice(&[0u8; HANDSHAKE_PAYLOAD_LEN]); // junk C2
            let mut duplex = DuplexBuf {
                read: Cursor::new(probe),
                write: Vec::new(),
            };
            let kind = server_handshake_negotiated(&mut duplex).expect("handshake");
            assert_eq!(
                kind,
                HandshakeKind::Digest {
                    scheme,
                    peer_response_verified: false
                }
            );
            let s1 = duplex.write[1..1 + HANDSHAKE_PAYLOAD_LEN].to_vec();
            let s2 = duplex.write[1 + HANDSHAKE_PAYLOAD_LEN..].to_vec();
            let (s1_scheme, s1_digest) =
                find_digest(&s1, &GENUINE_FMS_KEY[..FMS_LABEL_LEN]).expect("S1 digest");
            assert_eq!(s1_scheme, scheme, "server mirrors the client's schema");
            assert!(
                verify_response_digest(&s2, &c1_digest, &GENUINE_FMS_KEY),
                "S2 must chain from the C1 digest"
            );

            // Second pass: same C1 (deterministic PRNG) + genuine C2.
            let mut c2 = [0u8; HANDSHAKE_PAYLOAD_LEN];
            fill_random(&mut c2, 0xBBBB);
            install_response_digest(&mut c2, &s1_digest, &GENUINE_FP_KEY);
            let mut replay = Vec::new();
            replay.push(RTMP_VERSION);
            replay.extend_from_slice(&c1);
            replay.extend_from_slice(&c2);
            let mut duplex2 = DuplexBuf {
                read: Cursor::new(replay),
                write: Vec::new(),
            };
            let kind2 = server_handshake_negotiated(&mut duplex2).expect("handshake");
            // The server's S1 seed is wall-clock based; within the same
            // second the S1 (and thus its digest) matches the probe
            // pass, letting the chained C2 verify. Guard against the
            // rare second-boundary flake by accepting either outcome
            // when S1 changed.
            let s1_replay = &duplex2.write[1..1 + HANDSHAKE_PAYLOAD_LEN];
            if s1_replay == &s1[..] {
                assert_eq!(
                    kind2,
                    HandshakeKind::Digest {
                        scheme,
                        peer_response_verified: true
                    }
                );
            } else {
                assert!(matches!(kind2, HandshakeKind::Digest { .. }));
            }
        }
    }
}
