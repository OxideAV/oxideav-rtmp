//! RTMP Aggregate Message (type 22) parser + builder.
//!
//! RTMP 1.0 §7.1.6 defines the *Aggregate Message* as a single
//! [`crate::chunk::Message`] of type id `22` whose payload carries a
//! sequence of FLV-shaped sub-messages so several audio / video / data
//! frames can travel through the chunk stream as one message — fewer
//! chunk headers, one contiguous block on the wire.
//!
//! Wire layout per spec §7.1.6:
//!
//! ```text
//! +--------+-------+---------+--------+-------+---------+ - - -
//! |Header 0|Body 0 |Back     |Header 1|Body 1 |Back     |
//! |        |       |Pointer 0|        |       |Pointer 1|
//! +--------+-------+---------+--------+-------+---------+ - - -
//! ```
//!
//! Each sub-message header is the §6.1 message-header shape, which
//! the spec explicitly says "matches the format of FLV file ... used
//! for backward seek":
//!
//! * `Message Type` UI8 — sub-message type id (8 audio / 9 video /
//!   18 script / 15 / 17 / 20 …).
//! * `Payload length` UI24 BE — length of the sub body.
//! * `Timestamp` UI24 + UI8 — low 24 bits then upper 8 bits, per FLV
//!   §E.4.1 (a forward-only SI32 ms clock).
//! * `Stream ID` UI24 BE — set to 0 on the wire; per spec "the message
//!   stream ID of the aggregate message overrides the message stream
//!   IDs of the sub-messages".
//! * payload.
//! * `Back pointer` UI32 BE — `11 + DataSize` of the preceding sub-tag,
//!   exactly the FLV §E.3 `PreviousTagSize` invariant.
//!
//! Timestamp re-normalisation (§7.1.6):
//!
//! > "The difference between the timestamps of the aggregate message
//! > and the first sub-message is the offset used to renormalize the
//! > timestamps of the sub-messages to the stream timescale. The
//! > offset is added to each sub-message's timestamp to arrive at the
//! > normalized stream time."
//!
//! So a parser lifts each sub-message's wire timestamp `t_i` to the
//! stream clock as `t_i + (aggregate.timestamp - t_0)`, and a builder
//! that wants the SHOULD-be-zero offset just sets
//! `aggregate.timestamp == subs[0].timestamp`.
//!
//! This module is purely a transform over [`Message`] values — no I/O.
//! Send the result through [`crate::chunk::ChunkWriter`] / pull
//! aggregates out via [`crate::chunk::ChunkReader::read_message`] and
//! feed them to [`parse_aggregate`].

use crate::chunk::Message;
use crate::error::{Error, Result};
use crate::message::MSG_AGGREGATE;

/// RTMP 1.0 §6.1.1 fixed-width message header (1 + 3 + 4 + 3 = 11
/// bytes), shared with the FLV §E.4.1 `FLVTAG` header and the §7.1.6
/// aggregate sub-message header.
pub const SUB_HEADER_SIZE: usize = 11;

/// Trailing `PreviousTagSize` back-pointer width (UI32 BE).
pub const BACK_POINTER_SIZE: usize = 4;

/// UI24 cap on a sub-message payload length, per §6.1.1
/// "Length: Three-byte field".
const UI24_MAX: u32 = 0x00FF_FFFF;

