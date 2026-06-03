//! Injection-robustness property tests.
//!
//! Every public parser surface fed an *arbitrary* byte string must
//! return a `Result`, never:
//!
//! * panic / index out of bounds / unwrap on `None`,
//! * stack-overflow on a forged deeply-nested container,
//! * over-allocate (`Vec::with_capacity` taking a 4-billion request
//!   straight from the wire),
//! * spin forever consuming zero bytes per iteration.
//!
//! These tests use a deterministic xorshift PRNG (no `rand` dep) so
//! every CI run exercises the same byte streams. The seeds cover both
//! "random noise" and "structured-but-corrupted RTMP" inputs — the
//! latter built by mutating valid frames in place.
//!
//! Wall: this file reads only the crate's own public API + standard
//! library. No cross-crate deps, no external library source.

use std::io::Cursor;

use oxideav_rtmp::aggregate::parse_aggregate;
use oxideav_rtmp::amf::{decode as amf0_decode, decode_all as amf0_decode_all, Amf0Value};
use oxideav_rtmp::amf3::{
    decode as amf3_decode, decode_all as amf3_decode_all, decode_data_message,
};
use oxideav_rtmp::chunk::{ChunkReader, ChunkWriter, Message};
use oxideav_rtmp::flv::{parse_audio, parse_video};
use oxideav_rtmp::handshake::{client_handshake, server_handshake};

// ---------------------------------------------------------------------------
// Deterministic PRNG — xorshift64*.
//
// We avoid `rand` to keep the crate dep-free (oxideav-core is the only
// dep allowed by the workspace) and we want reproducible CI: each test
// hard-codes its seed.
// ---------------------------------------------------------------------------

struct Xs64 {
    state: u64,
}

