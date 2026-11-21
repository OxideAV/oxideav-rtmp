#![no_main]

//! Handshake: the fuzz input plays the peer in all four roles (simple
//! client, digest client under both block orders, negotiating server),
//! and — when it is long enough — is also treated as a raw C1/S1
//! packet for the digest helpers. Contract: never panics, digest and
//! key offsets always land inside the packet.

use std::io::{Cursor, Read, Write};

use libfuzzer_sys::fuzz_target;
use oxideav_rtmp::handshake::{
    DigestScheme, FMS_LABEL_LEN, FP_LABEL_LEN, GENUINE_FMS_KEY, GENUINE_FP_KEY,
    HANDSHAKE_DH_KEY_LEN, HANDSHAKE_DIGEST_LEN, HANDSHAKE_PAYLOAD_LEN, client_handshake,
    client_handshake_digest, digest_offset, find_digest, install_digest, install_response_digest,
    key_offset, server_handshake, server_handshake_negotiated, verify_digest,
    verify_response_digest,
};

/// The peer's bytes on the read side, a sink on the write side.
struct Peer<'a> {
    input: Cursor<&'a [u8]>,
    written: usize,
}

impl Read for Peer<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.input.read(buf)
    }
}

impl Write for Peer<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.written += buf.len();
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn peer(data: &[u8]) -> Peer<'_> {
    Peer {
        input: Cursor::new(data),
        written: 0,
    }
}

fuzz_target!(|data: &[u8]| {
    let _ = client_handshake(&mut peer(data));
    let _ = client_handshake_digest(&mut peer(data), DigestScheme::Schema0);
    let _ = client_handshake_digest(&mut peer(data), DigestScheme::Schema1);
    let _ = server_handshake(&mut peer(data));
    let mut p = peer(data);
    if server_handshake_negotiated(&mut p).is_ok() {
        // A completed server handshake wrote exactly S0 + S1 + S2.
        assert_eq!(p.written, 1 + 2 * HANDSHAKE_PAYLOAD_LEN);
    }

    if data.len() >= HANDSHAKE_PAYLOAD_LEN {
        let mut packet = [0u8; HANDSHAKE_PAYLOAD_LEN];
        packet.copy_from_slice(&data[..HANDSHAKE_PAYLOAD_LEN]);
        for scheme in [DigestScheme::Schema0, DigestScheme::Schema1] {
            let d = digest_offset(&packet, scheme);
            assert!(d + HANDSHAKE_DIGEST_LEN <= HANDSHAKE_PAYLOAD_LEN);
            let k = key_offset(&packet, scheme);
            assert!(k + HANDSHAKE_DH_KEY_LEN <= HANDSHAKE_PAYLOAD_LEN);
            let _ = verify_digest(&packet, scheme, &GENUINE_FP_KEY[..FP_LABEL_LEN]);
            // Installing a digest makes the packet verify under the
            // same scheme and key, and detection finds it.
            let mut signed = packet;
            let digest = install_digest(&mut signed, scheme, &GENUINE_FMS_KEY[..FMS_LABEL_LEN]);
            assert_eq!(
                verify_digest(&signed, scheme, &GENUINE_FMS_KEY[..FMS_LABEL_LEN]),
                Some(digest)
            );
            assert!(find_digest(&signed, &GENUINE_FMS_KEY[..FMS_LABEL_LEN]).is_some());
            let mut response = packet;
            install_response_digest(&mut response, &digest, &GENUINE_FP_KEY);
            assert!(verify_response_digest(&response, &digest, &GENUINE_FP_KEY));
        }
        let _ = find_digest(&packet, &GENUINE_FP_KEY[..FP_LABEL_LEN]);
    }
});
