//! RTMP message-type constants + tiny builders for the protocol
//! control and command messages we send during publish setup.
//!
//! Each builder returns a [`chunk::Message`] ready to feed to
//! [`chunk::ChunkWriter::write_message`].

use crate::amf::{encode_command, Amf0Value};
use crate::caps::ConnectCapabilities;
use crate::chunk::Message;

// §6.1 "Message Header" — type ids.
pub const MSG_SET_CHUNK_SIZE: u8 = 1;
pub const MSG_ABORT: u8 = 2;
pub const MSG_ACK: u8 = 3;
pub const MSG_USER_CONTROL: u8 = 4;
pub const MSG_WINDOW_ACK_SIZE: u8 = 5;
pub const MSG_SET_PEER_BANDWIDTH: u8 = 6;
pub const MSG_AUDIO: u8 = 8;
pub const MSG_VIDEO: u8 = 9;
pub const MSG_DATA_AMF3: u8 = 15;
pub const MSG_SHARED_OBJECT_AMF3: u8 = 16;
pub const MSG_COMMAND_AMF3: u8 = 17;
pub const MSG_DATA_AMF0: u8 = 18;
pub const MSG_SHARED_OBJECT_AMF0: u8 = 19;
pub const MSG_COMMAND_AMF0: u8 = 20;
pub const MSG_AGGREGATE: u8 = 22;

// §7.1.7 "User Control Message Events"
pub const USR_STREAM_BEGIN: u16 = 0;
pub const USR_STREAM_EOF: u16 = 1;
pub const USR_STREAM_DRY: u16 = 2;
pub const USR_SET_BUFFER_LENGTH: u16 = 3;
pub const USR_STREAM_IS_RECORDED: u16 = 4;
pub const USR_PING_REQUEST: u16 = 6;
pub const USR_PING_RESPONSE: u16 = 7;

// Chunk stream id conventions — not mandated by spec but used by every
// major commodity implementation we have interoperated with, so we match.
pub const CSID_PROTOCOL_CONTROL: u32 = 2;
pub const CSID_COMMAND: u32 = 3;
pub const CSID_AUDIO: u32 = 4;
pub const CSID_VIDEO: u32 = 5;
pub const CSID_DATA: u32 = 6;

// ---------------------------------------------------------------------------
// Protocol control builders
// ---------------------------------------------------------------------------

pub fn build_set_chunk_size(size: u32) -> Message {
    // Bit 31 is reserved → mask to 31 bits.
    let size = size & 0x7FFF_FFFF;
    Message {
        msg_type_id: MSG_SET_CHUNK_SIZE,
        msg_stream_id: 0,
        timestamp: 0,
        payload: size.to_be_bytes().to_vec(),
    }
}

pub fn build_window_ack_size(size: u32) -> Message {
    Message {
        msg_type_id: MSG_WINDOW_ACK_SIZE,
        msg_stream_id: 0,
        timestamp: 0,
        payload: size.to_be_bytes().to_vec(),
    }
}

pub fn build_set_peer_bandwidth(size: u32, limit_type: u8) -> Message {
    let mut p = Vec::with_capacity(5);
    p.extend_from_slice(&size.to_be_bytes());
    p.push(limit_type);
    Message {
        msg_type_id: MSG_SET_PEER_BANDWIDTH,
        msg_stream_id: 0,
        timestamp: 0,
        payload: p,
    }
}

pub fn build_user_control_stream_begin(stream_id: u32) -> Message {
    let mut p = Vec::with_capacity(6);
    p.extend_from_slice(&USR_STREAM_BEGIN.to_be_bytes());
    p.extend_from_slice(&stream_id.to_be_bytes());
    Message {
        msg_type_id: MSG_USER_CONTROL,
        msg_stream_id: 0,
        timestamp: 0,
        payload: p,
    }
}