/// Split an Aggregate Message (`msg_type_id == 22`) into its component
/// sub-messages.
///
/// Returns sub-messages with:
///
/// * `msg_type_id` — from the sub-header `Message Type` byte.
/// * `payload` — the sub-message body bytes (exact length from the
///   sub-header `Payload length` UI24).
/// * `timestamp` — the sub-header timestamp **plus the §7.1.6
///   re-normalisation offset** so each sub lives on the aggregate's
///   stream clock.
/// * `msg_stream_id` — overridden to the aggregate's `msg_stream_id`
///   per §7.1.6 "the message stream ID of the aggregate message
///   overrides the message stream IDs of the sub-messages".
///
/// Verifies the §E.3 `PreviousTagSize == 11 + DataSize` invariant on
/// every sub-message; a mismatch returns
/// [`Error::InvalidChunk`]. Truncated headers / payloads / back
/// pointers surface as [`Error::UnexpectedEof`]. Empty aggregates
/// (zero-byte payload) decode to an empty `Vec`.
pub fn parse_aggregate(msg: &Message) -> Result<Vec<Message>> {
    if msg.msg_type_id != MSG_AGGREGATE {
        return Err(Error::InvalidChunk(format!(
            "aggregate: expected message type {MSG_AGGREGATE}, got {}",
            msg.msg_type_id
        )));
    }
    let body = &msg.payload[..];
    let mut pos = 0usize;
    let mut subs: Vec<Message> = Vec::new();
    // Stream-clock offset applied to every sub. Computed once from the
    // first sub's wire timestamp, per §7.1.6.
    let mut offset: Option<i64> = None;
    while pos < body.len() {
        if pos + SUB_HEADER_SIZE > body.len() {
            return Err(Error::UnexpectedEof);
        }
        // §6.1.1 sub-header.
        let tag_type = body[pos];
        let data_size =
            ((body[pos + 1] as u32) << 16) | ((body[pos + 2] as u32) << 8) | (body[pos + 3] as u32);
        // FLV-shaped timestamp: lower 24 bits then upper 8 bits
        // (§E.4.1 "TimestampExtended ... represents the upper 8 bits").
        let ts_lo =
            ((body[pos + 4] as u32) << 16) | ((body[pos + 5] as u32) << 8) | (body[pos + 6] as u32);
        let ts_hi = body[pos + 7] as u32;
        let wire_ts = (ts_hi << 24) | ts_lo;
        // StreamID UI24 is on the wire but ignored per §7.1.6 (the
        // aggregate's stream id overrides it). We still bounds-check
        // by consuming the three bytes.
        let _wire_stream_id = ((body[pos + 8] as u32) << 16)
            | ((body[pos + 9] as u32) << 8)
            | (body[pos + 10] as u32);
        pos += SUB_HEADER_SIZE;

        // Sub payload.
        let payload_end = pos
            .checked_add(data_size as usize)
            .ok_or_else(|| Error::InvalidChunk("aggregate: sub payload length overflow".into()))?;
        if payload_end > body.len() {
            return Err(Error::UnexpectedEof);
        }
        let payload = body[pos..payload_end].to_vec();
        pos = payload_end;

        // Trailing `PreviousTagSize` UI32 BE. §E.3 invariant:
        // `PreviousTagSize == 11 + DataSize` of the preceding tag.
        if pos + BACK_POINTER_SIZE > body.len() {
            return Err(Error::UnexpectedEof);
        }
        let prev_tag_size =
            u32::from_be_bytes([body[pos], body[pos + 1], body[pos + 2], body[pos + 3]]);
        pos += BACK_POINTER_SIZE;
        let expected = (SUB_HEADER_SIZE as u32).saturating_add(data_size);
        if prev_tag_size != expected {
            return Err(Error::InvalidChunk(format!(
                "aggregate: back pointer {prev_tag_size} != 11 + DataSize {expected} (§7.1.6 / §E.3)"
            )));
        }

        // §7.1.6 timestamp re-normalisation: offset is fixed by the
        // first sub's wire timestamp and applied to every sub. We use
        // wrapping arithmetic on u32 because the spec models the
        // sub-message timestamp as an SI32 forward-only clock —
        // a publisher that pushes a value >= 2^31 has already
        // overflowed real-world relevance, but the wrapping keeps the
        // round-trip exact for synthetic / adversarial inputs.
        let off = match offset {
            Some(o) => o,
            None => {
                let o = (msg.timestamp as i64) - (wire_ts as i64);
                offset = Some(o);
                o
            }
        };
        let normalized = (wire_ts as i64).wrapping_add(off) as u32;

        subs.push(Message {
            msg_type_id: tag_type,
            msg_stream_id: msg.msg_stream_id,
            timestamp: normalized,
            payload,
        });
    }
    Ok(subs)
}

