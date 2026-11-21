#![no_main]

//! Chunk-stream reassembly.
//!
//! Phase 1 feeds the raw input to a `ChunkReader` whose memory budgets
//! are far below the wire maxima and applies Set Chunk Size / Abort
//! messages the way a session does. Contract: never panics, never
//! returns a message above the configured ceiling, never accounts more
//! bytes than were supplied.
//!
//! Phase 2 derives a message list from the input, writes it with a
//! `ChunkWriter` (changing chunk size and chunk stream id on the way)
//! and requires the reader to return exactly the same messages.

use std::io::Cursor;

use libfuzzer_sys::fuzz_target;
use oxideav_rtmp::chunk::{ChunkReader, ChunkWriter, Message};

const MAX_MESSAGE: usize = 1 << 16;
const MAX_IN_FLIGHT: usize = 1 << 18;

fn hostile(data: &[u8]) {
    let mut r = ChunkReader::new(Cursor::new(data));
    r.set_max_message_size(MAX_MESSAGE);
    r.set_max_in_flight_bytes(MAX_IN_FLIGHT);
    r.set_window_ack_size(64);
    // Every message consumes at least one wire byte, so this bounds
    // the loop as well as the input does.
    for _ in 0..=data.len() {
        match r.read_message() {
            Ok(m) => {
                assert!(m.payload.len() <= MAX_MESSAGE);
                assert!(r.received_bytes() as usize <= data.len());
                let _ = r.ack_due();
                let _ = m.stream_kind();
                let _ = m.validate_protocol_control_invariants();
                if m.payload.len() >= 4 {
                    let v = u32::from_be_bytes([
                        m.payload[0],
                        m.payload[1],
                        m.payload[2],
                        m.payload[3],
                    ]);
                    match m.msg_type_id {
                        1 => r.set_chunk_size((v & 0x7FFF_FFFF) as usize),
                        2 => {
                            r.abort_partial(v);
                        }
                        5 => r.set_window_ack_size(v),
                        _ => {}
                    }
                }
            }
            Err(_) => break,
        }
    }
}

fn round_trip(data: &[u8]) {
    let mut msgs: Vec<(u32, Message)> = Vec::new();
    let mut sizes: Vec<usize> = Vec::new();
    let mut ts: u32 = 0;
    let mut rest = data;
    while rest.len() >= 8 && msgs.len() < 24 {
        let (h, tail) = rest.split_at(8);
        let csid = match h[0] % 5 {
            0 => 2 + (h[1] as u32 % 62),
            1 => 64 + h[1] as u32,
            2 => 320 + u16::from_be_bytes([h[1], h[2]]) as u32 % 65_280,
            3 => 5,
            _ => 4,
        };
        // Mostly forward-moving timestamps with occasional jumps past
        // the 24-bit boundary and backwards.
        ts = match h[3] % 8 {
            0 => ts.wrapping_add(0x00FF_FFF0),
            1 => ts.wrapping_sub(h[4] as u32),
            2 => u32::from_be_bytes([h[4], h[5], h[6], h[7]]),
            _ => ts.wrapping_add(h[4] as u32),
        };
        let len = (u16::from_be_bytes([h[5], h[6]]) as usize % 700).min(tail.len());
        let (payload, tail) = tail.split_at(len);
        msgs.push((
            csid,
            Message {
                msg_type_id: 8 + (h[7] % 16),
                msg_stream_id: (h[7] >> 4) as u32,
                timestamp: ts,
                payload: payload.to_vec(),
            },
        ));
        sizes.push(1 + (u16::from_be_bytes([h[2], h[0]]) as usize % 900));
        rest = tail;
    }
    let mut wire = Vec::new();
    let mut boundaries = Vec::new();
    {
        let mut w = ChunkWriter::new(&mut wire);
        for ((csid, m), size) in msgs.iter().zip(&sizes) {
            w.set_chunk_size(*size);
            w.write_message(*csid, m).expect("in-range message");
            boundaries.push(());
        }
    }
    let mut r = ChunkReader::new(Cursor::new(&wire));
    for ((_, want), size) in msgs.iter().zip(&sizes) {
        r.set_chunk_size(*size);
        let got = r.read_message().expect("own output must read back");
        assert_eq!(got.msg_type_id, want.msg_type_id);
        assert_eq!(got.msg_stream_id, want.msg_stream_id);
        assert_eq!(got.timestamp, want.timestamp);
        assert_eq!(got.payload, want.payload);
    }
    assert!(r.is_idle());
    assert_eq!(r.received_bytes() as usize, wire.len());
}

fuzz_target!(|data: &[u8]| {
    hostile(data);
    round_trip(data);
});