/// User-control `StreamEOF` event (`UCM` type 1).
///
/// Per RTMP 1.0 §7.1.7, the server uses this to tell the peer that
/// "playback of data is over as requested ... that the stream is dry."
/// In the publish direction we re-use it as the symmetric end-of-stream
/// signal so the peer learns the publisher is done before observing the
/// TCP FIN. The 4-byte event body is the stream id of the dry stream.
pub fn build_user_control_stream_eof(stream_id: u32) -> Message {
    let mut p = Vec::with_capacity(6);
    p.extend_from_slice(&USR_STREAM_EOF.to_be_bytes());
    p.extend_from_slice(&stream_id.to_be_bytes());
    Message {
        msg_type_id: MSG_USER_CONTROL,
        msg_stream_id: 0,
        timestamp: 0,
        payload: p,
    }
}

/// User-control `StreamDry` event (`UCM` type 2).
///
/// Per RTMP 1.0 §3.7 ("Commands Messages" — User Control message table),
/// "the server sends this event to notify the client that there is no
/// more data on the stream. If the server does not detect any message
/// for a time period, it can notify the subscribed clients that the
/// stream is dry." The 4-byte event body is the stream id of the dry
/// stream. Distinct from `StreamEOF`: `StreamDry` is "no data right
/// now," `StreamEOF` is "playback finished."
pub fn build_user_control_stream_dry(stream_id: u32) -> Message {
    let mut p = Vec::with_capacity(6);
    p.extend_from_slice(&USR_STREAM_DRY.to_be_bytes());
    p.extend_from_slice(&stream_id.to_be_bytes());
    Message {
        msg_type_id: MSG_USER_CONTROL,
        msg_stream_id: 0,
        timestamp: 0,
        payload: p,
    }
}

/// User-control `SetBufferLength` event (`UCM` type 3).
///
/// Per RTMP 1.0 §3.7, "the client sends this event to inform the server
/// of the buffer size (in milliseconds) that is used to buffer any data
/// coming over a stream. This event is sent before the server starts
/// processing the stream. The first 4 bytes of the event data represent
/// the stream ID and the next 4 bytes represent the buffer length, in
/// milliseconds." This is the only standard UCM event with a non-4-byte
/// event-data body (8 bytes total).
pub fn build_user_control_set_buffer_length(stream_id: u32, buffer_ms: u32) -> Message {
    let mut p = Vec::with_capacity(10);
    p.extend_from_slice(&USR_SET_BUFFER_LENGTH.to_be_bytes());
    p.extend_from_slice(&stream_id.to_be_bytes());
    p.extend_from_slice(&buffer_ms.to_be_bytes());
    Message {
        msg_type_id: MSG_USER_CONTROL,
        msg_stream_id: 0,
        timestamp: 0,
        payload: p,
    }
}

/// User-control `StreamIsRecorded` event (`UCM` type 4).
///
/// Per RTMP 1.0 §3.7, "the server sends this event to notify the client
/// that the stream is a recorded stream. The 4 bytes event data
/// represent the stream ID of the recorded stream." Servers typically
/// emit this right after `StreamBegin` for an on-demand stream.
pub fn build_user_control_stream_is_recorded(stream_id: u32) -> Message {
    let mut p = Vec::with_capacity(6);
    p.extend_from_slice(&USR_STREAM_IS_RECORDED.to_be_bytes());
    p.extend_from_slice(&stream_id.to_be_bytes());
    Message {
        msg_type_id: MSG_USER_CONTROL,
        msg_stream_id: 0,
        timestamp: 0,
        payload: p,
    }
}

/// User-control `PingRequest` event (`UCM` type 6).
///
/// Per RTMP 1.0 §3.7, "the server sends this event to test whether the
/// client is reachable. Event data is a 4-byte timestamp, representing
/// the local server time when the server dispatched the command. The
/// client responds with kMsgPingResponse on receiving kMsgPingRequest."
pub fn build_user_control_ping_request(timestamp_ms: u32) -> Message {
    let mut p = Vec::with_capacity(6);
    p.extend_from_slice(&USR_PING_REQUEST.to_be_bytes());
    p.extend_from_slice(&timestamp_ms.to_be_bytes());
    Message {
        msg_type_id: MSG_USER_CONTROL,
        msg_stream_id: 0,
        timestamp: 0,
        payload: p,
    }
}

