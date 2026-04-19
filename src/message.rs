//! RTMP message-type constants + tiny builders for the protocol
//! control and command messages we send during publish setup.
//!
//! Each builder returns a [`chunk::Message`] ready to feed to
//! [`chunk::ChunkWriter::write_message`].

use crate::amf::{encode_command, Amf0Value};
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

// Chunk stream id conventions — not mandated by spec but used by
// every major impl (FFmpeg, nginx-rtmp, OBS) so we match.
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
pub fn build_connect(transaction_id: f64, app: &str, tc_url: &str, flash_ver: &str) -> Message {
    let cmd_obj = Amf0Value::Object(vec![
        ("app".into(), Amf0Value::String(app.into())),
        ("type".into(), Amf0Value::String("nonprivate".into())),
        ("flashVer".into(), Amf0Value::String(flash_ver.into())),
        ("tcUrl".into(), Amf0Value::String(tc_url.into())),
        ("fpad".into(), Amf0Value::Boolean(false)),
        ("capabilities".into(), Amf0Value::Number(15.0)),
        ("audioCodecs".into(), Amf0Value::Number(0x0FFF as f64)),
        ("videoCodecs".into(), Amf0Value::Number(0x00FF as f64)),
        ("videoFunction".into(), Amf0Value::Number(1.0)),
    ]);
    let payload = encode_command("connect", transaction_id, cmd_obj, &[]);
    Message {
        msg_type_id: MSG_COMMAND_AMF0,
        msg_stream_id: 0,
        timestamp: 0,
        payload,
    }
}

/// `_result` for the connect transaction. Standard server reply carries
/// the server's flashVer + a NetConnection.Connect.Success info object.
pub fn build_connect_result(transaction_id: f64) -> Message {
    let props = Amf0Value::Object(vec![
        ("fmsVer".into(), Amf0Value::String("FMS/3,0,1,123".into())),
        ("capabilities".into(), Amf0Value::Number(31.0)),
        ("mode".into(), Amf0Value::Number(1.0)),
    ]);
    let info = Amf0Value::Object(vec![
        ("level".into(), Amf0Value::String("status".into())),
        (
            "code".into(),
            Amf0Value::String("NetConnection.Connect.Success".into()),
        ),
        (
            "description".into(),
            Amf0Value::String("Connection accepted.".into()),
        ),
        ("objectEncoding".into(), Amf0Value::Number(0.0)),
    ]);
    let payload = encode_command("_result", transaction_id, props, &[info]);
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