/// Pack a sequence of sub-messages into an Aggregate Message
/// (`msg_type_id = 22`).
///
/// The aggregate's `timestamp` is set to the first sub's timestamp so
/// the §7.1.6 SHOULD-be-zero offset holds; subs after the first carry
/// their original timestamps verbatim on the wire (a receiver that
/// runs the offset arithmetic recovers them at the same stream-clock
/// values). All sub `Stream ID` fields are written as 0 per §7.1.6
/// "the aggregate's stream id overrides", with the aggregate carrying
/// the real `stream_id`.
///
/// Returns `Err(InvalidChunk)` if any sub-payload exceeds the §6.1.1
/// UI24 length cap, or if the cumulative aggregate body would itself
/// overflow `u32`.
pub fn build_aggregate(stream_id: u32, subs: &[Message]) -> Result<Message> {
    let agg_ts = subs.first().map(|s| s.timestamp).unwrap_or(0);
    let mut body: Vec<u8> = Vec::new();
    for sub in subs {
        if sub.payload.len() > UI24_MAX as usize {
            return Err(Error::InvalidChunk(format!(
                "aggregate: sub payload {} exceeds UI24 max {}",
                sub.payload.len(),
                UI24_MAX
            )));
        }
        let data_size = sub.payload.len() as u32;
        // Bounds check: each sub adds 11 + payload + 4 to the body.
        let inc = (SUB_HEADER_SIZE as u64) + (data_size as u64) + (BACK_POINTER_SIZE as u64);
        if (body.len() as u64).saturating_add(inc) > u32::MAX as u64 {
            return Err(Error::InvalidChunk(
                "aggregate: cumulative body would exceed u32".into(),
            ));
        }
        // §6.1.1 sub-header. We DO NOT write `sub.msg_stream_id` here:
        // per §7.1.6 the aggregate's outer stream id is authoritative
        // and the sub StreamID UI24 is mandated 0 in the FLV layout
        // (§E.4.1 "Always 0").
        body.push(sub.msg_type_id);
        body.push(((data_size >> 16) & 0xFF) as u8);
        body.push(((data_size >> 8) & 0xFF) as u8);
        body.push((data_size & 0xFF) as u8);
        let ts = sub.timestamp;
        // FLV-shaped split-timestamp.
        body.push(((ts >> 16) & 0xFF) as u8);
        body.push(((ts >> 8) & 0xFF) as u8);
        body.push((ts & 0xFF) as u8);
        body.push(((ts >> 24) & 0xFF) as u8);
        // StreamID UI24 — always 0 per §7.1.6 / §E.4.1.
        body.push(0);
        body.push(0);
        body.push(0);
        body.extend_from_slice(&sub.payload);
        // §E.3 back pointer: 11 + DataSize.
        let prev_tag_size = (SUB_HEADER_SIZE as u32).saturating_add(data_size);
        body.extend_from_slice(&prev_tag_size.to_be_bytes());
    }
    Ok(Message {
        msg_type_id: MSG_AGGREGATE,
        msg_stream_id: stream_id,
        timestamp: agg_ts,
        payload: body,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::{MSG_AUDIO, MSG_DATA_AMF0, MSG_VIDEO};

    /// Build → parse round-trips three sub-messages with no
    /// re-normalisation (aggregate.timestamp == subs[0].timestamp).
    /// Every sub-message survives byte-for-byte and lands back on the
    /// aggregate's stream id per §7.1.6.
    #[test]
    fn round_trip_three_subs_no_offset() {
        let subs = vec![
            Message {
                msg_type_id: MSG_VIDEO,
                msg_stream_id: 999,
                timestamp: 1000,
                payload: vec![0x17, 0x01, 0xAA, 0xBB, 0xCC],
            },
            Message {
                msg_type_id: MSG_AUDIO,
                msg_stream_id: 999,
                timestamp: 1020,
                payload: vec![0xAF, 0x01, 0xDE, 0xAD, 0xBE, 0xEF],
            },
            Message {
                msg_type_id: MSG_VIDEO,
                msg_stream_id: 999,
                timestamp: 1040,
                payload: vec![0x27, 0x01, 0x42],
            },
        ];
        let agg = build_aggregate(7, &subs).unwrap();
        assert_eq!(agg.msg_type_id, MSG_AGGREGATE);
        assert_eq!(agg.msg_stream_id, 7);
        assert_eq!(agg.timestamp, 1000);

        let parsed = parse_aggregate(&agg).unwrap();
        assert_eq!(parsed.len(), 3);
        for (got, want) in parsed.iter().zip(subs.iter()) {
            assert_eq!(got.msg_type_id, want.msg_type_id);
            assert_eq!(got.timestamp, want.timestamp);
            assert_eq!(got.payload, want.payload);
            // §7.1.6 stream-id override: every sub lands on the
            // aggregate's id, NOT the original `999`.
            assert_eq!(got.msg_stream_id, 7);
        }
    }

    /// §7.1.6 timestamp re-normalisation: a parser receiving an
    /// aggregate whose own timestamp is `T_agg` and whose first sub
    /// carries `T_0` shifts every sub by `(T_agg - T_0)`. The first
    /// sub lands exactly on `T_agg`.
    #[test]
    fn timestamp_renormalisation_shifts_every_sub() {
        // Build a body by hand so we can stamp the wire timestamps
        // independently of the aggregate's outer timestamp.
        let mut body: Vec<u8> = Vec::new();
        for (ty, ts, pld) in &[
            (MSG_VIDEO, 100u32, vec![0xAAu8]),
            (MSG_AUDIO, 120u32, vec![0xBBu8, 0xCC]),
        ] {
            body.push(*ty);
            body.push(0);
            body.push(0);
            body.push(pld.len() as u8);
            body.push(((ts >> 16) & 0xFF) as u8);
            body.push(((ts >> 8) & 0xFF) as u8);
            body.push((ts & 0xFF) as u8);
            body.push(((ts >> 24) & 0xFF) as u8);
            body.push(0);
            body.push(0);
            body.push(0);
            body.extend_from_slice(pld);
            let prev = 11u32 + pld.len() as u32;
            body.extend_from_slice(&prev.to_be_bytes());
        }
        let agg = Message {
            msg_type_id: MSG_AGGREGATE,
            msg_stream_id: 5,
            timestamp: 5000, // offset = 5000 - 100 = +4900
            payload: body,
        };
        let parsed = parse_aggregate(&agg).unwrap();
        assert_eq!(parsed.len(), 2);
        // First sub re-normalises to exactly the aggregate's clock.
        assert_eq!(parsed[0].timestamp, 5000);
        // Second sub shifts by the same offset.
        assert_eq!(parsed[1].timestamp, 120 + 4900);
        // Stream id propagated.
        assert_eq!(parsed[0].msg_stream_id, 5);
        assert_eq!(parsed[1].msg_stream_id, 5);
    }

    /// Empty aggregate body → empty `Vec`; the round-trip from an
    /// empty `subs` slice produces a zero-payload aggregate that the
    /// parser accepts.
    #[test]
    fn empty_aggregate_round_trip() {
        let agg = build_aggregate(3, &[]).unwrap();
        assert!(agg.payload.is_empty());
        assert_eq!(agg.timestamp, 0);
        let parsed = parse_aggregate(&agg).unwrap();
        assert!(parsed.is_empty());
    }

    /// Wrong outer message type id is a clean `InvalidChunk`.
    #[test]
    fn parse_rejects_wrong_outer_type() {
        let msg = Message {
            msg_type_id: MSG_VIDEO,
            msg_stream_id: 1,
            timestamp: 0,
            payload: vec![],
        };
        match parse_aggregate(&msg) {
            Err(Error::InvalidChunk(_)) => {}
            other => panic!("expected InvalidChunk, got {other:?}"),
        }
    }

    /// Truncated sub-header → `UnexpectedEof`.
    #[test]
    fn truncated_sub_header_is_eof() {
        let agg = Message {
            msg_type_id: MSG_AGGREGATE,
            msg_stream_id: 0,
            timestamp: 0,
            // 5 bytes — not enough for an 11-byte sub-header.
            payload: vec![0x09, 0x00, 0x00, 0x01, 0x00],
        };
        match parse_aggregate(&agg) {
            Err(Error::UnexpectedEof) => {}
            other => panic!("expected UnexpectedEof, got {other:?}"),
        }
    }

    /// Truncated sub-payload → `UnexpectedEof`.
    #[test]
    fn truncated_sub_payload_is_eof() {
        // Header says DataSize == 10, but only 3 payload bytes follow.
        let mut body = vec![MSG_VIDEO, 0, 0, 10];
        body.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0]); // ts + sid
        body.extend_from_slice(&[0xAA, 0xBB, 0xCC]);
        let agg = Message {
            msg_type_id: MSG_AGGREGATE,
            msg_stream_id: 0,
            timestamp: 0,
            payload: body,
        };
        match parse_aggregate(&agg) {
            Err(Error::UnexpectedEof) => {}
            other => panic!("expected UnexpectedEof, got {other:?}"),
        }
    }

    /// Truncated back pointer → `UnexpectedEof`.
    #[test]
    fn truncated_back_pointer_is_eof() {
        let mut body = vec![MSG_VIDEO, 0, 0, 1];
        body.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0]); // ts + sid
        body.push(0xAA); // payload
        body.extend_from_slice(&[0, 0]); // 2 of the 4 back-pointer bytes
        let agg = Message {
            msg_type_id: MSG_AGGREGATE,
            msg_stream_id: 0,
            timestamp: 0,
            payload: body,
        };
        match parse_aggregate(&agg) {
            Err(Error::UnexpectedEof) => {}
            other => panic!("expected UnexpectedEof, got {other:?}"),
        }
    }

    /// `PreviousTagSize != 11 + DataSize` → `InvalidChunk` per §7.1.6
    /// "the back pointer contains the size of the previous message
    /// including its header".
    #[test]
    fn mismatched_back_pointer_is_invalid_chunk() {
        let mut body = vec![MSG_VIDEO, 0, 0, 1];
        body.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0]); // ts + sid
        body.push(0xAA); // payload
        body.extend_from_slice(&999u32.to_be_bytes()); // wrong: should be 12
        let agg = Message {
            msg_type_id: MSG_AGGREGATE,
            msg_stream_id: 0,
            timestamp: 0,
            payload: body,
        };
        match parse_aggregate(&agg) {
            Err(Error::InvalidChunk(_)) => {}
            other => panic!("expected InvalidChunk, got {other:?}"),
        }
    }

    /// Each sub-message header on the wire stamps `StreamID = 0`
    /// (§7.1.6 / §E.4.1 "Always 0"). The aggregate's own
    /// `msg_stream_id` is what surfaces to the receiver, regardless of
    /// what the caller stuffed into each sub's `msg_stream_id` slot
    /// before building.
    #[test]
    fn build_emits_zero_stream_ids_on_sub_headers() {
        let sub = Message {
            msg_type_id: MSG_DATA_AMF0,
            msg_stream_id: 0xABCDEF, // ignored on the wire
            timestamp: 50,
            payload: vec![0x02, 0x00, 0x04, b'P', b'i', b'n', b'g'],
        };
        let agg = build_aggregate(7, &[sub]).unwrap();
        // Sub header: 1 + 3 (DataSize) + 4 (ts) + 3 (sid) = 11
        // bytes; the sid is the last three of the header.
        let sid_bytes = &agg.payload[8..11];
        assert_eq!(sid_bytes, &[0, 0, 0]);
        // And the round-trip surfaces the aggregate's id (7), not the
        // 0xABCDEF the caller passed.
        let parsed = parse_aggregate(&agg).unwrap();
        assert_eq!(parsed[0].msg_stream_id, 7);
    }

    /// First sub's wire timestamp lands on the aggregate's outer
    /// timestamp on a build (§7.1.6 SHOULD).
    #[test]
    fn build_sets_outer_timestamp_to_first_sub() {
        let subs = vec![
            Message {
                msg_type_id: MSG_VIDEO,
                msg_stream_id: 0,
                timestamp: 12345,
                payload: vec![0x17],
            },
            Message {
                msg_type_id: MSG_AUDIO,
                msg_stream_id: 0,
                timestamp: 12365,
                payload: vec![0xAF],
            },
        ];
        let agg = build_aggregate(1, &subs).unwrap();
        assert_eq!(agg.timestamp, 12345);
        // Wire ts of the first sub equals the aggregate ts → parser
        // offset is zero → both subs come back exactly as written.
        let parsed = parse_aggregate(&agg).unwrap();
        assert_eq!(parsed[0].timestamp, 12345);
        assert_eq!(parsed[1].timestamp, 12365);
    }

    /// Aggregate payload longer than a single UI24 sub-DataSize is
    /// fine — only the per-sub UI24 cap matters. Build + parse a
    /// hundred small subs.
    #[test]
    fn many_small_subs_round_trip() {
        let subs: Vec<Message> = (0..100)
            .map(|i| Message {
                msg_type_id: MSG_VIDEO,
                msg_stream_id: 0,
                timestamp: i * 33,
                payload: vec![i as u8; 4],
            })
            .collect();
        let agg = build_aggregate(1, &subs).unwrap();
        let parsed = parse_aggregate(&agg).unwrap();
        assert_eq!(parsed.len(), subs.len());
        for (got, want) in parsed.iter().zip(subs.iter()) {
            assert_eq!(got.msg_type_id, want.msg_type_id);
            assert_eq!(got.timestamp, want.timestamp);
            assert_eq!(got.payload, want.payload);
        }
    }

    /// Builder rejects a sub whose payload exceeds the §6.1.1 UI24
    /// `Payload length` cap.
    #[test]
    fn build_rejects_oversize_sub_payload() {
        let oversize = Message {
            msg_type_id: MSG_VIDEO,
            msg_stream_id: 0,
            timestamp: 0,
            // UI24_MAX + 1 bytes — exceeds the field's representable
            // range. We don't actually allocate those bytes for the
            // payload-check path — the builder consults the length
            // field, not the data — so we use `Vec::with_capacity(0)`
            // and forge a length via a transmute-free trick: build it
            // from a small allocation but lie to it by extending up
            // to the cap. To keep the test cheap, we use a real
            // allocation just above the cap.
            payload: vec![0u8; (UI24_MAX as usize) + 1],
        };
        match build_aggregate(0, &[oversize]) {
            Err(Error::InvalidChunk(_)) => {}
            other => panic!("expected InvalidChunk, got {other:?}"),
        }
    }

    /// The parser must not panic on a sub-header whose DataSize would
    /// overflow `usize` arithmetic.
    #[test]
    fn parse_rejects_overflowing_data_size() {
        // DataSize = 0xFFFFFF, but the body is only 11 bytes long; the
        // parser must surface this as a clean UnexpectedEof rather
        // than panicking on the indexing arithmetic.
        let mut body = vec![MSG_VIDEO, 0xFF, 0xFF, 0xFF];
        body.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0]);
        let agg = Message {
            msg_type_id: MSG_AGGREGATE,
            msg_stream_id: 0,
            timestamp: 0,
            payload: body,
        };
        match parse_aggregate(&agg) {
            Err(Error::UnexpectedEof) => {}
            other => panic!("expected UnexpectedEof, got {other:?}"),
        }
    }

    /// `parse_aggregate` is robust against deterministic random input:
    /// any byte stream must surface as `Ok` or a clean typed `Err`,
    /// never panic, never spin. Mirrors the
    /// `tests/injection_robustness.rs` philosophy.
    #[test]
    fn fuzz_random_bodies_never_panic() {
        // Tiny xorshift32 PRNG, deterministic seed.
        let mut state: u32 = 0xCAFEF00D;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            state
        };
        for _ in 0..1024 {
            let len = (next() % 512) as usize;
            let mut body = vec![0u8; len];
            for byte in body.iter_mut() {
                *byte = (next() & 0xFF) as u8;
            }
            let agg = Message {
                msg_type_id: MSG_AGGREGATE,
                msg_stream_id: next(),
                timestamp: next(),
                payload: body,
            };
            // Result is unused — the point is that we must not panic.
            let _ = parse_aggregate(&agg);
        }
    }
}