/// User-control `PingResponse` event (`UCM` type 7).
///
/// Per RTMP 1.0 §3.7, "the client sends this event to the server in
/// response to the ping request. The event data is a 4-byte timestamp,
/// which was received with the kMsgPingRequest request." The caller is
/// responsible for echoing back the exact timestamp the peer's
/// `PingRequest` carried.
pub fn build_user_control_ping_response(timestamp_ms: u32) -> Message {
    let mut p = Vec::with_capacity(6);
    p.extend_from_slice(&USR_PING_RESPONSE.to_be_bytes());
    p.extend_from_slice(&timestamp_ms.to_be_bytes());
    Message {
        msg_type_id: MSG_USER_CONTROL,
        msg_stream_id: 0,
        timestamp: 0,
        payload: p,
    }
}

pub fn build_ack(bytes_received: u32) -> Message {
    Message {
        msg_type_id: MSG_ACK,
        msg_stream_id: 0,
        timestamp: 0,
        payload: bytes_received.to_be_bytes().to_vec(),
    }
}

// ---------------------------------------------------------------------------
// Command (AMF0) builders
// ---------------------------------------------------------------------------

/// `connect` — sent by the client right after handshake to open a
/// NetConnection onto the server's `app`. `tc_url` is the full
/// `rtmp://host[:port]/app` string; `app` is the last path segment.
///
/// Legacy publisher shape: no Enhanced RTMP capabilities advertised.
/// For an E-RTMP-aware publisher use
/// [`build_connect_with_caps`] which extends the Command Object with
/// `fourCcList` / `audio|videoFourCcInfoMap` / `capsEx`.
pub fn build_connect(transaction_id: f64, app: &str, tc_url: &str, flash_ver: &str) -> Message {
    build_connect_with_caps(
        transaction_id,
        app,
        tc_url,
        flash_ver,
        &ConnectCapabilities::default(),
    )
}

/// `connect` with Enhanced RTMP v1+v2 capability advertisements
/// (`enhanced-rtmp-v2.pdf` §"Enhancing NetConnection connect Command").
///
/// The legacy Command Object properties (`app` / `type` / `flashVer` /
/// `tcUrl` / `fpad` / `capabilities` / `audioCodecs` / `videoCodecs` /
/// `videoFunction`) are emitted in the historical order every commodity
/// peer expects, and the non-default `ConnectCapabilities` entries are
/// appended after them via [`ConnectCapabilities::encode_into`]. The
/// per-property emission order is the documented one:
/// `objectEncoding` → `fourCcList` → `videoFourCcInfoMap` →
/// `audioFourCcInfoMap` → `capsEx`. Empty / default fields are skipped,
/// so an empty `caps` block produces exactly the byte layout
/// [`build_connect`] would.
pub fn build_connect_with_caps(
    transaction_id: f64,
    app: &str,
    tc_url: &str,
    flash_ver: &str,
    caps: &ConnectCapabilities,
) -> Message {
    let mut pairs: Vec<(String, Amf0Value)> = vec![
        ("app".into(), Amf0Value::String(app.into())),
        ("type".into(), Amf0Value::String("nonprivate".into())),
        ("flashVer".into(), Amf0Value::String(flash_ver.into())),
        ("tcUrl".into(), Amf0Value::String(tc_url.into())),
        ("fpad".into(), Amf0Value::Boolean(false)),
        ("capabilities".into(), Amf0Value::Number(15.0)),
        ("audioCodecs".into(), Amf0Value::Number(0x0FFF as f64)),
        ("videoCodecs".into(), Amf0Value::Number(0x00FF as f64)),
        ("videoFunction".into(), Amf0Value::Number(1.0)),
    ];
    caps.encode_into(&mut pairs);
    let payload = encode_command("connect", transaction_id, Amf0Value::Object(pairs), &[]);
    Message {
        msg_type_id: MSG_COMMAND_AMF0,
        msg_stream_id: 0,
        timestamp: 0,
        payload,
    }
}

