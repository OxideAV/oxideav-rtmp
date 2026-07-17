//! RTMP message-type constants + tiny builders for the protocol
//! control and command messages we send during publish setup.
//!
//! Each builder returns a [`chunk::Message`](crate::chunk::Message)
//! ready to feed to
//! [`chunk::ChunkWriter::write_message`](crate::chunk::ChunkWriter::write_message).

use crate::amf::{encode_command, Amf0Value};
use crate::caps::ConnectCapabilities;
use crate::chunk::Message;
use crate::error::{Error, Result};

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

/// Abort Message (protocol control type 2, RTMP 1.0 §5.2).
///
/// Per the spec, "Protocol control message 2, Abort Message, is used to
/// notify the peer if it is waiting for chunks to complete a message,
/// then to discard the partially received message over a chunk stream
/// and abort processing of that message. The peer receives the chunk
/// stream ID of the message to be discarded as payload of this protocol
/// message." The body is a single 4-byte big-endian chunk stream ID
/// (Figure 3). Like every protocol-control message it travels on the
/// control stream (`msg_stream_id == 0`).
pub fn build_abort(chunk_stream_id: u32) -> Message {
    Message {
        msg_type_id: MSG_ABORT,
        msg_stream_id: 0,
        timestamp: 0,
        payload: chunk_stream_id.to_be_bytes().to_vec(),
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

/// §5.4.5 Limit Type 0 — "Hard: The peer SHOULD limit its output
/// bandwidth to the indicated window size."
pub const PEER_BANDWIDTH_LIMIT_HARD: u8 = 0;
/// §5.4.5 Limit Type 1 — "Soft: The peer SHOULD limit its output
/// bandwidth to the the window indicated in this message or the limit
/// already in effect, whichever is smaller."
pub const PEER_BANDWIDTH_LIMIT_SOFT: u8 = 1;
/// §5.4.5 Limit Type 2 — "Dynamic: If the previous Limit Type was
/// Hard, treat this message as though it was marked Hard, otherwise
/// ignore this message."
pub const PEER_BANDWIDTH_LIMIT_DYNAMIC: u8 = 2;

/// Split a §5.4.5 Set Peer Bandwidth payload into `(window, limit
/// type)`. The payload is 5 bytes (4-byte Acknowledgement Window
/// size + 1-byte Limit Type); a 4-byte payload with the limit byte
/// missing is tolerated as Hard — the conservative reading that
/// matches unconditional adoption.
pub fn parse_set_peer_bandwidth(payload: &[u8]) -> crate::error::Result<(u32, u8)> {
    if payload.len() < 4 {
        return Err(crate::error::Error::InvalidChunk(format!(
            "Set Peer Bandwidth payload too short: {} bytes",
            payload.len()
        )));
    }
    let mut w = [0u8; 4];
    w.copy_from_slice(&payload[..4]);
    let limit = payload.get(4).copied().unwrap_or(PEER_BANDWIDTH_LIMIT_HARD);
    Ok((u32::from_be_bytes(w), limit))
}

/// §5.4.5 Set Peer Bandwidth limit-type state machine.
///
/// Tracks the effective output-bandwidth window across a sequence of
/// Set Peer Bandwidth messages:
///
/// * **Hard (0)** — adopt the indicated window.
/// * **Soft (1)** — adopt the smaller of the indicated window and the
///   limit already in effect.
/// * **Dynamic (2)** — "If the previous Limit Type was Hard, treat
///   this message as though it was marked Hard, otherwise ignore this
///   message."
/// * Reserved limit types are ignored.
///
/// [`apply`](Self::apply) returns `Some(window)` only when the
/// effective window *changed* — the moment the receiver "SHOULD
/// respond with a Window Acknowledgement Size message if the window
/// size is different from the last one sent".
#[derive(Debug, Default, Clone)]
pub struct PeerBandwidthLimiter {
    window: Option<u32>,
    last_was_hard: bool,
}

impl PeerBandwidthLimiter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Currently effective output-bandwidth window, if any Set Peer
    /// Bandwidth has been accepted yet.
    pub fn window(&self) -> Option<u32> {
        self.window
    }

    /// Apply one inbound Set Peer Bandwidth. Returns the new effective
    /// window iff it changed.
    pub fn apply(&mut self, window: u32, limit_type: u8) -> Option<u32> {
        match limit_type {
            PEER_BANDWIDTH_LIMIT_HARD => {
                self.last_was_hard = true;
                self.adopt(window)
            }
            PEER_BANDWIDTH_LIMIT_SOFT => {
                self.last_was_hard = false;
                let effective = self.window.map_or(window, |cur| cur.min(window));
                self.adopt(effective)
            }
            PEER_BANDWIDTH_LIMIT_DYNAMIC => {
                if self.last_was_hard {
                    // "treat this message as though it was marked
                    // Hard" — including for the next Dynamic in a row.
                    self.adopt(window)
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    fn adopt(&mut self, window: u32) -> Option<u32> {
        if self.window == Some(window) {
            return None;
        }
        self.window = Some(window);
        Some(window)
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

// ---------------------------------------------------------------------------
// User Control Message typed accessor (round-trip parser)
// ---------------------------------------------------------------------------

/// Strongly-typed view of a User Control Message body per RTMP 1.0
/// §3.7 / §7.1.7.
///
/// The `build_user_control_*` family above produces a [`Message`]
/// with `msg_type_id == MSG_USER_CONTROL` and a payload shaped
/// `event_type:U16BE | event_data:..`. [`UserControlEvent::parse`]
/// is the inverse: lift such a payload into one of the seven
/// spec-defined variants (or the catch-all [`Self::Unknown`] for
/// forward compatibility — the spec leaves event types 5, 8..,
/// reserved).
///
/// `Unknown` carries both the raw `event_type` and the unconsumed
/// `event_data` bytes so a forwarding ingest can route unrecognised
/// UCMs without losing information; a strict consumer can refuse
/// the message by matching on it.
///
/// Round-trip helper: [`UserControlEvent::to_message`] produces the
/// same [`Message`] the matching `build_user_control_*` builder
/// emits, so `parse(build_x().payload) == Ok(x)` for every variant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UserControlEvent {
    /// UCM type 0 — server tells the client that a stream is ready to
    /// receive messages on the given stream id. Emitted right after
    /// `_result(createStream)` from the server side; carried as the
    /// 4-byte BE stream id in the event data.
    StreamBegin { stream_id: u32 },
    /// UCM type 1 — playback / publish finished on the given stream
    /// id. The publisher emits this before tearing down its socket so
    /// the peer learns "EOF was intentional" rather than guessing
    /// whether the TCP FIN was a crash. 4-byte BE stream id.
    StreamEof { stream_id: u32 },
    /// UCM type 2 — server has not seen any data on the given stream
    /// for a while. Distinct from [`Self::StreamEof`]: this is a
    /// transient "no data right now" signal; the stream may resume.
    /// 4-byte BE stream id.
    StreamDry { stream_id: u32 },
    /// UCM type 3 — client tells the server how many milliseconds of
    /// buffer it is willing to keep filled. The only standard UCM
    /// event with an 8-byte event-data body: 4 bytes BE stream id
    /// followed by 4 bytes BE buffer length in ms.
    SetBufferLength { stream_id: u32, buffer_ms: u32 },
    /// UCM type 4 — server announces that the stream is recorded
    /// (on-demand / archival). 4-byte BE stream id. Typically emitted
    /// right after [`Self::StreamBegin`] on a play request.
    StreamIsRecorded { stream_id: u32 },
    /// UCM type 6 — sender's local time in ms; receiver must echo the
    /// same value back in a [`Self::PingResponse`]. Used for liveness
    /// probing + RTT measurement. 4-byte BE timestamp.
    PingRequest { timestamp_ms: u32 },
    /// UCM type 7 — exact echo of the timestamp from a paired
    /// [`Self::PingRequest`]. 4-byte BE timestamp.
    PingResponse { timestamp_ms: u32 },
    /// Any event type not assigned by RTMP 1.0 §3.7 — UCM 5 is
    /// reserved, and any UCM type ≥ 8 is forward-compatible space
    /// the spec leaves unspecified. `data` holds the unconsumed
    /// event-data bytes verbatim so a forwarding ingest can route
    /// the message through without re-encoding.
    Unknown { event_type: u16, data: Vec<u8> },
}

impl UserControlEvent {
    /// Decode a UCM payload (the contents of a [`Message`] with
    /// `msg_type_id == MSG_USER_CONTROL`) into a [`UserControlEvent`]
    /// per RTMP 1.0 §3.7 / §7.1.7.
    ///
    /// Returns [`Error::ProtocolViolation`] if the payload is shorter
    /// than the 2-byte event-type header, or if one of the
    /// fixed-shape spec-defined variants is truncated below its
    /// declared event-data size (4 bytes for the stream-id-carrying
    /// variants and ping, 8 bytes for `SetBufferLength`). Unknown
    /// event types accept any tail length, including zero, so a
    /// forwarding ingest never rejects forward-compatible messages.
    pub fn parse(payload: &[u8]) -> Result<Self> {
        if payload.len() < 2 {
            return Err(Error::ProtocolViolation(
                "UserControl: payload < 2 bytes (need event type)".into(),
            ));
        }
        let event_type = u16::from_be_bytes([payload[0], payload[1]]);
        let data = &payload[2..];
        match event_type {
            USR_STREAM_BEGIN => Ok(Self::StreamBegin {
                stream_id: read_u32_be(data, "StreamBegin")?,
            }),
            USR_STREAM_EOF => Ok(Self::StreamEof {
                stream_id: read_u32_be(data, "StreamEOF")?,
            }),
            USR_STREAM_DRY => Ok(Self::StreamDry {
                stream_id: read_u32_be(data, "StreamDry")?,
            }),
            USR_SET_BUFFER_LENGTH => {
                if data.len() < 8 {
                    return Err(Error::ProtocolViolation(format!(
                        "UserControl SetBufferLength: event data {} < 8 bytes",
                        data.len()
                    )));
                }
                let stream_id = u32::from_be_bytes([data[0], data[1], data[2], data[3]]);
                let buffer_ms = u32::from_be_bytes([data[4], data[5], data[6], data[7]]);
                Ok(Self::SetBufferLength {
                    stream_id,
                    buffer_ms,
                })
            }
            USR_STREAM_IS_RECORDED => Ok(Self::StreamIsRecorded {
                stream_id: read_u32_be(data, "StreamIsRecorded")?,
            }),
            USR_PING_REQUEST => Ok(Self::PingRequest {
                timestamp_ms: read_u32_be(data, "PingRequest")?,
            }),
            USR_PING_RESPONSE => Ok(Self::PingResponse {
                timestamp_ms: read_u32_be(data, "PingResponse")?,
            }),
            other => Ok(Self::Unknown {
                event_type: other,
                data: data.to_vec(),
            }),
        }
    }

    /// 2-byte BE event-type identifier per §7.1.7. Matches the value
    /// the wire form embeds in its first two bytes.
    pub fn event_type(&self) -> u16 {
        match self {
            Self::StreamBegin { .. } => USR_STREAM_BEGIN,
            Self::StreamEof { .. } => USR_STREAM_EOF,
            Self::StreamDry { .. } => USR_STREAM_DRY,
            Self::SetBufferLength { .. } => USR_SET_BUFFER_LENGTH,
            Self::StreamIsRecorded { .. } => USR_STREAM_IS_RECORDED,
            Self::PingRequest { .. } => USR_PING_REQUEST,
            Self::PingResponse { .. } => USR_PING_RESPONSE,
            Self::Unknown { event_type, .. } => *event_type,
        }
    }

    /// True iff this is one of the seven event types §3.7 / §7.1.7
    /// assigns a fixed shape to. [`Self::Unknown`] returns false.
    pub fn is_spec_defined(&self) -> bool {
        !matches!(self, Self::Unknown { .. })
    }

    /// Inverse of [`Self::parse`]: produce the matching protocol
    /// control [`Message`] (msg_type_id = 4, msg_stream_id = 0,
    /// timestamp = 0). For the seven spec-defined variants this
    /// emits byte-for-byte the same payload the corresponding
    /// `build_user_control_*` builder would; for [`Self::Unknown`]
    /// the event-type bytes and the carried `data` are concatenated
    /// verbatim, so a parse / re-encode cycle is byte-stable.
    pub fn to_message(&self) -> Message {
        match self {
            Self::StreamBegin { stream_id } => build_user_control_stream_begin(*stream_id),
            Self::StreamEof { stream_id } => build_user_control_stream_eof(*stream_id),
            Self::StreamDry { stream_id } => build_user_control_stream_dry(*stream_id),
            Self::SetBufferLength {
                stream_id,
                buffer_ms,
            } => build_user_control_set_buffer_length(*stream_id, *buffer_ms),
            Self::StreamIsRecorded { stream_id } => {
                build_user_control_stream_is_recorded(*stream_id)
            }
            Self::PingRequest { timestamp_ms } => build_user_control_ping_request(*timestamp_ms),
            Self::PingResponse { timestamp_ms } => build_user_control_ping_response(*timestamp_ms),
            Self::Unknown { event_type, data } => {
                let mut p = Vec::with_capacity(2 + data.len());
                p.extend_from_slice(&event_type.to_be_bytes());
                p.extend_from_slice(data);
                Message {
                    msg_type_id: MSG_USER_CONTROL,
                    msg_stream_id: 0,
                    timestamp: 0,
                    payload: p,
                }
            }
        }
    }
}

/// Helper for [`UserControlEvent::parse`] — read a 4-byte BE field
/// out of `event_data` or surface [`Error::ProtocolViolation`] with
/// the variant name in the message.
fn read_u32_be(data: &[u8], variant: &str) -> Result<u32> {
    if data.len() < 4 {
        return Err(Error::ProtocolViolation(format!(
            "UserControl {variant}: event data {} < 4 bytes",
            data.len()
        )));
    }
    Ok(u32::from_be_bytes([data[0], data[1], data[2], data[3]]))
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

// ---------------------------------------------------------------------------
// NetStream onStatus code strings (RTMP 1.0 Commands-Messages §4.2)
// ---------------------------------------------------------------------------

/// `NetStream.Play.Start` — §4.2.1: "If the play command is
/// successful, the client receives OnStatus message from server which
/// is NetStream.Play.Start."
pub const STATUS_PLAY_START: &str = "NetStream.Play.Start";
/// `NetStream.Play.Reset` — §4.2.1 message flow: "NetStream.Play.Reset
/// is sent by the server only if the play command sent by the client
/// has set the reset flag."
pub const STATUS_PLAY_RESET: &str = "NetStream.Play.Reset";
/// `NetStream.Play.StreamNotFound` — §4.2.1: "If the specified stream
/// is not found, NetStream.Play.StreamNotFound is received."
pub const STATUS_PLAY_STREAM_NOT_FOUND: &str = "NetStream.Play.StreamNotFound";
/// `NetStream.Seek.Notify` — §4.2.7: "The server sends a status message
/// NetStream.Seek.Notify when seek is successful."
pub const STATUS_SEEK_NOTIFY: &str = "NetStream.Seek.Notify";
/// `NetStream.Pause.Notify` — §4.2.8: "The server sends a status
/// message NetStream.Pause.Notify when the stream is paused."
pub const STATUS_PAUSE_NOTIFY: &str = "NetStream.Pause.Notify";
/// `NetStream.Unpause.Notify` — §4.2.8: "NetStream.Unpause.Notify is
/// sent when a stream in un-paused."
pub const STATUS_UNPAUSE_NOTIFY: &str = "NetStream.Unpause.Notify";
/// `NetStream.Publish.Start` — §4.2.6: "The server responds with the
/// OnStatus command to mark the beginning of publish."
pub const STATUS_PUBLISH_START: &str = "NetStream.Publish.Start";
/// `NetStream.Unpublish.Success` — the unpublish notification the
/// server emits on the NetStream when a publish is torn down cleanly.
pub const STATUS_UNPUBLISH_SUCCESS: &str = "NetStream.Unpublish.Success";
/// `NetStream.Publish.BadName` — refusal code emitted when a publish
/// attempt is rejected (stream key denied / name already in use).
pub const STATUS_PUBLISH_BAD_NAME: &str = "NetStream.Publish.BadName";

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

/// The `code` string a server MUST set in the onStatus Info Object to
/// request a client reconnect, per `enhanced-rtmp-v2.pdf` §"Reconnect
/// Request" (table "Info Object parameter for onStatus command when
/// handling reconnect").
pub const RECONNECT_REQUEST_CODE: &str = "NetConnection.Connect.ReconnectRequest";

/// `onStatus(NetConnection.Connect.ReconnectRequest)` — Enhanced RTMP
/// v2 §"Reconnect Request". A server emits this NetConnection-level
/// onStatus command to ask the client to reconnect — e.g. ahead of a
/// live-streaming-server update, or to remap the client to a
/// different server instance for load balancing / geolocation.
///
/// Per the spec's Info Object table:
///
/// * `code` MUST be `NetConnection.Connect.ReconnectRequest`
///   ([`RECONNECT_REQUEST_CODE`]).
/// * `level` MUST be `status`.
/// * `tcUrl` (optional) — "absolute or relative URI reference of the
///   server to which to reconnect. If not specified, use the tcUrl
///   for the current connection." A server that aims to remap the
///   client MUST set it.
/// * `description` (optional) — human-readable information about the
///   message.
///
/// The command rides the NetConnection (message stream id 0) with
/// transaction id 0 ("no response needed") and a null Command Object,
/// matching the spec's "Server to client, NetConnection onStatus
/// command" table.
pub fn build_reconnect_request(tc_url: Option<&str>, description: Option<&str>) -> Message {
    let mut props = vec![
        (
            "code".into(),
            Amf0Value::String(RECONNECT_REQUEST_CODE.into()),
        ),
        ("level".into(), Amf0Value::String("status".into())),
    ];
    if let Some(desc) = description {
        props.push(("description".into(), Amf0Value::String(desc.into())));
    }
    if let Some(url) = tc_url {
        props.push(("tcUrl".into(), Amf0Value::String(url.into())));
    }
    let payload = encode_command(
        "onStatus",
        0.0,
        Amf0Value::Null,
        &[Amf0Value::Object(props)],
    );
    Message {
        msg_type_id: MSG_COMMAND_AMF0,
        // NetConnection commands live on the control stream (message
        // stream id 0) — this is a connection-level status event, not
        // a NetStream one.
        msg_stream_id: 0,
        timestamp: 0,
        payload,
    }
}

/// The reserved server-intercepted data-frame control name that asks
/// the server to *store* the wrapped data message and replay it to
/// every future subscriber (the `@` prefix marks it as a control name
/// rather than a subscriber-visible handler).
pub const SET_DATA_FRAME: &str = "@setDataFrame";
/// The inverse control name: discard a previously stored data frame
/// so it is no longer replayed to new subscribers.
pub const CLEAR_DATA_FRAME: &str = "@clearDataFrame";

/// `@setDataFrame("onMetaData", …)` — the standard way to publish
/// per-stream metadata (width, height, video/audio codec ids,
/// duration, …) before the first audio/video packet.
pub fn build_set_data_frame(stream_id: u32, metadata: Amf0Value) -> Message {
    build_set_data_frame_named(stream_id, "onMetaData", metadata)
}

/// `@setDataFrame(handler, value)` for an arbitrary handler name
/// (`"onMetaData"`, `"onCuePoint"`, `"onFI"`, …): three AMF0 values —
/// the reserved control name, the handler the server should re-emit
/// under, and the payload (typically an ECMA array or object). The
/// server strips the first value and replays `[handler, value]` as a
/// plain data message to each new subscriber at play time.
pub fn build_set_data_frame_named(stream_id: u32, handler: &str, value: Amf0Value) -> Message {
    let mut payload = Vec::new();
    crate::amf::encode(&mut payload, &Amf0Value::String(SET_DATA_FRAME.into()));
    crate::amf::encode(&mut payload, &Amf0Value::String(handler.into()));
    crate::amf::encode(&mut payload, &value);
    Message {
        msg_type_id: MSG_DATA_AMF0,
        msg_stream_id: stream_id,
        timestamp: 0,
        payload,
    }
}

/// `@clearDataFrame(handler)` — tell the server to discard the stored
/// data frame registered under `handler` (matching the name given to
/// `@setDataFrame`) so it is no longer replayed to new subscribers.
/// Two AMF0 values only; there is no payload argument.
pub fn build_clear_data_frame(stream_id: u32, handler: &str) -> Message {
    let mut payload = Vec::new();
    crate::amf::encode(&mut payload, &Amf0Value::String(CLEAR_DATA_FRAME.into()));
    crate::amf::encode(&mut payload, &Amf0Value::String(handler.into()));
    Message {
        msg_type_id: MSG_DATA_AMF0,
        msg_stream_id: stream_id,
        timestamp: 0,
        payload,
    }
}

/// A decoded `@setDataFrame` / `@clearDataFrame` data-frame control
/// (the publisher→server metadata-carriage convention). Produced by
/// [`parse_data_frame`].
#[derive(Debug, Clone, PartialEq)]
pub enum DataFrameCommand {
    /// `["@setDataFrame", handler, value]` — store `value` under
    /// `handler` and replay it to future subscribers.
    Set {
        handler: String,
        /// The wrapped payload — usually an ECMA array, though some
        /// encoders send a strict object (any AMF0 value is accepted).
        value: Amf0Value,
    },
    /// `["@clearDataFrame", handler]` — drop the stored frame.
    Clear { handler: String },
}

/// Classify a decoded data-message value list as a data-frame control.
///
/// Returns `None` when the message is not `@`-prefixed data-frame
/// control traffic (e.g. a bare `onMetaData` or `onCuePoint` message)
/// or when a control name arrives without its required arguments —
/// callers fall back to their plain data-message handling in either
/// case. AMF3 data messages (type 15) classify identically after
/// bridging through [`crate::amf3::decode_message_to_amf0`].
pub fn parse_data_frame(values: &[Amf0Value]) -> Option<DataFrameCommand> {
    let name = values.first()?.as_str()?;
    match name {
        SET_DATA_FRAME => {
            let handler = values.get(1)?.as_str()?.to_owned();
            let value = values.get(2)?.clone();
            Some(DataFrameCommand::Set { handler, value })
        }
        CLEAR_DATA_FRAME => {
            let handler = values.get(1)?.as_str()?.to_owned();
            Some(DataFrameCommand::Clear { handler })
        }
        _ => None,
    }
}

/// `onMetaData` data message — the shape a **server** sends to a
/// subscriber on a play stream.
///
/// The publish direction wraps metadata as
/// `@setDataFrame("onMetaData", meta)` ([`build_set_data_frame`]);
/// when the server relays it down to a playing client the
/// `@setDataFrame` RPC prefix is dropped and the data message body is
/// just the `["onMetaData", meta]` pair — the same §E.4.4 name+value
/// layout an FLV script-data tag carries.
pub fn build_on_meta_data(stream_id: u32, metadata: &Amf0Value) -> Message {
    build_data_message(stream_id, "onMetaData", metadata)
}

/// A bare `[handler, value]` AMF0 data message (type 18) for an
/// arbitrary handler name — the server→subscriber shape of any data
/// frame (`onMetaData`, `onCuePoint`, `onFI`, …): the handler name
/// string followed by its argument, with no `@setDataFrame` prefix.
pub fn build_data_message(stream_id: u32, handler: &str, value: &Amf0Value) -> Message {
    let mut payload = Vec::new();
    crate::amf::encode(&mut payload, &Amf0Value::String(handler.into()));
    crate::amf::encode(&mut payload, value);
    Message {
        msg_type_id: MSG_DATA_AMF0,
        msg_stream_id: stream_id,
        timestamp: 0,
        payload,
    }
}

/// A §7.2.1.2 NetConnection `call` — a remote procedure call either
/// peer may issue. On the wire the *command name field carries the
/// procedure name* (the spec's command table opens with "Procedure
/// Name — Name of the remote procedure that is called" where every
/// other command's table opens with a fixed Command Name), so any
/// command whose name is not one of the spec-defined built-ins is an
/// RPC directed at the receiving application.
///
/// `transaction_id` is non-zero when the caller expects a response
/// ("If a response is expected we give a transaction Id. Else we pass
/// a value of 0"); answer with [`build_call_result`] /
/// [`build_call_error`] echoing it. `command_object` is Null when the
/// caller had no command info to attach.
#[derive(Debug, Clone, PartialEq)]
pub struct CallCommand {
    /// Remote procedure name (the wire command-name field).
    pub procedure: String,
    /// Non-zero iff the caller expects a `_result` / `_error` reply.
    pub transaction_id: f64,
    /// The §7.2.1.2 Command Object ("If there exists any command info
    /// this is set, else this is set to null type").
    pub command_object: Amf0Value,
    /// Optional Arguments, verbatim.
    pub arguments: Vec<Amf0Value>,
}

impl CallCommand {
    /// Interpret a decoded command frame as a `call` RPC. Returns
    /// `None` when the frame is too short to be one (no name or no
    /// transaction id) — the caller is responsible for first routing
    /// spec-defined command names elsewhere
    /// ([`is_reserved_command_name`]).
    pub fn parse(values: &[Amf0Value]) -> Option<CallCommand> {
        let procedure = values.first()?.as_str()?.to_owned();
        let transaction_id = values.get(1)?.as_f64()?;
        let command_object = values.get(2).cloned().unwrap_or(Amf0Value::Null);
        let arguments = values.get(3..).unwrap_or(&[]).to_vec();
        Some(CallCommand {
            procedure,
            transaction_id,
            command_object,
            arguments,
        })
    }

    /// Byte-level inverse of [`parse`](Self::parse): the outbound
    /// `call` command message (AMF0, on the NetConnection's message
    /// stream 0 like every other NetConnection command).
    pub fn to_message(&self) -> Message {
        build_call(
            &self.procedure,
            self.transaction_id,
            self.command_object.clone(),
            &self.arguments,
        )
    }

    /// Whether the caller expects a reply (§7.2.1.2: transaction id 0
    /// means fire-and-forget).
    pub fn expects_response(&self) -> bool {
        self.transaction_id != 0.0
    }
}

/// §7.2.1.2 `call` — the outbound RPC message. `procedure` rides the
/// command-name field; pass `transaction_id` 0 when no response is
/// expected.
pub fn build_call(
    procedure: &str,
    transaction_id: f64,
    command_object: Amf0Value,
    arguments: &[Amf0Value],
) -> Message {
    let payload = crate::amf::encode_command(procedure, transaction_id, command_object, arguments);
    Message {
        msg_type_id: MSG_COMMAND_AMF0,
        msg_stream_id: 0,
        timestamp: 0,
        payload,
    }
}

/// §7.2.1.2 response frame: `_result(transaction_id, command_object,
/// response)`. Echo the RPC's transaction id.
pub fn build_call_result(
    transaction_id: f64,
    command_object: Amf0Value,
    response: Amf0Value,
) -> Message {
    let payload =
        crate::amf::encode_command("_result", transaction_id, command_object, &[response]);
    Message {
        msg_type_id: MSG_COMMAND_AMF0,
        msg_stream_id: 0,
        timestamp: 0,
        payload,
    }
}

/// Failure counterpart of [`build_call_result`] — same §7.2.1.2
/// response structure under the `_error` command name.
pub fn build_call_error(
    transaction_id: f64,
    command_object: Amf0Value,
    response: Amf0Value,
) -> Message {
    let payload = crate::amf::encode_command("_error", transaction_id, command_object, &[response]);
    Message {
        msg_type_id: MSG_COMMAND_AMF0,
        msg_stream_id: 0,
        timestamp: 0,
        payload,
    }
}

/// The command names the RTMP 1.0 spec itself defines (§7.2.1
/// NetConnection: `connect` / `call` / `close` / `createStream`; §7.2.2
/// NetStream: `play` / `play2` / `deleteStream` / `closeStream` /
/// `receiveAudio` / `receiveVideo` / `publish` / `seek` / `pause`; the
/// §7.2 response names `_result` / `_error` / `onStatus`; plus the
/// pre-publish advisories `releaseStream` / `FCPublish` / `FCUnpublish`
/// that ride the same channel). A command whose name is NOT in this set
/// is a §7.2.1.2 `call` RPC, whose procedure name rides the
/// command-name field. (`call` itself is deliberately NOT matched:
/// on the wire an RPC's name field carries the procedure name, so a
/// literal "call" can only be a peer whose procedure is named "call" —
/// it surfaces as an ordinary RPC.)
pub fn is_reserved_command_name(name: &str) -> bool {
    matches!(
        name,
        "connect"
            | "close"
            | "createStream"
            | "play"
            | "play2"
            | "deleteStream"
            | "closeStream"
            | "receiveAudio"
            | "receiveVideo"
            | "publish"
            | "seek"
            | "pause"
            | "_result"
            | "_error"
            | "onStatus"
            | "releaseStream"
            | "FCPublish"
            | "FCUnpublish"
    )
}

/// A NetStream control command sent **by a client to the server**,
/// per RTMP 1.0 Commands-Messages §4.2. These are the subscriber-side
/// (play) and shared control commands a server receives on a NetStream
/// after `createStream`: `play`, `play2`, `pause`, `seek`,
/// `receiveAudio`, `receiveVideo`.
///
/// Every §4.2 command shares the same envelope on the wire — a
/// command-name String, a transaction-id Number (always 0 for these),
/// and a Null Command Object — followed by command-specific
/// arguments. [`NetStreamCommand::parse`] classifies an
/// already-decoded AMF0 command frame; [`NetStreamCommand::to_message`]
/// is the byte-level inverse, emitting the frame on the supplied
/// message stream id.
///
/// `play2` carries a single AMF object of parameters rather than flat
/// arguments (§4.2.2); the whole object is preserved verbatim as
/// [`NetStreamCommand::Play2`]'s payload so an unusual or
/// vendor-extended parameter set round-trips without loss.
#[derive(Debug, Clone, PartialEq)]
pub enum NetStreamCommand {
    /// §4.2.1 `play` — request a stream by name. `start` / `duration` /
    /// `reset` are optional trailing arguments (defaults −2 / −1 /
    /// `false` per spec) and are `None` when the client omitted them.
    Play {
        /// Stream name to play (may carry an `mp4:` / `mp3:` prefix).
        stream_name: String,
        /// Optional Start time in seconds (spec default −2).
        start: Option<f64>,
        /// Optional Duration of playback in seconds (spec default −1).
        duration: Option<f64>,
        /// Optional Reset flag — clear the queued playlist first.
        reset: Option<bool>,
    },
    /// §4.2.2 `play2` — switch bit-rate without a timeline change. The
    /// AMF parameter object is preserved as-is.
    Play2(Amf0Value),
    /// §4.2.7 `seek` — seek `milliseconds` into the playlist.
    Seek {
        /// Offset in milliseconds to seek to.
        milliseconds: f64,
    },
    /// §4.2.8 `pause` — pause (`true`) or resume (`false`) playback at
    /// the given current stream time in milliseconds.
    Pause {
        /// `true` to pause, `false` to resume.
        pause: bool,
        /// Current stream time in milliseconds at the pause point.
        milliseconds: f64,
    },
    /// §4.2.4 `receiveAudio` — tell the server whether to forward audio.
    ReceiveAudio(bool),
    /// §4.2.5 `receiveVideo` — tell the server whether to forward video.
    ReceiveVideo(bool),
}

impl NetStreamCommand {
    /// Classify an already-decoded AMF0 command frame (the output of
    /// [`amf::decode_all`](crate::amf::decode_all) on a type-20 /
    /// type-17 command message body). Returns `Ok(Some(cmd))` for a
    /// recognised §4.2 NetStream control command, `Ok(None)` for any
    /// other command name (e.g. `connect`, `publish`, `_result`,
    /// `closeStream`) — those are handled elsewhere and are not an
    /// error here.
    ///
    /// The leading three values are the §4.2 envelope (name,
    /// transaction id, Command Object); command-specific arguments
    /// follow. Missing optional trailing arguments are tolerated;
    /// a missing **required** argument is an [`Error::InvalidCommand`].
    pub fn parse(values: &[Amf0Value]) -> Result<Option<Self>> {
        let name = match values.first().and_then(Amf0Value::as_str) {
            Some(n) => n,
            None => return Ok(None),
        };
        // Command-specific arguments begin after name, transaction id,
        // and the (Null) Command Object.
        let args = values.get(3..).unwrap_or(&[]);
        let cmd =
            match name {
                "play" => {
                    let stream_name = args
                        .first()
                        .and_then(Amf0Value::as_str)
                        .ok_or_else(|| Error::InvalidCommand("`play` missing stream name".into()))?
                        .to_string();
                    NetStreamCommand::Play {
                        stream_name,
                        start: args.get(1).and_then(Amf0Value::as_f64),
                        duration: args.get(2).and_then(Amf0Value::as_f64),
                        reset: args.get(3).and_then(Amf0Value::as_bool),
                    }
                }
                "play2" => {
                    let params = args.first().cloned().ok_or_else(|| {
                        Error::InvalidCommand("`play2` missing parameters".into())
                    })?;
                    NetStreamCommand::Play2(params)
                }
                "seek" => {
                    let milliseconds =
                        args.first().and_then(Amf0Value::as_f64).ok_or_else(|| {
                            Error::InvalidCommand("`seek` missing milliSeconds".into())
                        })?;
                    NetStreamCommand::Seek { milliseconds }
                }
                "pause" => {
                    let pause = args.first().and_then(Amf0Value::as_bool).ok_or_else(|| {
                        Error::InvalidCommand("`pause` missing pause flag".into())
                    })?;
                    let milliseconds =
                        args.get(1).and_then(Amf0Value::as_f64).ok_or_else(|| {
                            Error::InvalidCommand("`pause` missing milliSeconds".into())
                        })?;
                    NetStreamCommand::Pause {
                        pause,
                        milliseconds,
                    }
                }
                "receiveAudio" => {
                    let flag = args.first().and_then(Amf0Value::as_bool).ok_or_else(|| {
                        Error::InvalidCommand("`receiveAudio` missing bool flag".into())
                    })?;
                    NetStreamCommand::ReceiveAudio(flag)
                }
                "receiveVideo" => {
                    let flag = args.first().and_then(Amf0Value::as_bool).ok_or_else(|| {
                        Error::InvalidCommand("`receiveVideo` missing bool flag".into())
                    })?;
                    NetStreamCommand::ReceiveVideo(flag)
                }
                _ => return Ok(None),
            };
        Ok(Some(cmd))
    }

    /// The §4.2 command name string this variant carries on the wire.
    pub fn command_name(&self) -> &'static str {
        match self {
            NetStreamCommand::Play { .. } => "play",
            NetStreamCommand::Play2(_) => "play2",
            NetStreamCommand::Seek { .. } => "seek",
            NetStreamCommand::Pause { .. } => "pause",
            NetStreamCommand::ReceiveAudio(_) => "receiveAudio",
            NetStreamCommand::ReceiveVideo(_) => "receiveVideo",
        }
    }

    /// Build the AMF0 command arguments (everything after the §4.2
    /// envelope: name + transaction id 0 + Null Command Object).
    fn args(&self) -> Vec<Amf0Value> {
        match self {
            NetStreamCommand::Play {
                stream_name,
                start,
                duration,
                reset,
            } => {
                // Trailing optionals are positional: a present later
                // field forces the earlier optionals to materialise
                // (spec defaults) so position is unambiguous.
                let mut out = vec![Amf0Value::String(stream_name.clone())];
                if start.is_some() || duration.is_some() || reset.is_some() {
                    out.push(Amf0Value::Number(start.unwrap_or(-2.0)));
                }
                if duration.is_some() || reset.is_some() {
                    out.push(Amf0Value::Number(duration.unwrap_or(-1.0)));
                }
                if let Some(r) = reset {
                    out.push(Amf0Value::Boolean(*r));
                }
                out
            }
            NetStreamCommand::Play2(params) => vec![params.clone()],
            NetStreamCommand::Seek { milliseconds } => vec![Amf0Value::Number(*milliseconds)],
            NetStreamCommand::Pause {
                pause,
                milliseconds,
            } => vec![Amf0Value::Boolean(*pause), Amf0Value::Number(*milliseconds)],
            NetStreamCommand::ReceiveAudio(flag) | NetStreamCommand::ReceiveVideo(flag) => {
                vec![Amf0Value::Boolean(*flag)]
            }
        }
    }

    /// Serialise this command to an AMF0 command [`Message`] on the
    /// given message stream id. Transaction id is 0 and the Command
    /// Object is Null, per §4.2 ("Transaction ID set to 0", "Command
    /// information object does not exist. Set to null type.").
    pub fn to_message(&self, stream_id: u32) -> Message {
        let payload = encode_command(self.command_name(), 0.0, Amf0Value::Null, &self.args());
        Message {
            msg_type_id: MSG_COMMAND_AMF0,
            msg_stream_id: stream_id,
            timestamp: 0,
            payload,
        }
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

    /// Wire bytes for an Abort Message (protocol control type 2, RTMP
    /// 1.0 §5.2): a bare 4-byte big-endian chunk stream ID on the
    /// control stream (`msg_stream_id == 0`, `timestamp == 0`).
    #[test]
    fn abort_wire_bytes() {
        let m = build_abort(0x0001_0203);
        assert_eq!(m.msg_type_id, MSG_ABORT);
        assert_eq!(m.msg_stream_id, 0);
        assert_eq!(m.timestamp, 0);
        assert_eq!(m.payload, vec![0x00, 0x01, 0x02, 0x03]);
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

    // ---- UserControlEvent typed accessor (parse + round-trip) -----------

    /// `UserControlEvent::parse` classifies each spec-defined event
    /// type into its strongly-typed variant. Spot-check all seven.
    #[test]
    fn user_control_event_parse_recognises_spec_types() {
        let cases: &[(&[u8], UserControlEvent)] = &[
            (
                &[0x00, 0x00, 0x00, 0x00, 0x00, 0x01],
                UserControlEvent::StreamBegin { stream_id: 1 },
            ),
            (
                &[0x00, 0x01, 0x00, 0x00, 0x00, 0x07],
                UserControlEvent::StreamEof { stream_id: 7 },
            ),
            (
                &[0x00, 0x02, 0x00, 0x10, 0x20, 0x30],
                UserControlEvent::StreamDry {
                    stream_id: 0x0010_2030,
                },
            ),
            (
                &[0x00, 0x03, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x0B, 0xB8],
                UserControlEvent::SetBufferLength {
                    stream_id: 1,
                    buffer_ms: 3000,
                },
            ),
            (
                &[0x00, 0x04, 0x00, 0x00, 0x00, 0x05],
                UserControlEvent::StreamIsRecorded { stream_id: 5 },
            ),
            (
                &[0x00, 0x06, 0xDE, 0xAD, 0xBE, 0xEF],
                UserControlEvent::PingRequest {
                    timestamp_ms: 0xDEAD_BEEF,
                },
            ),
            (
                &[0x00, 0x07, 0xDE, 0xAD, 0xBE, 0xEF],
                UserControlEvent::PingResponse {
                    timestamp_ms: 0xDEAD_BEEF,
                },
            ),
        ];
        for (wire, expected) in cases {
            let parsed = UserControlEvent::parse(wire).expect("parse UCM");
            assert_eq!(&parsed, expected);
            assert!(parsed.is_spec_defined());
            assert_eq!(
                parsed.event_type() as usize,
                ((wire[0] as usize) << 8) | wire[1] as usize
            );
        }
    }

    /// Parse → re-encode of each spec-defined builder output is
    /// byte-identical to the original. Locks the inverse property
    /// against accidental wire-format drift.
    #[test]
    fn user_control_event_round_trip_matches_builder_bytes() {
        let originals = [
            build_user_control_stream_begin(1),
            build_user_control_stream_eof(7),
            build_user_control_stream_dry(0x0010_2030),
            build_user_control_set_buffer_length(1, 3000),
            build_user_control_stream_is_recorded(5),
            build_user_control_ping_request(0xDEAD_BEEF),
            build_user_control_ping_response(0xDEAD_BEEF),
        ];
        for m in &originals {
            let parsed = UserControlEvent::parse(&m.payload).expect("parse UCM");
            let rebuilt = parsed.to_message();
            assert_eq!(rebuilt.msg_type_id, MSG_USER_CONTROL);
            assert_eq!(rebuilt.msg_stream_id, 0);
            assert_eq!(rebuilt.timestamp, 0);
            assert_eq!(rebuilt.payload, m.payload);
        }
    }

    /// UCM event type 5 (spec-reserved) and any value ≥ 8 surface as
    /// [`UserControlEvent::Unknown`] with the unconsumed tail bytes
    /// preserved verbatim. Round-tripping an `Unknown` rebuilds the
    /// exact same payload — forwarding ingests stay format-neutral.
    #[test]
    fn user_control_event_unknown_preserves_event_type_and_tail() {
        // §7.1.7 leaves event type 5 reserved.
        let wire: &[u8] = &[0x00, 0x05, 0xAA, 0xBB, 0xCC, 0xDD, 0xEE];
        let parsed = UserControlEvent::parse(wire).expect("parse reserved UCM");
        assert_eq!(
            parsed,
            UserControlEvent::Unknown {
                event_type: 5,
                data: vec![0xAA, 0xBB, 0xCC, 0xDD, 0xEE],
            },
        );
        assert!(!parsed.is_spec_defined());
        assert_eq!(parsed.event_type(), 5);
        // Round-trip: the rebuilt payload is byte-identical.
        let rebuilt = parsed.to_message();
        assert_eq!(rebuilt.payload, wire);

        // Forward-compat: any event type ≥ 8 also lands in Unknown,
        // even with an empty event-data tail (no truncation refusal).
        let future: &[u8] = &[0xFF, 0xFE];
        let parsed_future = UserControlEvent::parse(future).expect("parse future UCM");
        assert_eq!(
            parsed_future,
            UserControlEvent::Unknown {
                event_type: 0xFFFE,
                data: Vec::new(),
            },
        );
        assert_eq!(parsed_future.to_message().payload, future);
    }

    /// `parse` refuses payloads truncated below the 2-byte event-type
    /// header AND below the fixed event-data size of each spec-defined
    /// variant (`SetBufferLength` needs 8 bytes, every other
    /// spec-defined variant needs 4 bytes).
    #[test]
    fn user_control_event_parse_rejects_truncated_payload() {
        // < 2 bytes — can't even read the event type.
        assert!(matches!(
            UserControlEvent::parse(&[]),
            Err(Error::ProtocolViolation(_))
        ));
        assert!(matches!(
            UserControlEvent::parse(&[0x00]),
            Err(Error::ProtocolViolation(_))
        ));
        // event type present but spec-defined variant body truncated.
        for type_byte in [
            USR_STREAM_BEGIN,
            USR_STREAM_EOF,
            USR_STREAM_DRY,
            USR_STREAM_IS_RECORDED,
            USR_PING_REQUEST,
            USR_PING_RESPONSE,
        ] {
            let wire = [(type_byte >> 8) as u8, type_byte as u8, 0x00, 0x00, 0x00];
            assert!(matches!(
                UserControlEvent::parse(&wire),
                Err(Error::ProtocolViolation(_))
            ));
        }
        // SetBufferLength's 8-byte rule: 7 bytes refused, 8 accepted.
        let too_short = [0x00, 0x03, 0, 0, 0, 1, 0, 0, 11];
        assert!(matches!(
            UserControlEvent::parse(&too_short),
            Err(Error::ProtocolViolation(_))
        ));
    }

    /// `build_reconnect_request` shape per `enhanced-rtmp-v2.pdf`
    /// §"Reconnect Request": `["onStatus", 0.0, null, info]`, where
    /// info carries `code = NetConnection.Connect.ReconnectRequest`,
    /// `level = status`, plus the optional `tcUrl` / `description`
    /// pairs — and the command rides message stream 0 (NetConnection,
    /// not NetStream).
    #[test]
    fn reconnect_request_full_info_object() {
        let m = build_reconnect_request(
            Some("rtmp://foo.mydomain.com:1935/realtimeapp"),
            Some("The streaming server is undergoing updates."),
        );
        assert_eq!(m.msg_type_id, MSG_COMMAND_AMF0);
        assert_eq!(m.msg_stream_id, 0, "NetConnection command stream");
        let vals = crate::amf::decode_all(&m.payload).unwrap();
        assert_eq!(vals[0].as_str(), Some("onStatus"));
        assert_eq!(vals[1].as_f64(), Some(0.0), "transaction id 0");
        assert_eq!(vals[2], Amf0Value::Null, "no command object");
        let info = &vals[3];
        assert_eq!(
            info.get("code").and_then(Amf0Value::as_str),
            Some(RECONNECT_REQUEST_CODE)
        );
        assert_eq!(
            info.get("level").and_then(Amf0Value::as_str),
            Some("status")
        );
        assert_eq!(
            info.get("tcUrl").and_then(Amf0Value::as_str),
            Some("rtmp://foo.mydomain.com:1935/realtimeapp")
        );
        assert_eq!(
            info.get("description").and_then(Amf0Value::as_str),
            Some("The streaming server is undergoing updates.")
        );
    }

    /// Both Info Object extras are optional per spec — when neither is
    /// supplied the info object carries exactly the two mandatory
    /// pairs (`code`, `level`).
    #[test]
    fn reconnect_request_minimal_info_object() {
        let m = build_reconnect_request(None, None);
        let vals = crate::amf::decode_all(&m.payload).unwrap();
        let info = &vals[3];
        assert_eq!(
            info.get("code").and_then(Amf0Value::as_str),
            Some(RECONNECT_REQUEST_CODE)
        );
        assert_eq!(
            info.get("level").and_then(Amf0Value::as_str),
            Some("status")
        );
        assert!(info.get("tcUrl").is_none(), "tcUrl omitted when None");
        assert!(
            info.get("description").is_none(),
            "description omitted when None"
        );
    }

    /// Round-trip a NetStreamCommand through `to_message` → decode →
    /// `parse` and confirm the typed value is preserved.
    fn round_trip(cmd: NetStreamCommand) {
        let m = cmd.to_message(1);
        assert_eq!(m.msg_type_id, MSG_COMMAND_AMF0);
        assert_eq!(m.msg_stream_id, 1);
        let vals = crate::amf::decode_all(&m.payload).unwrap();
        // §4.2 envelope: name, transaction id 0, Null Command Object.
        assert_eq!(vals[0].as_str(), Some(cmd.command_name()));
        assert_eq!(vals[1].as_f64(), Some(0.0));
        assert!(matches!(vals[2], Amf0Value::Null));
        let parsed = NetStreamCommand::parse(&vals).unwrap().unwrap();
        assert_eq!(parsed, cmd);
    }

    #[test]
    fn netstream_play_full_round_trip() {
        round_trip(NetStreamCommand::Play {
            stream_name: "mp4:sample.m4v".into(),
            start: Some(-2.0),
            duration: Some(-1.0),
            reset: Some(true),
        });
    }

    /// A `play` with only the stream name omits the optional trailing
    /// fields on the wire and parses them back as `None`.
    #[test]
    fn netstream_play_name_only() {
        let cmd = NetStreamCommand::Play {
            stream_name: "live".into(),
            start: None,
            duration: None,
            reset: None,
        };
        let m = cmd.to_message(1);
        let vals = crate::amf::decode_all(&m.payload).unwrap();
        // name + txn + null + stream name = exactly 4 values.
        assert_eq!(vals.len(), 4);
        round_trip(cmd);
    }

    /// `reset` present forces `start`/`duration` to materialise at
    /// their spec defaults so positional parsing stays unambiguous.
    #[test]
    fn netstream_play_reset_forces_defaults() {
        let cmd = NetStreamCommand::Play {
            stream_name: "s".into(),
            start: None,
            duration: None,
            reset: Some(false),
        };
        let m = cmd.to_message(1);
        let vals = crate::amf::decode_all(&m.payload).unwrap();
        assert_eq!(vals[3].as_str(), Some("s"));
        assert_eq!(vals[4].as_f64(), Some(-2.0)); // start default
        assert_eq!(vals[5].as_f64(), Some(-1.0)); // duration default
        assert_eq!(vals[6].as_bool(), Some(false));
    }

    #[test]
    fn netstream_pause_round_trip() {
        round_trip(NetStreamCommand::Pause {
            pause: true,
            milliseconds: 4096.0,
        });
        round_trip(NetStreamCommand::Pause {
            pause: false,
            milliseconds: 0.0,
        });
    }

    #[test]
    fn netstream_seek_round_trip() {
        round_trip(NetStreamCommand::Seek {
            milliseconds: 12345.0,
        });
    }

    #[test]
    fn netstream_receive_audio_video_round_trip() {
        round_trip(NetStreamCommand::ReceiveAudio(false));
        round_trip(NetStreamCommand::ReceiveAudio(true));
        round_trip(NetStreamCommand::ReceiveVideo(false));
        round_trip(NetStreamCommand::ReceiveVideo(true));
    }

    #[test]
    fn netstream_play2_preserves_param_object() {
        let params = Amf0Value::Object(vec![
            ("len".into(), Amf0Value::Number(-1.0)),
            ("offset".into(), Amf0Value::Number(0.0)),
            ("start".into(), Amf0Value::Number(0.0)),
            ("streamName".into(), Amf0Value::String("hi".into())),
            ("transition".into(), Amf0Value::String("switch".into())),
        ]);
        round_trip(NetStreamCommand::Play2(params));
    }

    /// A non-NetStream command name (`connect`, `_result`, teardown,
    /// …) is not a §4.2 control command: `parse` returns `Ok(None)`
    /// rather than erroring, so the server's teardown / connect paths
    /// keep handling it.
    #[test]
    fn netstream_parse_ignores_other_commands() {
        for name in [
            "connect",
            "_result",
            "createStream",
            "closeStream",
            "publish",
        ] {
            let vals = vec![
                Amf0Value::String(name.into()),
                Amf0Value::Number(1.0),
                Amf0Value::Null,
            ];
            assert!(NetStreamCommand::parse(&vals).unwrap().is_none());
        }
    }

    /// A recognised command name missing a required argument is a hard
    /// `InvalidCommand`, not a silently-dropped frame.
    #[test]
    fn netstream_parse_missing_required_arg_errors() {
        // `seek` with no milliSeconds.
        let vals = vec![
            Amf0Value::String("seek".into()),
            Amf0Value::Number(0.0),
            Amf0Value::Null,
        ];
        assert!(matches!(
            NetStreamCommand::parse(&vals),
            Err(Error::InvalidCommand(_))
        ));
    }

    /// An empty / nameless frame is `Ok(None)` (no command name).
    #[test]
    fn netstream_parse_empty_is_none() {
        assert!(NetStreamCommand::parse(&[]).unwrap().is_none());
    }

    // ----- §5.4.5 Set Peer Bandwidth limit types -----

    #[test]
    fn peer_bandwidth_hard_adopts_and_dedupes() {
        let mut l = PeerBandwidthLimiter::new();
        assert_eq!(
            l.apply(2_500_000, PEER_BANDWIDTH_LIMIT_HARD),
            Some(2_500_000)
        );
        // Same window again — no change, no reply owed.
        assert_eq!(l.apply(2_500_000, PEER_BANDWIDTH_LIMIT_HARD), None);
        assert_eq!(
            l.apply(1_000_000, PEER_BANDWIDTH_LIMIT_HARD),
            Some(1_000_000)
        );
        assert_eq!(l.window(), Some(1_000_000));
    }

    #[test]
    fn peer_bandwidth_soft_takes_smaller_of_indicated_and_in_effect() {
        let mut l = PeerBandwidthLimiter::new();
        assert_eq!(
            l.apply(1_000_000, PEER_BANDWIDTH_LIMIT_HARD),
            Some(1_000_000)
        );
        // Soft with a larger window: the limit already in effect wins.
        assert_eq!(l.apply(5_000_000, PEER_BANDWIDTH_LIMIT_SOFT), None);
        assert_eq!(l.window(), Some(1_000_000));
        // Soft with a smaller window: the indicated window wins.
        assert_eq!(l.apply(500_000, PEER_BANDWIDTH_LIMIT_SOFT), Some(500_000));
        // Soft with no limit in effect adopts the indicated window.
        let mut fresh = PeerBandwidthLimiter::new();
        assert_eq!(
            fresh.apply(750_000, PEER_BANDWIDTH_LIMIT_SOFT),
            Some(750_000)
        );
    }

    #[test]
    fn peer_bandwidth_dynamic_only_after_hard() {
        // "If the previous Limit Type was Hard, treat this message as
        // though it was marked Hard, otherwise ignore this message."
        let mut l = PeerBandwidthLimiter::new();
        // No previous type at all — ignored.
        assert_eq!(l.apply(9_000_000, PEER_BANDWIDTH_LIMIT_DYNAMIC), None);
        assert_eq!(l.window(), None);
        // After Hard — treated as Hard.
        l.apply(1_000_000, PEER_BANDWIDTH_LIMIT_HARD);
        assert_eq!(
            l.apply(2_000_000, PEER_BANDWIDTH_LIMIT_DYNAMIC),
            Some(2_000_000)
        );
        // A Dynamic-as-Hard keeps the Hard latch for the next Dynamic.
        assert_eq!(
            l.apply(3_000_000, PEER_BANDWIDTH_LIMIT_DYNAMIC),
            Some(3_000_000)
        );
        // After Soft — ignored.
        l.apply(500_000, PEER_BANDWIDTH_LIMIT_SOFT);
        assert_eq!(l.apply(9_000_000, PEER_BANDWIDTH_LIMIT_DYNAMIC), None);
        assert_eq!(l.window(), Some(500_000));
    }

    #[test]
    fn peer_bandwidth_reserved_limit_types_ignored() {
        let mut l = PeerBandwidthLimiter::new();
        assert_eq!(l.apply(1_000_000, 3), None);
        assert_eq!(l.apply(1_000_000, 0xFF), None);
        assert_eq!(l.window(), None);
    }

    #[test]
    fn parse_set_peer_bandwidth_payload_shapes() {
        // Spec 5-byte shape.
        let msg = build_set_peer_bandwidth(2_500_000, PEER_BANDWIDTH_LIMIT_SOFT);
        assert_eq!(
            parse_set_peer_bandwidth(&msg.payload).unwrap(),
            (2_500_000, PEER_BANDWIDTH_LIMIT_SOFT)
        );
        // Missing limit byte tolerated as Hard.
        assert_eq!(
            parse_set_peer_bandwidth(&2_500_000u32.to_be_bytes()).unwrap(),
            (2_500_000, PEER_BANDWIDTH_LIMIT_HARD)
        );
        // Short payload is a clean error.
        assert!(parse_set_peer_bandwidth(&[0, 1, 2]).is_err());
    }
}