impl Xs64 {
    fn new(seed: u64) -> Self {
        // xorshift breaks on a zero state — bias the seed up.
        Self {
            state: seed | 0x9E37_79B9_7F4A_7C15,
        }
    }
    fn next(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.state = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn fill(&mut self, buf: &mut [u8]) {
        for chunk in buf.chunks_mut(8) {
            let v = self.next().to_le_bytes();
            let n = chunk.len();
            chunk.copy_from_slice(&v[..n]);
        }
    }
}

// ---------------------------------------------------------------------------
// AMF0 fuzz — random bytes through decode_all
// ---------------------------------------------------------------------------

/// Feed 1024 random byte strings of varying length to `amf0::decode_all`
/// and assert that every call either returns `Ok` or `Err` — never
/// panics. Each iteration is bounded in size (max 256 bytes) to keep
/// the total runtime well under a second.
#[test]
fn amf0_decode_all_random_bytes_never_panics() {
    let mut rng = Xs64::new(0xA110_C0DE_0001);
    for iter in 0..1024 {
        let len = (rng.next() as usize) % 257;
        let mut buf = vec![0u8; len];
        rng.fill(&mut buf);
        // Wrap in catch_unwind so a regression is reported per-iteration
        // rather than aborting the whole test process — better signal.
        let buf_copy = buf.clone();
        let result = std::panic::catch_unwind(move || amf0_decode_all(&buf_copy));
        assert!(
            result.is_ok(),
            "amf0_decode_all panicked on iteration {iter}, input bytes: {buf:?}"
        );
    }
}

/// Feed 1024 random byte strings to `amf0::decode` directly (single
/// value, no exhaustion). Same guarantee.
#[test]
fn amf0_decode_single_random_bytes_never_panics() {
    let mut rng = Xs64::new(0xA110_C0DE_0002);
    for iter in 0..1024 {
        let len = (rng.next() as usize) % 129;
        let mut buf = vec![0u8; len];
        rng.fill(&mut buf);
        let buf_copy = buf.clone();
        let result = std::panic::catch_unwind(move || {
            let mut pos = 0;
            amf0_decode(&buf_copy, &mut pos)
        });
        assert!(
            result.is_ok(),
            "amf0_decode panicked on iteration {iter}, input bytes: {buf:?}"
        );
    }
}

/// A forged AMF0 frame can nest Object inside Object indefinitely. The
/// decode_at_depth guard must trip and return Err well before the call
/// stack runs out — concretely, before MAX_DECODE_DEPTH + a small
/// margin's worth of nested Objects can succeed.
#[test]
fn amf0_deeply_nested_object_returns_error_not_stack_overflow() {
    // Marker layout per AMF0 spec:
    //   0x03 = Object, then [u16-len][bytes] key, then nested value.
    //   Empty key + 0x09 = Object end.
    //
    // Build an object that opens N levels deep without ever closing
    // any of them. Each level adds 5 bytes: 0x03, 0x00, 0x01, b'a',
    // <next-marker-byte>. A flat 2_000 levels far exceeds the 64-level
    // depth guard and would blow a default 8 MiB stack in release mode
    // without it.
    let depth = 2_000usize;
    let mut buf = Vec::with_capacity(depth * 5 + 10);
    for _ in 0..depth {
        buf.push(0x03); // Object marker
        buf.extend_from_slice(&1u16.to_be_bytes()); // key len 1
        buf.push(b'a'); // key
    }
    // Innermost value is Null (so the cursor would otherwise complete).
    buf.push(0x05);
    // Close out all the objects (these bytes will never be reached
    // because the guard trips first).
    for _ in 0..depth {
        buf.extend_from_slice(&0u16.to_be_bytes());
        buf.push(0x09);
    }
    let result = amf0_decode_all(&buf);
    assert!(
        result.is_err(),
        "expected depth-guard error, got Ok with {:?}",
        result.ok().map(|v| v.len())
    );
}

/// `M_STRICT_ARRAY` advertises a `u32` count up front. A forged count
/// of `u32::MAX` paired with an empty body must error fast (each
/// element decode reads at least one byte and errors immediately) —
/// never allocate 4 GiB and never spin.
#[test]
fn amf0_strict_array_with_huge_count_errors_fast() {
    // 0x0A = strict-array marker, then 4-byte BE count = u32::MAX,
    // then... nothing. Each iteration must fail-fast on the first
    // missing element byte.
    let mut buf = vec![0x0A];
    buf.extend_from_slice(&u32::MAX.to_be_bytes());
    let start = std::time::Instant::now();
    let result = amf0_decode_all(&buf);
    let elapsed = start.elapsed();
    assert!(result.is_err());
    // 4 GiB allocation would dwarf this; even loop-on-error per
    // iteration would take seconds.
    assert!(
        elapsed.as_millis() < 100,
        "strict-array decode should fail in microseconds, took {:?}",
        elapsed
    );
}

/// `M_STRING` uses a `u16` length — bounded — but a forged value can
/// still claim 65535 bytes from a 3-byte buffer. The decoder must
/// surface a truncated-string error and not allocate the 64 KiB up
/// front.
#[test]
fn amf0_string_with_oversize_length_returns_truncated_error() {
    let buf = [0x02u8, 0xFF, 0xFF];
    let result = amf0_decode_all(&buf);
    assert!(result.is_err());
}

// ---------------------------------------------------------------------------
// AMF3 fuzz — same shape, exercises a wider marker set + reference tables
// ---------------------------------------------------------------------------

#[test]
fn amf3_decode_random_bytes_never_panics() {
    let mut rng = Xs64::new(0xA113_C0DE_0001);
    for iter in 0..1024 {
        let len = (rng.next() as usize) % 257;
        let mut buf = vec![0u8; len];
        rng.fill(&mut buf);
        let buf_copy = buf.clone();
        let result = std::panic::catch_unwind(move || {
            let mut pos = 0;
            amf3_decode(&buf_copy, &mut pos)
        });
        assert!(
            result.is_ok(),
            "amf3_decode panicked on iteration {iter}, input bytes: {buf:?}"
        );
    }
}

#[test]
fn amf3_decode_all_random_bytes_never_panics() {
    let mut rng = Xs64::new(0xA113_C0DE_0002);
    for iter in 0..1024 {
        let len = (rng.next() as usize) % 257;
        let mut buf = vec![0u8; len];
        rng.fill(&mut buf);
        let buf_copy = buf.clone();
        let result = std::panic::catch_unwind(move || amf3_decode_all(&buf_copy));
        assert!(
            result.is_ok(),
            "amf3_decode_all panicked on iteration {iter}, input bytes: {buf:?}"
        );
    }
}

/// AMF3 data-message bridge — the same input space as AMF0 commands
/// since the spec routes them through an AMF0 frame that may switch
/// markers to AMF3 via `0x11`. Must tolerate adversarial bytes.
#[test]
fn amf3_decode_data_message_random_bytes_never_panics() {
    let mut rng = Xs64::new(0xA113_C0DE_0003);
    for iter in 0..512 {
        let len = (rng.next() as usize) % 257;
        let mut buf = vec![0u8; len];
        rng.fill(&mut buf);
        let buf_copy = buf.clone();
        let result = std::panic::catch_unwind(move || decode_data_message(&buf_copy));
        assert!(
            result.is_ok(),
            "decode_data_message panicked on iteration {iter}, input bytes: {buf:?}"
        );
    }
}

/// AMF3 has the same nested-container DoS vector as AMF0 — an inline
/// anonymous Object can carry a dynamic property whose value is
/// another inline anonymous Object. The Decoder's depth field must
/// trip its guard before the stack does.
#[test]
fn amf3_deeply_nested_object_returns_error_not_stack_overflow() {
    // Per AMF3 §3.12, an inline anonymous Object marker (0x0A) with
    // header U29 = 0x0B encodes:
    //   bit 0 = 1 (inline object), bit 1 = 1 (inline traits),
    //   bit 2 = 0 (non-externalizable), bit 3 = 1 (dynamic),
    //   bits 4+ = 0 (no sealed members).
    // After the header comes the class-name string. Inline empty
    // string = U29 = 0x01 (literal flag, len 0).
    // Then the dynamic-trait loop: read a key (U29 + bytes), then a
    // value. An inline single-character key takes U29 = 0x03 ("len 1
    // literal") + 1 byte. An empty key (U29 = 0x01) terminates.
    //
    // So each nested-Object level adds: 0x0A 0x0B 0x01  0x03 'a'
    // then the value (next Object) — and we still owe a terminator
    // empty-key 0x01 per level. Build 2_000 opens, one inner Null
    // (0x01 in AMF3 is Null), and 2_000 terminators.
    let depth = 2_000usize;
    let mut buf = Vec::with_capacity(depth * 6 + 10);
    for _ in 0..depth {
        buf.push(0x0A); // Object marker
        buf.push(0x0B); // header: inline + inline traits + dynamic, 0 sealed
        buf.push(0x01); // class-name: inline literal len 0 → ""
        buf.push(0x03); // dynamic-key: inline literal len 1
        buf.push(b'a');
    }
    // Innermost value = AMF3 Null (0x01), then one terminating
    // empty dynamic-key (0x01) per opened object level.
    buf.push(0x01);
    buf.resize(buf.len() + depth, 0x01);
    let result = amf3_decode_all(&buf);
    assert!(
        result.is_err(),
        "expected AMF3 depth-guard error, got Ok with {:?}",
        result.ok().map(|v| v.len())
    );
}

// ---------------------------------------------------------------------------
// FLV tag fuzz — RTMP carries these in audio (type 8) / video (type 9) msgs
// ---------------------------------------------------------------------------

#[test]
fn flv_parse_video_random_bytes_never_panics() {
    let mut rng = Xs64::new(0x0F1F_0DE0_0001);
    for iter in 0..2048 {
        let len = (rng.next() as usize) % 65;
        let mut buf = vec![0u8; len];
        rng.fill(&mut buf);
        let buf_copy = buf.clone();
        let result = std::panic::catch_unwind(move || parse_video(&buf_copy));
        assert!(
            result.is_ok(),
            "parse_video panicked on iteration {iter}, input bytes: {buf:?}"
        );
    }
}

#[test]
fn flv_parse_audio_random_bytes_never_panics() {
    let mut rng = Xs64::new(0x0F1F_0DE0_0002);
    for iter in 0..2048 {
        let len = (rng.next() as usize) % 65;
        let mut buf = vec![0u8; len];
        rng.fill(&mut buf);
        let buf_copy = buf.clone();
        let result = std::panic::catch_unwind(move || parse_audio(&buf_copy));
        assert!(
            result.is_ok(),
            "parse_audio panicked on iteration {iter}, input bytes: {buf:?}"
        );
    }
}

/// An empty payload is a particularly tempting edge case. The legacy
/// FLV tag layout requires at least one header byte; the parser must
/// return Err, never index-out-of-bounds.
#[test]
fn flv_parse_video_empty_payload_returns_error() {
    assert!(parse_video(&[]).is_err());
}

#[test]
fn flv_parse_audio_empty_payload_returns_error() {
    assert!(parse_audio(&[]).is_err());
}

// ---------------------------------------------------------------------------
// Chunk-reader fuzz — feeds raw bytes through ChunkReader::read_message
// ---------------------------------------------------------------------------

/// The chunk-stream reader must tolerate any input on its underlying
/// `Read` — adversarial peers can send malformed basic headers, claim
/// extended timestamps, or forge fmt-1/2/3 chunks without a prior
/// fmt-0 establishing the stream state. None of those may panic.
///
/// We bound the payload by setting an aggressive chunk-size cap on
/// the reader so the worst case (`msg_length` = 24-bit max ≈ 16 MiB,
/// chunk size = 16 MiB) cannot trigger a multi-GB allocation in the
/// test process.
#[test]
fn chunk_reader_random_bytes_never_panics() {
    let mut rng = Xs64::new(0xC4F0_FFEE_0001);
    for iter in 0..512 {
        let len = (rng.next() as usize) % 257;
        let mut buf = vec![0u8; len];
        rng.fill(&mut buf);
        let buf_copy = buf.clone();
        let result = std::panic::catch_unwind(move || {
            let mut r = ChunkReader::new(Cursor::new(buf_copy));
            // Cap allocation: chunk_size is the largest single read,
            // and read_message stops when the partial buffer fills to
            // msg_length — which here is whatever the random fmt-0
            // header claims. Capping the per-chunk allocation to 4 KiB
            // means a single read_message call allocates ≤ 4 KiB +
            // header overhead even if the wire claims 16 MiB.
            //
            // Note: we accept that for genuinely-random bytes most
            // calls Err immediately on basic-header decode. The point
            // is the no-panic guarantee, not the no-error guarantee.
            r.set_chunk_size(4096);
            let _ = r.read_message();
        });
        assert!(
            result.is_ok(),
            "ChunkReader panicked on iteration {iter}, input bytes: {buf:?}"
        );
    }
}

/// A forged fmt-0 chunk header can announce any 24-bit message length.
/// With a small chunk size, read_message allocates `chunk_size` bytes
/// per loop iteration; if the wire then runs out of bytes it must
/// return an I/O error (UnexpectedEof) — not loop forever and not
/// over-allocate up front.
#[test]
fn chunk_reader_oversize_msg_length_returns_eof_error() {
    // Hand-build a single fmt-0 chunk:
    //   basic header: fmt=0, csid=3 → 0x03
    //   timestamp:  3 bytes BE = 0
    //   msg_length: 3 bytes BE = 0x00FFFE (almost the 24-bit max)
    //   type_id:    1 byte = 9 (video)
    //   stream_id:  4 bytes LE = 1
    // Then a single byte of payload — far short of the announced length.
    let mut wire = Vec::new();
    wire.push(0x03);
    wire.extend_from_slice(&[0x00, 0x00, 0x00]); // ts
    wire.extend_from_slice(&[0x00, 0xFF, 0xFE]); // msg_length
    wire.push(9); // type_id
    wire.extend_from_slice(&1u32.to_le_bytes()); // stream_id
    wire.push(0xAA); // one byte of "payload"

    let start = std::time::Instant::now();
    let mut r = ChunkReader::new(Cursor::new(wire));
    r.set_chunk_size(4096); // bounds per-iter allocation
    let result = r.read_message();
    let elapsed = start.elapsed();
    assert!(result.is_err());
    assert!(
        elapsed.as_millis() < 200,
        "ChunkReader should fail-fast on truncated wire, took {:?}",
        elapsed
    );
}

/// A fmt-1/2/3 chunk that arrives before any fmt-0 has primed the
/// per-csid state must surface a controlled error (`InvalidChunk`).
#[test]
fn chunk_reader_fmt1_without_prior_fmt0_returns_invalid_chunk() {
    // fmt=1, csid=3 → 0x43, then 7 bytes of fmt-1 header. Even if the
    // payload would parse, the state must reject the fmt-1.
    let mut wire = Vec::new();
    wire.push(0x43);
    wire.extend_from_slice(&[0x00, 0x00, 0x10]); // delta
    wire.extend_from_slice(&[0x00, 0x00, 0x04]); // msg_length
    wire.push(20); // type_id
    wire.extend_from_slice(&[0x00; 4]); // forged payload bytes — won't be read

    let mut r = ChunkReader::new(Cursor::new(wire));
    let result = r.read_message();
    assert!(result.is_err());
}

/// A fmt-0 chunk with `msg_length = 0` would have the reader return
/// immediately with an empty payload (`partial.len() >= 0` always true).
/// Make sure that path doesn't trip an "empty buffer" assumption
/// downstream and that two such messages can be read back-to-back.
#[test]
fn chunk_reader_zero_length_message_round_trips() {
    let msg = Message {
        msg_type_id: 20,
        msg_stream_id: 0,
        timestamp: 0,
        payload: Vec::new(),
    };
    let mut buf = Vec::new();
    {
        let mut w = ChunkWriter::new(&mut buf);
        w.write_message(3, &msg).unwrap();
        w.write_message(3, &msg).unwrap();
    }
    let mut r = ChunkReader::new(Cursor::new(&buf));
    let m1 = r.read_message().unwrap();
    let m2 = r.read_message().unwrap();
    assert_eq!(m1.payload.len(), 0);
    assert_eq!(m2.payload.len(), 0);
    assert_eq!(m1.msg_type_id, 20);
    assert_eq!(m2.msg_type_id, 20);
}

// ---------------------------------------------------------------------------
// Handshake — truncation + bad version byte
// ---------------------------------------------------------------------------

/// A duplex stream where the read side is backed by a pre-canned
/// buffer and the write side is a `Vec<u8>` sink. Lets us feed
/// adversarial bytes into the handshake without involving a TCP
/// socket.
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

/// Client handshake against an empty server reply must Err, not panic.
#[test]
fn client_handshake_truncated_server_reply_errors() {
    for trunc in [0usize, 1, 100, 1536, 1537, 1536 + 100] {
        let mut srv_bytes = Vec::new();
        srv_bytes.push(0x03);
        srv_bytes.extend_from_slice(&[0u8; 1536]);
        srv_bytes.extend_from_slice(&[0u8; 1536]);
        srv_bytes.truncate(trunc);

        let mut duplex = DuplexBuf {
            read: Cursor::new(srv_bytes),
            write: Vec::new(),
        };
        let result = client_handshake(&mut duplex);
        if trunc < 1 + 1536 + 1536 {
            assert!(result.is_err(), "trunc={trunc} should have errored");
        } else {
            assert!(result.is_ok(), "trunc={trunc} should have succeeded");
        }
    }
}

/// Wrong RTMP version byte (anything other than 0x03) → controlled
/// `UnsupportedHandshakeVersion` error from both directions.
#[test]
fn handshake_bad_version_byte_errors_cleanly() {
    for bad in [0x00u8, 0x01, 0x06, 0xFF] {
        // Client side: bad S0
        let mut srv = vec![bad];
        srv.extend_from_slice(&[0u8; 1536 * 2]);
        let mut duplex = DuplexBuf {
            read: Cursor::new(srv),
            write: Vec::new(),
        };
        assert!(client_handshake(&mut duplex).is_err());

        // Server side: bad C0
        let mut cli = vec![bad];
        cli.extend_from_slice(&[0u8; 1536 * 2]);
        let mut duplex = DuplexBuf {
            read: Cursor::new(cli),
            write: Vec::new(),
        };
        assert!(server_handshake(&mut duplex).is_err());
    }
}

/// Server handshake fed truncated bytes after a valid C0 must Err.
#[test]
fn server_handshake_truncated_client_reply_errors() {
    for trunc in [1usize, 100, 1536, 1537, 1536 + 100, 1536 + 1500] {
        let mut cli_bytes = Vec::new();
        cli_bytes.push(0x03);
        cli_bytes.extend_from_slice(&[0u8; 1536]); // C1
        cli_bytes.extend_from_slice(&[0u8; 1536]); // C2
        cli_bytes.truncate(trunc);

        let mut duplex = DuplexBuf {
            read: Cursor::new(cli_bytes),
            write: Vec::new(),
        };
        let result = server_handshake(&mut duplex);
        if trunc < 1 + 1536 + 1536 {
            assert!(result.is_err(), "trunc={trunc} should have errored");
        } else {
            assert!(result.is_ok(), "trunc={trunc} should have succeeded");
        }
    }
}

// ---------------------------------------------------------------------------
// Structured AMF0 mutation — verify a valid-then-corrupted onMetaData
// frame fails cleanly rather than emitting half-baked values.
// ---------------------------------------------------------------------------

/// Build a valid AMF0 `onMetaData` payload, then flip random bytes
/// inside it. Every mutation must produce either a valid Amf0Value
/// list or a clean Err — never a panic.
// ---------------------------------------------------------------------------
// Aggregate Message (type 22) fuzz — `aggregate::parse_aggregate`
// ---------------------------------------------------------------------------
//
// RTMP 1.0 §7.1.6 — an Aggregate Message body is a sequence of FLV-shaped
// sub-headers + payloads + `PreviousTagSize` back-pointers. Any adversarial
// peer that delivers a type-22 message with arbitrary inner bytes must
// surface a clean Err, never panic. Targeted at the bounds-check arithmetic
// in `parse_aggregate` (UI24 length field, back-pointer slot, the §7.1.6
// timestamp re-normalisation subtract).
#[test]
fn aggregate_parse_random_bodies_never_panic() {
    let mut rng = Xs64::new(0xA66E_6A7E_0001);
    for iter in 0..1024 {
        let len = (rng.next() as usize) % 257;
        let mut buf = vec![0u8; len];
        rng.fill(&mut buf);
        let outer_ts = rng.next() as u32;
        let outer_sid = rng.next() as u32;
        let buf_copy = buf.clone();
        let result = std::panic::catch_unwind(move || {
            parse_aggregate(&Message {
                msg_type_id: 22,
                msg_stream_id: outer_sid,
                timestamp: outer_ts,
                payload: buf_copy,
            })
        });
        assert!(
            result.is_ok(),
            "parse_aggregate panicked on iteration {iter}, input bytes: {buf:?}"
        );
    }
}

/// Adversarial sub-header that claims `DataSize = 0xFFFFFF` (UI24 max)
/// but the body is short. Must surface as a clean `Err`, no
/// gigabyte-sized allocation, no panic.
#[test]
fn aggregate_oversize_sub_data_size_errors_fast() {
    // tag type = 9 (video), DataSize UI24 = 0xFFFFFF (16 MiB).
    let mut body = vec![9u8, 0xFF, 0xFF, 0xFF];
    body.extend_from_slice(&[0; 7]); // ts + sid
                                     // No payload, no back pointer — far short of what the header claims.
    let msg = Message {
        msg_type_id: 22,
        msg_stream_id: 0,
        timestamp: 0,
        payload: body,
    };
    assert!(parse_aggregate(&msg).is_err());
}

#[test]
fn amf0_valid_then_mutated_never_panics() {
    use oxideav_rtmp::amf::encode;
    let original = {
        let mut buf = Vec::new();
        encode(&mut buf, &Amf0Value::String("@setDataFrame".into()));
        encode(&mut buf, &Amf0Value::String("onMetaData".into()));
        encode(
            &mut buf,
            &Amf0Value::Object(vec![
                ("width".into(), Amf0Value::Number(1920.0)),
                ("height".into(), Amf0Value::Number(1080.0)),
                ("framerate".into(), Amf0Value::Number(30.0)),
                ("videocodecid".into(), Amf0Value::Number(7.0)),
                ("audiocodecid".into(), Amf0Value::Number(10.0)),
            ]),
        );
        buf
    };

    let mut rng = Xs64::new(0xDEAD_BEEF_0001);
    for iter in 0..1024 {
        let mut mutated = original.clone();
        // Flip 1..=4 random bytes.
        let nflips = 1 + (rng.next() as usize) % 4;
        for _ in 0..nflips {
            let pos = (rng.next() as usize) % mutated.len();
            mutated[pos] ^= (rng.next() as u8).max(1); // never xor 0
        }
        let buf_copy = mutated.clone();
        let result = std::panic::catch_unwind(move || amf0_decode_all(&buf_copy));
        assert!(
            result.is_ok(),
            "mutated onMetaData panicked on iter {iter}, bytes: {mutated:?}"
        );
    }
}