/// `_result` for the connect transaction. Standard server reply carries
/// the server's flashVer + a NetConnection.Connect.Success info object.
///
/// Legacy server shape: no Enhanced RTMP capability advertisement.
/// E-RTMP-aware servers should use [`build_connect_result_with_caps`]
/// to echo `videoFourCcInfoMap` / `capsEx` etc. back at the client per
/// `enhanced-rtmp-v2.pdf` §"Enhancing NetConnection connect Command".
pub fn build_connect_result(transaction_id: f64) -> Message {
    build_connect_result_with_caps(transaction_id, &ConnectCapabilities::default())
}

/// `_result` for the connect transaction with Enhanced RTMP capability
/// advertisement.
///
/// The Command Object slot (the first AMF0 value after the transaction
/// id) is the server's properties bag (`fmsVer` / `capabilities` /
/// `mode`); the trailing single argument is the
/// `NetConnection.Connect.Success` info object. Any non-default
/// `ConnectCapabilities` properties are appended to the info object —
/// `enhanced-rtmp-v2.pdf` is explicit: "the server provides some
/// properties within an Object as one of the parameters" and gives
/// `videoFourCcInfoMap` / `capsEx` as the canonical names. The info
/// object's existing `level` / `code` / `description` /
/// `objectEncoding` block is preserved, so a pre-2023 client still sees
/// the success status it expects and a v2-aware client lifts the
/// capability properties off the same object via
/// [`crate::caps::ConnectCapabilities::from_amf0`].
pub fn build_connect_result_with_caps(transaction_id: f64, caps: &ConnectCapabilities) -> Message {
    let props = Amf0Value::Object(vec![
        ("fmsVer".into(), Amf0Value::String("FMS/3,0,1,123".into())),
        ("capabilities".into(), Amf0Value::Number(31.0)),
        ("mode".into(), Amf0Value::Number(1.0)),
    ]);
    // Info object carries the success status + the capability block.
    // `objectEncoding` is encoded twice when the caller sets it — once
    // in our default `0.0` slot and once via `encode_into`. We pick
    // whichever the caller asks for: drop the default if they set their
    // own.
    let mut info_pairs: Vec<(String, Amf0Value)> = vec![
        ("level".into(), Amf0Value::String("status".into())),
        (
            "code".into(),
            Amf0Value::String("NetConnection.Connect.Success".into()),
        ),
        (
            "description".into(),
            Amf0Value::String("Connection accepted.".into()),
        ),
    ];
    if caps.object_encoding.is_none() {
        info_pairs.push(("objectEncoding".into(), Amf0Value::Number(0.0)));
    }
    caps.encode_into(&mut info_pairs);
    let payload = encode_command(
        "_result",
        transaction_id,
        props,
        &[Amf0Value::Object(info_pairs)],
    );
    Message {
        msg_type_id: MSG_COMMAND_AMF0,
        msg_stream_id: 0,
        timestamp: 0,
        payload,
    }
}

/// `releaseStream` — client advisory sent right before publish. The
/// server's reply isn't required for correctness.
pub fn build_release_stream(transaction_id: f64, stream_name: &str) -> Message {
    let payload = encode_command(
        "releaseStream",
        transaction_id,
        Amf0Value::Null,
        &[Amf0Value::String(stream_name.into())],
    );
    Message {
        msg_type_id: MSG_COMMAND_AMF0,
        msg_stream_id: 0,
        timestamp: 0,
        payload,
    }
}

