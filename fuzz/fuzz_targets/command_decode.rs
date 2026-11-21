#![no_main]

//! Message-body decoders. The first input byte picks a message type
//! id and the rest is the body, dispatched the way the sessions
//! dispatch it; every typed view that accepts the body is exercised,
//! and the views with a byte-level inverse must round-trip.

use libfuzzer_sys::fuzz_target;
use oxideav_rtmp::caps::ConnectCapabilities;
use oxideav_rtmp::chunk::Message;
use oxideav_rtmp::message::{
    CallCommand, NetStreamCommand, UserControlEvent, parse_data_frame, parse_set_peer_bandwidth,
};
use oxideav_rtmp::shared_object::{SharedObjectMessage, parse_shared_object};
use oxideav_rtmp::{Amf0Value, OnMetaData, TrackInfo, amf, amf3, parse_aggregate};

fn command_views(values: &[Amf0Value]) {
    if let Ok(Some(cmd)) = NetStreamCommand::parse(values) {
        // A recognised NetStream command re-encodes and re-parses to
        // the same command.
        let msg = cmd.to_message(1);
        let back = amf::decode_all(&msg.payload).expect("own command must decode");
        let again = NetStreamCommand::parse(&back).expect("own command must parse");
        assert_eq!(again.map(|c| c.to_message(1).payload), Some(msg.payload));
    }
    if let Some(call) = CallCommand::parse(values) {
        let _ = call.expects_response();
        let _ = call.to_message();
    }
    let _ = parse_data_frame(values);
    for v in values {
        let caps = ConnectCapabilities::from_amf0(v);
        let mut pairs = Vec::new();
        caps.encode_into(&mut pairs);
        if let Ok(meta) = OnMetaData::from_amf0(v) {
            let _ = meta.audio_fourcc();
            let _ = meta.video_fourcc();
            let _ = meta.to_amf0();
        }
        let _ = TrackInfo::from_amf0(v);
        let _ = oxideav_rtmp::ColorInfo::from_amf0(v);
    }
}

fuzz_target!(|data: &[u8]| {
    let Some((&selector, body)) = data.split_first() else {
        return;
    };
    let msg = |ty: u8| Message {
        msg_type_id: ty,
        msg_stream_id: 1,
        timestamp: 1000,
        payload: body.to_vec(),
    };
    match selector % 8 {
        0 => {
            if let Ok(values) = amf::decode_all(body) {
                command_views(&values);
            }
        }
        1 => {
            if let Ok(values) = amf3::decode_message_to_amf0(body) {
                command_views(&values);
            }
        }
        2 => {
            if let Ok(ev) = UserControlEvent::parse(body) {
                // The typed view rebuilds the bytes it was parsed
                // from (trailing bytes past a fixed-size event's data
                // are tolerated on parse and not reproduced).
                assert!(body.starts_with(&ev.to_message().payload));
                let _ = ev.event_type();
                let _ = ev.is_spec_defined();
            }
            let _ = parse_set_peer_bandwidth(body);
        }
        3 => {
            if let Ok(so) = SharedObjectMessage::<Amf0Value>::parse_amf0(body) {
                if let Ok(bytes) = so.build_amf0() {
                    let again =
                        SharedObjectMessage::<Amf0Value>::parse_amf0(&bytes).expect("own SO");
                    assert_eq!(again.build_amf0().expect("rebuild"), bytes);
                }
            }
            let _ = parse_shared_object(&msg(19));
        }
        4 => {
            if let Ok(so) = SharedObjectMessage::<oxideav_rtmp::Amf3Value>::parse_amf3(body) {
                let _ = so.build_amf3();
                let _ = so.to_amf0();
            }
            let _ = parse_shared_object(&msg(16));
        }
        5 => {
            if let Ok(subs) = parse_aggregate(&msg(22)) {
                // Sub-messages never claim more payload than the body.
                let total: usize = subs.iter().map(|s| s.payload.len()).sum();
                assert!(total <= body.len());
                if let Ok(rebuilt) = oxideav_rtmp::build_aggregate(1, &subs) {
                    let again = parse_aggregate(&rebuilt).expect("own aggregate");
                    assert_eq!(again.len(), subs.len());
                    for (a, b) in again.iter().zip(&subs) {
                        assert_eq!(a.msg_type_id, b.msg_type_id);
                        assert_eq!(a.payload, b.payload);
                        assert_eq!(a.timestamp, b.timestamp);
                    }
                }
            }
        }
        6 => {
            let m = msg(body.first().copied().unwrap_or(0));
            let _ = m.stream_kind();
            let _ = m.validate_protocol_control_invariants();
        }
        _ => {
            let _ = oxideav_rtmp::flv_crypt::parse_encrypted_body(body);
        }
    }
});
