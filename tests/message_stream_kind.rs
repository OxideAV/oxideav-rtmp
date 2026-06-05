//! Typed-accessor + spec-§5 validation tests for
//! [`oxideav_rtmp::chunk::Message`].
//!
//! Covers:
//!
//! * Round-trip — build a SetChunkSize protocol-control message via
//!   [`oxideav_rtmp::message::build_set_chunk_size`], render it through
//!   [`oxideav_rtmp::chunk::ChunkWriter`], reassemble with
//!   [`oxideav_rtmp::chunk::ChunkReader`], confirm the reassembled
//!   message classifies as [`MessageStreamKind::Control`] and passes
//!   [`Message::validate_protocol_control_invariants`].
//! * NetStream classification — a video message stamped with the
//!   server-assigned NetStream id 1 classifies as
//!   [`MessageStreamKind::NetStream(1)`].
//! * Negative — a hand-crafted SetChunkSize whose `msg_stream_id` was
//!   forged to a non-zero NetStream id is rejected by the validator
//!   per Message Formats spec §5 ("Protocol control messages MUST have
//!   message stream ID 0").
//! * Reserved-bit — a `msg_stream_id` with the §4.1 reserved top byte
//!   set surfaces as [`MessageStreamKind::Reserved`] and is rejected
//!   by the validator.

use std::io::Cursor;

use oxideav_rtmp::chunk::{ChunkReader, ChunkWriter, Message, MessageStreamKind};
use oxideav_rtmp::message::{build_set_chunk_size, MSG_SET_CHUNK_SIZE, MSG_VIDEO};

#[test]
fn protocol_control_round_trips_as_control_stream() {
    // Build a real SetChunkSize the same way the publish setup does,
    // render onto a chunk-stream byte buffer, then reassemble and ask
    // the typed accessor what kind of stream it rode on.
    let msg = build_set_chunk_size(4096);
    assert_eq!(msg.msg_type_id, MSG_SET_CHUNK_SIZE);
    assert_eq!(msg.msg_stream_id, 0);
    assert!(msg.is_control_stream());
    assert_eq!(msg.stream_kind(), MessageStreamKind::Control);
    msg.validate_protocol_control_invariants()
        .expect("freshly built SetChunkSize satisfies spec §5");

    let mut wire = Vec::new();
    {
        let mut w = ChunkWriter::new(&mut wire);
        // Per spec §5 + the Chunk Stream conventions in `message.rs`,
        // protocol-control traffic rides csid 2.
        w.write_message(2, &msg).expect("write SetChunkSize");
    }
    let mut r = ChunkReader::new(Cursor::new(&wire));
    let recv = r.read_message().expect("read SetChunkSize");
    assert_eq!(recv.msg_type_id, MSG_SET_CHUNK_SIZE);
    assert_eq!(recv.msg_stream_id, 0);
    assert_eq!(recv.stream_kind(), MessageStreamKind::Control);
    assert!(recv.is_control_stream());
    recv.validate_protocol_control_invariants()
        .expect("reassembled SetChunkSize still satisfies spec §5");
    assert_eq!(recv.payload, 4096u32.to_be_bytes());
}

#[test]
fn netstream_video_message_classifies_as_netstream() {
    // A video message on the NetStream the server allocated via
    // `_result(createStream)` — stream id 1 is the canonical first
    // handle every commodity server returns.
    let msg = Message {
        msg_type_id: MSG_VIDEO,
        msg_stream_id: 1,
        timestamp: 0,
        payload: vec![0x17, 0x00, 0x00, 0x00, 0x00], // AVC seq header marker
    };
    assert!(!msg.is_control_stream());
    assert_eq!(msg.stream_kind(), MessageStreamKind::NetStream(1));
    msg.validate_protocol_control_invariants()
        .expect("type-9 video on NetStream is not protocol-control");
}

#[test]
fn protocol_control_with_nonzero_msid_is_rejected() {
    // Forge a SetChunkSize whose msg_stream_id is the NetStream id the
    // server handed back — illegal per Message Formats spec §5
    // ("Protocol control messages MUST have message stream ID 0").
    // Validator should refuse without us having to parse it off any
    // wire.
    let bad = Message {
        msg_type_id: MSG_SET_CHUNK_SIZE,
        msg_stream_id: 1,
        timestamp: 0,
        payload: 4096u32.to_be_bytes().to_vec(),
    };
    let err = bad
        .validate_protocol_control_invariants()
        .expect_err("must refuse non-zero msid on protocol-control");
    let s = err.to_string();
    assert!(
        s.contains("protocol-control") && s.contains("msg_stream_id"),
        "diagnostic must point at the §5 violation; got {s:?}"
    );
}

#[test]
fn netstream_id_with_reserved_top_byte_is_rejected() {
    // Message Formats spec §4.1 allocates 3 bytes for the stream id
    // field; anything that sets the high byte is reserved. The typed
    // accessor surfaces this as Reserved(...) so a strict consumer
    // can refuse.
    let bad = Message {
        msg_type_id: MSG_VIDEO,
        msg_stream_id: 0x0100_0001,
        timestamp: 0,
        payload: vec![0x17, 0x00, 0x00, 0x00, 0x00],
    };
    assert_eq!(bad.stream_kind(), MessageStreamKind::Reserved(0x0100_0001));
    let err = bad
        .validate_protocol_control_invariants()
        .expect_err("must refuse reserved high byte");
    assert!(err.to_string().contains("reserved"));
}