/// `FCPublish` — another pre-publish advisory from Flash Media Live
/// Encoder. Many servers don't care; we send it for compatibility
/// with the few that do.
pub fn build_fc_publish(transaction_id: f64, stream_name: &str) -> Message {
    let payload = encode_command(
        "FCPublish",
        transaction_id,
        Amf0Value::Null,
        &[Amf0Value::String(stream_name.into())],
    );
    Message {
        msg_type_id: MSG_COMMAND_AMF0,
        msg_stream_id: 0,
        timestamp: 0,
        payload,
    }
}

/// `createStream` — client requests a new NetStream handle. Server
/// replies with `_result` carrying a fresh stream id the client uses
/// for subsequent audio/video messages.
pub fn build_create_stream(transaction_id: f64) -> Message {
    let payload = encode_command("createStream", transaction_id, Amf0Value::Null, &[]);
    Message {
        msg_type_id: MSG_COMMAND_AMF0,
        msg_stream_id: 0,
        timestamp: 0,
        payload,
    }
}

pub fn build_create_stream_result(transaction_id: f64, stream_id: f64) -> Message {
    let payload = encode_command(
        "_result",
        transaction_id,
        Amf0Value::Null,
        &[Amf0Value::Number(stream_id)],
    );
    Message {
        msg_type_id: MSG_COMMAND_AMF0,
        msg_stream_id: 0,
        timestamp: 0,
        payload,
    }
}

/// `publish` — client tells the server which stream name it's about
/// to feed. `publish_type` is usually `"live"`, `"record"`, or
/// `"append"`.
pub fn build_publish(
    transaction_id: f64,
    stream_id: u32,
    stream_name: &str,
    publish_type: &str,
) -> Message {
    let payload = encode_command(
        "publish",
        transaction_id,
        Amf0Value::Null,
        &[
            Amf0Value::String(stream_name.into()),
            Amf0Value::String(publish_type.into()),
        ],
    );
    Message {
        msg_type_id: MSG_COMMAND_AMF0,
        msg_stream_id: stream_id,
        timestamp: 0,
        payload,
    }
}

/// `onStatus` — server pushes this on the NetStream to signal state
/// changes (e.g. `NetStream.Publish.Start`). `code` / `description`
/// vary per event.
pub fn build_on_status(stream_id: u32, level: &str, code: &str, description: &str) -> Message {
    let info = Amf0Value::Object(vec![
        ("level".into(), Amf0Value::String(level.into())),
        ("code".into(), Amf0Value::String(code.into())),
        ("description".into(), Amf0Value::String(description.into())),
    ]);
    let payload = encode_command("onStatus", 0.0, Amf0Value::Null, &[info]);
    Message {
        msg_type_id: MSG_COMMAND_AMF0,
        msg_stream_id: stream_id,
        timestamp: 0,
        payload,
    }
}

