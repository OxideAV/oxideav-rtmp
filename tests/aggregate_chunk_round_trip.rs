//! End-to-end test for [`oxideav_rtmp::aggregate`]: pack a video +
//! audio + data sub-message bundle into an Aggregate Message
//! (RTMP 1.0 §7.1.6 type 22), push it through
//! [`ChunkWriter::write_message`] / [`ChunkReader::read_message`], then
//! re-split with `parse_aggregate` and verify every sub-message comes
//! back byte-identical, on the aggregate's stream clock.
//!
//! Source of truth for the wire layout: `docs/streaming/rtmp/
//! rtmp-v1-0-spec-veovera.pdf` §7.1.6 (Aggregate Message) — together
//! with §6.1.1 (the FLV-shaped 11-byte sub-header) and the §E.3 / §E.4.1
//! cross-reference into `docs/container/flv/flv_v10_1.pdf`.

use std::io::Cursor;

use oxideav_rtmp::aggregate::{build_aggregate, parse_aggregate};
use oxideav_rtmp::chunk::{ChunkReader, ChunkWriter, Message};

/// Drive the full `build_aggregate -> ChunkWriter -> ChunkReader ->
/// parse_aggregate` pipeline and assert the sub-messages round-trip
/// byte-exact. The inner timestamps differ from the aggregate's by a
/// known offset so the §7.1.6 re-normalisation is also exercised.
#[test]
fn aggregate_round_trip_through_chunk_stream() {
    // Three sub-messages that look like real publisher traffic: an AVC
    // CodedFrames video tag, an AAC raw audio tag, and an `onMetaData`
    // script-data tag. Bytes are placeholders — the test asserts
    // identity, not codec correctness.
    let video = Message {
        msg_type_id: 9,
        msg_stream_id: 0,
        timestamp: 1000,
        payload: vec![0x17, 0x01, 0x00, 0x00, 0x00, 0xCA, 0xFE, 0xBA, 0xBE],
    };
    let audio = Message {
        msg_type_id: 8,
        msg_stream_id: 0,
        timestamp: 1023,
        payload: vec![0xAF, 0x01, 0xDE, 0xAD, 0xBE, 0xEF],
    };
    let script = Message {
        msg_type_id: 18,
        msg_stream_id: 0,
        timestamp: 1040,
        payload: vec![
            0x02, 0x00, 0x0A, b'o', b'n', b'M', b'e', b't', b'a', b'D', b'a', b't', b'a',
        ],
    };
    let subs = vec![video.clone(), audio.clone(), script.clone()];

    // Build the aggregate carried on stream id 7.
    let agg = build_aggregate(7, &subs).expect("build_aggregate");
    assert_eq!(agg.msg_type_id, 22);
    assert_eq!(agg.msg_stream_id, 7);
    // §7.1.6 SHOULD: outer timestamp = first sub's timestamp.
    assert_eq!(agg.timestamp, 1000);

    // Push through ChunkWriter → ChunkReader round-trip.
    let mut buf = Vec::<u8>::new();
    {
        let mut w = ChunkWriter::new(&mut buf);
        // csid 7: aggregate messages are not part of the protocol-control
        // family, so they ride a regular media chunk stream id.
        w.write_message(7, &agg).expect("write_message");
    }
    let mut r = ChunkReader::new(Cursor::new(buf));
    let received = r.read_message().expect("read_message");
    assert_eq!(received.msg_type_id, 22);
    assert_eq!(received.msg_stream_id, 7);
    assert_eq!(received.timestamp, 1000);
    assert_eq!(received.payload, agg.payload);

    // Re-split.
    let parsed = parse_aggregate(&received).expect("parse_aggregate");
    assert_eq!(parsed.len(), 3);
    for (got, want) in parsed.iter().zip(subs.iter()) {
        assert_eq!(got.msg_type_id, want.msg_type_id);
        assert_eq!(got.timestamp, want.timestamp);
        assert_eq!(got.payload, want.payload);
        // §7.1.6: every sub lands on the aggregate's stream id.
        assert_eq!(got.msg_stream_id, 7);
    }
}

/// Verify the wire layout of a single-sub aggregate matches the spec
/// byte-for-byte: §6.1.1 11-byte FLV-shaped header (Type, DataSize
/// UI24, Timestamp UI24+UI8, StreamID UI24=0) + payload + UI32 BE
/// back-pointer `= 11 + DataSize`.
#[test]
fn aggregate_wire_layout_one_sub() {
    let sub = Message {
        msg_type_id: 9,
        msg_stream_id: 0,
        timestamp: 0x0102_0304,
        payload: vec![0xAA, 0xBB, 0xCC, 0xDD],
    };
    let agg = build_aggregate(1, &[sub]).unwrap();
    assert_eq!(agg.msg_type_id, 22);
    assert_eq!(agg.msg_stream_id, 1);
    // Body = 11 (header) + 4 (payload) + 4 (back pointer).
    assert_eq!(agg.payload.len(), 19);
    // §6.1.1 header layout:
    //   [0]     tag type
    //   [1..4]  DataSize UI24 BE
    //   [4..7]  Timestamp UI24 BE (low 24 bits)
    //   [7]     TimestampExtended UI8 (upper 8 bits)
    //   [8..11] StreamID UI24 BE (zero)
    assert_eq!(agg.payload[0], 9);
    assert_eq!(&agg.payload[1..4], &[0x00, 0x00, 0x04]);
    // Wire ts: lo bytes [0x02, 0x03, 0x04], then ext byte [0x01]
    assert_eq!(&agg.payload[4..7], &[0x02, 0x03, 0x04]);
    assert_eq!(agg.payload[7], 0x01);
    assert_eq!(&agg.payload[8..11], &[0x00, 0x00, 0x00]);
    // Payload.
    assert_eq!(&agg.payload[11..15], &[0xAA, 0xBB, 0xCC, 0xDD]);
    // Back pointer = 11 + DataSize (4) = 15.
    assert_eq!(&agg.payload[15..19], &15u32.to_be_bytes());
}