/// `@setDataFrame("onMetaData", …)` — the standard way to publish
/// per-stream metadata (width, height, video/audio codec ids,
/// duration, …) before the first audio/video packet.
pub fn build_set_data_frame(stream_id: u32, metadata: Amf0Value) -> Message {
    let mut payload = Vec::new();
    crate::amf::encode(&mut payload, &Amf0Value::String("@setDataFrame".into()));
    crate::amf::encode(&mut payload, &Amf0Value::String("onMetaData".into()));
    crate::amf::encode(&mut payload, &metadata);
    Message {
        msg_type_id: MSG_DATA_AMF0,
        msg_stream_id: stream_id,
        timestamp: 0,
        payload,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Exact wire bytes for a `UserControl StreamBegin` per RTMP 1.0
    /// §7.1.7: 2-byte event type (0x0000) + 4-byte stream id BE.
    #[test]
    fn user_control_stream_begin_wire_bytes() {
        let m = build_user_control_stream_begin(1);
        assert_eq!(m.msg_type_id, MSG_USER_CONTROL);
        assert_eq!(m.msg_stream_id, 0);
        assert_eq!(m.payload, vec![0x00, 0x00, 0x00, 0x00, 0x00, 0x01]);
    }

    /// Symmetric wire bytes for `UserControl StreamEOF` (type 1): a
    /// publisher-side close emits this to signal end-of-publish before
    /// the TCP FIN, so the peer doesn't have to guess whether the
    /// connection dropped or terminated cleanly.
    #[test]
    fn user_control_stream_eof_wire_bytes() {
        let m = build_user_control_stream_eof(7);
        assert_eq!(m.msg_type_id, MSG_USER_CONTROL);
        assert_eq!(m.msg_stream_id, 0);
        assert_eq!(m.timestamp, 0);
        // Event type 1 (StreamEOF) | stream id 7.
        assert_eq!(m.payload, vec![0x00, 0x01, 0x00, 0x00, 0x00, 0x07]);
    }

    /// Wire bytes for `UserControl StreamDry` (type 2). Same six-byte
    /// frame as StreamBegin / StreamEOF: 2-byte BE event type, 4-byte BE
    /// stream id.
    #[test]
    fn user_control_stream_dry_wire_bytes() {
        let m = build_user_control_stream_dry(0x0010_2030);
        assert_eq!(m.msg_type_id, MSG_USER_CONTROL);
        assert_eq!(m.msg_stream_id, 0);
        assert_eq!(m.payload, vec![0x00, 0x02, 0x00, 0x10, 0x20, 0x30]);
    }

    /// Wire bytes for `UserControl SetBufferLength` (type 3). The only
    /// UCM event with an 8-byte event-data payload: 4 bytes stream id +
    /// 4 bytes buffer length in milliseconds.
    #[test]
    fn user_control_set_buffer_length_wire_bytes() {
        let m = build_user_control_set_buffer_length(1, 3000);
        assert_eq!(m.msg_type_id, MSG_USER_CONTROL);
        assert_eq!(m.msg_stream_id, 0);
        // Event type 3 (SetBufferLength) | stream id 1 | buffer 3000 ms.
        assert_eq!(
            m.payload,
            vec![0x00, 0x03, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x0B, 0xB8],
        );
    }

    /// Wire bytes for `UserControl StreamIsRecorded` (type 4): 2-byte BE
    /// event type, 4-byte BE stream id.
    #[test]
    fn user_control_stream_is_recorded_wire_bytes() {
        let m = build_user_control_stream_is_recorded(5);
        assert_eq!(m.msg_type_id, MSG_USER_CONTROL);
        assert_eq!(m.msg_stream_id, 0);
        assert_eq!(m.payload, vec![0x00, 0x04, 0x00, 0x00, 0x00, 0x05]);
    }

    /// Wire bytes for `UserControl PingRequest` (type 6): 2-byte BE
    /// event type, 4-byte BE local-server-time timestamp.
    #[test]
    fn user_control_ping_request_wire_bytes() {
        let m = build_user_control_ping_request(0xDEAD_BEEF);
        assert_eq!(m.msg_type_id, MSG_USER_CONTROL);
        assert_eq!(m.msg_stream_id, 0);
        assert_eq!(m.payload, vec![0x00, 0x06, 0xDE, 0xAD, 0xBE, 0xEF]);
    }

    /// Wire bytes for `UserControl PingResponse` (type 7): the same
    /// 4-byte timestamp the matching PingRequest carried, prefixed with
    /// the type-7 event header.
    #[test]
    fn user_control_ping_response_wire_bytes() {
        let m = build_user_control_ping_response(0xDEAD_BEEF);
        assert_eq!(m.msg_type_id, MSG_USER_CONTROL);
        assert_eq!(m.msg_stream_id, 0);
        assert_eq!(m.payload, vec![0x00, 0x07, 0xDE, 0xAD, 0xBE, 0xEF]);
    }

    /// `build_connect_with_caps` with a default capability block emits
    /// byte-identical output to the legacy `build_connect` builder.
    #[test]
    fn connect_with_empty_caps_matches_legacy() {
        let legacy = build_connect(1.0, "live", "rtmp://srv/live", "FMLE/3.0");
        let caps_empty = build_connect_with_caps(
            1.0,
            "live",
            "rtmp://srv/live",
            "FMLE/3.0",
            &ConnectCapabilities::default(),
        );
        assert_eq!(legacy.payload, caps_empty.payload);
    }

    /// `build_connect_with_caps` appends the Enhanced RTMP properties
    /// onto the Command Object in the documented v1+v2 order, after the
    /// legacy `videoFunction` field.
    #[test]
    fn connect_with_caps_appends_in_documented_order() {
        let mut video = crate::caps::FourCcInfoMap::new();
        video.insert("*", crate::caps::FOURCC_INFO_CAN_FORWARD);
        let mut audio = crate::caps::FourCcInfoMap::new();
        audio.insert("Opus", crate::caps::FOURCC_INFO_CAN_DECODE);
        let caps = ConnectCapabilities {
            object_encoding: Some(crate::caps::OBJECT_ENCODING_AMF3),
            fourcc_list: vec!["av01".into(), "hvc1".into()],
            video_fourcc_info_map: video,
            audio_fourcc_info_map: audio,
            caps_ex: crate::caps::CAPS_EX_RECONNECT | crate::caps::CAPS_EX_MULTITRACK,
        };

        let msg = build_connect_with_caps(1.0, "live", "rtmp://srv/live", "FMLE/3.0", &caps);
        // Walk the AMF0 payload and pull the Command Object's property
        // names. The third value is the Command Object (post-name,
        // post-tx-id).
        let vals = crate::amf::decode_all(&msg.payload).unwrap();
        assert_eq!(vals[0].as_str(), Some("connect"));
        let cmd_obj = match &vals[2] {
            Amf0Value::Object(p) => p,
            other => panic!("expected Object for command object, got {other:?}"),
        };
        let names: Vec<&str> = cmd_obj.iter().map(|(k, _)| k.as_str()).collect();
        let legacy_count = names
            .iter()
            .position(|n| *n == "videoFunction")
            .expect("legacy block must end with videoFunction")
            + 1;
        let extras = &names[legacy_count..];
        assert_eq!(
            extras,
            &[
                "objectEncoding",
                "fourCcList",
                "videoFourCcInfoMap",
                "audioFourCcInfoMap",
                "capsEx",
            ],
        );
    }

    /// `build_connect_result_with_caps` echoes the capability block back
    /// inside the trailing info object alongside the
    /// `NetConnection.Connect.Success` status.
    #[test]
    fn connect_result_with_caps_emits_info_block() {
        let mut video = crate::caps::FourCcInfoMap::new();
        video.insert("hvc1", crate::caps::FOURCC_INFO_CAN_DECODE);
        let caps = ConnectCapabilities {
            video_fourcc_info_map: video,
            caps_ex: crate::caps::CAPS_EX_MULTITRACK | crate::caps::CAPS_EX_MOD_EX,
            ..Default::default()
        };
        let msg = build_connect_result_with_caps(1.0, &caps);

        let vals = crate::amf::decode_all(&msg.payload).unwrap();
        assert_eq!(vals[0].as_str(), Some("_result"));
        // Info object is the fourth AMF0 value.
        let info = &vals[3];
        assert_eq!(
            info.get("code").and_then(Amf0Value::as_str),
            Some("NetConnection.Connect.Success"),
        );
        let parsed = ConnectCapabilities::from_amf0(info);
        assert_eq!(parsed.caps_ex, caps.caps_ex);
        assert_eq!(parsed.video_fourcc_info_map.get("hvc1"), Some(1));
    }

    /// `build_connect_result_with_caps` with an empty capability block
    /// emits the legacy bytes verbatim — pre-2023 clients keep parsing
    /// the same status info object they've always seen.
    #[test]
    fn connect_result_with_empty_caps_matches_legacy() {
        let legacy = build_connect_result(7.0);
        let empty = build_connect_result_with_caps(7.0, &ConnectCapabilities::default());
        assert_eq!(legacy.payload, empty.payload);
    }
}
