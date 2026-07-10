//! RTMP Shared Object message bodies (message types 19 = AMF0 and
//! 16 = AMF3).
//!
//! A Shared Object (SO) is a named, versioned property bag replicated
//! between client and server. The RTMP 1.0 spec names the message and
//! its event codes but not the body layout; the byte-level framing
//! implemented here follows
//! `docs/streaming/rtmp/rtmp-so-dataframe-digest-handshake.md` §1:
//!
//! ```text
//!   UI16  name length          ┐
//!   ...   name (UTF-8, bare)   │ SO header
//!   UI32  current version      │
//!   UI32  flags (bit 1 = persistent)
//!   4 B   reserved (zero)      ┘
//!   then, until the message body is exhausted:
//!   UI8   event type
//!   UI32  event data length
//!   ...   event data
//! ```
//!
//! Within event data, *property / handler names* are bare
//! length-prefixed UTF-8 strings (UI16 length + bytes, **no** AMF type
//! marker — the AMF0 object-key encoding), while *values* are full
//! AMF-typed values: AMF0 markers for message type 19, AMF3 markers
//! for message type 16. The two message types differ **only** in the
//! value serialization, which is why [`SharedObjectMessage`] is
//! generic over the value type ([`Amf0Value`] / [`Amf3Value`]).
//!
//! Parsing is lenient the same way the rest of this crate is: an
//! event's payload boundary comes from its length field, so unused
//! trailing bytes inside a well-framed event are skipped, and event
//! types outside the documented 1..=11 range are preserved verbatim as
//! [`SoEvent::Unknown`] so a relay can round-trip them. Truncation —
//! anywhere a declared length overruns the actual bytes — is a hard
//! error.
//!
//! AMF3 note: each event value is encoded/decoded as a self-contained
//! AMF3 value (fresh reference tables), matching how this module emits
//! them; SO messages observed in the wild carry small scalar values
//! where reference tables never come into play.

use crate::amf::{self, Amf0Value};
use crate::amf3::{self, Amf3Value};
use crate::chunk::Message;
use crate::error::{Error, Result};
use crate::message::{MSG_SHARED_OBJECT_AMF0, MSG_SHARED_OBJECT_AMF3};

// §1.3 event type codes.
/// Client subscribes to / opens the named SO.
pub const SO_EVENT_USE: u8 = 1;
/// Client detaches from the SO.
pub const SO_EVENT_RELEASE: u8 = 2;
/// Client asks the server to set a property to a value.
pub const SO_EVENT_REQUEST_CHANGE: u8 = 3;
/// Server broadcast: one or more properties changed.
pub const SO_EVENT_CHANGE: u8 = 4;
/// Server acknowledges a client's Request Change.
pub const SO_EVENT_SUCCESS: u8 = 5;
/// Named-handler invocation with arguments (`SharedObject.send`).
pub const SO_EVENT_SEND_MESSAGE: u8 = 6;
/// Server delivers a status/error (code + level).
pub const SO_EVENT_STATUS: u8 = 7;
/// Server instructs the client to drop all local properties.
pub const SO_EVENT_CLEAR: u8 = 8;
/// Server broadcast: a named property was deleted.
pub const SO_EVENT_REMOVE: u8 = 9;
/// Client asks the server to delete a named property.
pub const SO_EVENT_REQUEST_REMOVE: u8 = 10;
/// Server acknowledges a client's Use.
pub const SO_EVENT_USE_SUCCESS: u8 = 11;

/// §1.1 flags field: bit 1 marks the SO persistent (mirrors the
/// original `persistence` argument); `0` = non-persistent (session)
/// SO. Remaining bits are reserved.
pub const SO_FLAG_PERSISTENT: u32 = 2;

/// One Shared Object event (§1.2–§1.4), generic over the AMF value
/// flavour (AMF0 for message type 19, AMF3 for message type 16).
#[derive(Debug, Clone, PartialEq)]
pub enum SoEvent<V> {
    /// (1) Open/subscribe. No payload.
    Use,
    /// (2) Detach. No payload.
    Release,
    /// (3) Client proposes a new value for one property.
    RequestChange { name: String, value: V },
    /// (4) Authoritative new values to apply — repeated name/value
    /// pairs until the event length is consumed.
    Change { pairs: Vec<(String, V)> },
    /// (5) Ack of a committed Request Change. Some servers ack with an
    /// empty payload — that parses (and re-encodes) as an empty `name`.
    Success { name: String },
    /// (6) Named-handler RPC: handler name + zero or more AMF argument
    /// values.
    SendMessage { handler: String, args: Vec<V> },
    /// (7) Status/error delivery: `code` (e.g.
    /// `"SharedObject.NoReadAccess"`) then `level` (e.g. `"error"`) —
    /// both bare strings, in that wire order.
    Status { code: String, level: String },
    /// (8) Drop the whole local replica (full resync). No payload.
    Clear,
    /// (9) A named property was deleted; remove it from the replica.
    Remove { name: String },
    /// (10) Client asks for a named property to be deleted.
    RequestRemove { name: String },
    /// (11) Ack of a Use. No payload.
    UseSuccess,
    /// Any event code outside 1..=11 — payload preserved verbatim so a
    /// relay round-trips events it doesn't understand.
    Unknown { event_type: u8, data: Vec<u8> },
}

impl<V> SoEvent<V> {
    /// The wire event-type code (§1.3).
    pub fn event_type(&self) -> u8 {
        match self {
            SoEvent::Use => SO_EVENT_USE,
            SoEvent::Release => SO_EVENT_RELEASE,
            SoEvent::RequestChange { .. } => SO_EVENT_REQUEST_CHANGE,
            SoEvent::Change { .. } => SO_EVENT_CHANGE,
            SoEvent::Success { .. } => SO_EVENT_SUCCESS,
            SoEvent::SendMessage { .. } => SO_EVENT_SEND_MESSAGE,
            SoEvent::Status { .. } => SO_EVENT_STATUS,
            SoEvent::Clear => SO_EVENT_CLEAR,
            SoEvent::Remove { .. } => SO_EVENT_REMOVE,
            SoEvent::RequestRemove { .. } => SO_EVENT_REQUEST_REMOVE,
            SoEvent::UseSuccess => SO_EVENT_USE_SUCCESS,
            SoEvent::Unknown { event_type, .. } => *event_type,
        }
    }
}

/// One decoded Shared Object message: the §1.1 header plus the
/// back-to-back event sequence. Generic over the AMF value flavour —
/// [`Amf0Value`] rides message type 19, [`Amf3Value`] message type 16.
#[derive(Debug, Clone, PartialEq)]
pub struct SharedObjectMessage<V> {
    /// SO name (bare UTF-8 in the header, no AMF marker).
    pub name: String,
    /// Version counter, incremented by the authoritative side on each
    /// committed change; orders/deduplicates updates.
    pub version: u32,
    /// Persistence & scope flags — see [`SO_FLAG_PERSISTENT`]. The
    /// header's 4 trailing reserved bytes are consumed on parse and
    /// emitted as zero on build.
    pub flags: u32,
    /// The events, in wire order.
    pub events: Vec<SoEvent<V>>,
}

impl<V> SharedObjectMessage<V> {
    /// A fresh non-persistent message shell for `name` (version 0, no
    /// events).
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            version: 0,
            flags: 0,
            events: Vec::new(),
        }
    }

    /// Whether the flags mark the SO persistent.
    pub fn is_persistent(&self) -> bool {
        self.flags & SO_FLAG_PERSISTENT != 0
    }
}

// ---------------------------------------------------------------------------
// Bare (marker-less) UTF-8 strings — the AMF0 object-key encoding.
// ---------------------------------------------------------------------------

fn read_bare_string(buf: &[u8], pos: &mut usize) -> Result<String> {
    if buf.len() < *pos + 2 {
        return Err(so_err("truncated bare-string length"));
    }
    let len = u16::from_be_bytes([buf[*pos], buf[*pos + 1]]) as usize;
    *pos += 2;
    if buf.len() < *pos + len {
        return Err(so_err("bare string overruns event data"));
    }
    let s = std::str::from_utf8(&buf[*pos..*pos + len])
        .map_err(|_| so_err("bare string is not valid UTF-8"))?
        .to_owned();
    *pos += len;
    Ok(s)
}

fn write_bare_string(out: &mut Vec<u8>, s: &str) -> Result<()> {
    let len: u16 = s
        .len()
        .try_into()
        .map_err(|_| so_err("bare string longer than 65535 bytes"))?;
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(s.as_bytes());
    Ok(())
}

fn so_err(msg: &str) -> Error {
    Error::ProtocolViolation(format!("shared object: {msg}"))
}

// ---------------------------------------------------------------------------
// Generic parse / build core
// ---------------------------------------------------------------------------

type DecodeValue<'a, V> = &'a mut dyn FnMut(&[u8], &mut usize) -> Result<V>;
type EncodeValue<'a, V> = &'a mut dyn FnMut(&mut Vec<u8>, &V);

fn parse_events<V>(
    data: &[u8],
    event_type: u8,
    decode_value: DecodeValue<'_, V>,
) -> Result<SoEvent<V>> {
    let mut pos = 0usize;
    let ev = match event_type {
        SO_EVENT_USE => SoEvent::Use,
        SO_EVENT_RELEASE => SoEvent::Release,
        SO_EVENT_REQUEST_CHANGE => {
            let name = read_bare_string(data, &mut pos)?;
            let value = decode_value(data, &mut pos)?;
            SoEvent::RequestChange { name, value }
        }
        SO_EVENT_CHANGE => {
            let mut pairs = Vec::new();
            while pos < data.len() {
                let name = read_bare_string(data, &mut pos)?;
                let value = decode_value(data, &mut pos)?;
                pairs.push((name, value));
            }
            SoEvent::Change { pairs }
        }
        SO_EVENT_SUCCESS => {
            // Some servers ack with an entirely empty payload.
            let name = if data.is_empty() {
                String::new()
            } else {
                read_bare_string(data, &mut pos)?
            };
            SoEvent::Success { name }
        }
        SO_EVENT_SEND_MESSAGE => {
            let handler = read_bare_string(data, &mut pos)?;
            let mut args = Vec::new();
            while pos < data.len() {
                args.push(decode_value(data, &mut pos)?);
            }
            SoEvent::SendMessage { handler, args }
        }
        SO_EVENT_STATUS => {
            let code = read_bare_string(data, &mut pos)?;
            let level = read_bare_string(data, &mut pos)?;
            SoEvent::Status { code, level }
        }
        SO_EVENT_CLEAR => SoEvent::Clear,
        SO_EVENT_REMOVE => {
            let name = read_bare_string(data, &mut pos)?;
            SoEvent::Remove { name }
        }
        SO_EVENT_REQUEST_REMOVE => {
            let name = read_bare_string(data, &mut pos)?;
            SoEvent::RequestRemove { name }
        }
        SO_EVENT_USE_SUCCESS => SoEvent::UseSuccess,
        other => SoEvent::Unknown {
            event_type: other,
            data: data.to_vec(),
        },
    };
    // Trailing bytes inside a well-framed event are tolerated (the
    // event-length field is authoritative), matching lenient decode
    // elsewhere in the crate.
    Ok(ev)
}

fn build_event<V>(event: &SoEvent<V>, encode_value: EncodeValue<'_, V>) -> Result<Vec<u8>> {
    let mut data = Vec::new();
    match event {
        SoEvent::Use | SoEvent::Release | SoEvent::Clear | SoEvent::UseSuccess => {}
        SoEvent::RequestChange { name, value } => {
            write_bare_string(&mut data, name)?;
            encode_value(&mut data, value);
        }
        SoEvent::Change { pairs } => {
            for (name, value) in pairs {
                write_bare_string(&mut data, name)?;
                encode_value(&mut data, value);
            }
        }
        SoEvent::Success { name } => {
            // Empty name round-trips as the empty-payload server ack.
            if !name.is_empty() {
                write_bare_string(&mut data, name)?;
            }
        }
        SoEvent::SendMessage { handler, args } => {
            write_bare_string(&mut data, handler)?;
            for arg in args {
                encode_value(&mut data, arg);
            }
        }
        SoEvent::Status { code, level } => {
            write_bare_string(&mut data, code)?;
            write_bare_string(&mut data, level)?;
        }
        SoEvent::Remove { name } | SoEvent::RequestRemove { name } => {
            write_bare_string(&mut data, name)?;
        }
        SoEvent::Unknown { data: raw, .. } => data.extend_from_slice(raw),
    }
    Ok(data)
}

fn parse_message<V>(
    payload: &[u8],
    decode_value: DecodeValue<'_, V>,
) -> Result<SharedObjectMessage<V>> {
    // §1.1 header.
    if payload.len() < 2 {
        return Err(so_err("truncated header (name length)"));
    }
    let name_len = u16::from_be_bytes([payload[0], payload[1]]) as usize;
    let mut pos = 2usize;
    if payload.len() < pos + name_len {
        return Err(so_err("SO name overruns message body"));
    }
    let name = std::str::from_utf8(&payload[pos..pos + name_len])
        .map_err(|_| so_err("SO name is not valid UTF-8"))?
        .to_owned();
    pos += name_len;
    if payload.len() < pos + 12 {
        return Err(so_err("truncated header (version/flags/reserved)"));
    }
    let version = u32::from_be_bytes([
        payload[pos],
        payload[pos + 1],
        payload[pos + 2],
        payload[pos + 3],
    ]);
    let flags = u32::from_be_bytes([
        payload[pos + 4],
        payload[pos + 5],
        payload[pos + 6],
        payload[pos + 7],
    ]);
    // 4 reserved bytes consumed without interpretation.
    pos += 12;

    // §1.2 events until the message body is exhausted.
    let mut events = Vec::new();
    while pos < payload.len() {
        if payload.len() < pos + 5 {
            return Err(so_err("truncated event header"));
        }
        let event_type = payload[pos];
        let data_len = u32::from_be_bytes([
            payload[pos + 1],
            payload[pos + 2],
            payload[pos + 3],
            payload[pos + 4],
        ]) as usize;
        pos += 5;
        if payload.len() < pos + data_len {
            return Err(so_err("event data overruns message body"));
        }
        events.push(parse_events(
            &payload[pos..pos + data_len],
            event_type,
            decode_value,
        )?);
        pos += data_len;
    }

    Ok(SharedObjectMessage {
        name,
        version,
        flags,
        events,
    })
}

fn build_message<V>(
    msg: &SharedObjectMessage<V>,
    encode_value: EncodeValue<'_, V>,
) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    write_bare_string(&mut out, &msg.name)?;
    out.extend_from_slice(&msg.version.to_be_bytes());
    out.extend_from_slice(&msg.flags.to_be_bytes());
    out.extend_from_slice(&[0u8; 4]); // reserved
    for event in &msg.events {
        let data = build_event(event, encode_value)?;
        let data_len: u32 = data
            .len()
            .try_into()
            .map_err(|_| so_err("event data longer than u32"))?;
        out.push(event.event_type());
        out.extend_from_slice(&data_len.to_be_bytes());
        out.extend_from_slice(&data);
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// AMF0 (message type 19) surface
// ---------------------------------------------------------------------------

impl SharedObjectMessage<Amf0Value> {
    /// Parse a message-type-19 body.
    pub fn parse_amf0(payload: &[u8]) -> Result<Self> {
        parse_message(payload, &mut |buf, pos| amf::decode(buf, pos))
    }

    /// Serialize as a message-type-19 body.
    pub fn build_amf0(&self) -> Result<Vec<u8>> {
        build_message(self, &mut |out, v| amf::encode(out, v))
    }

    /// Wrap [`build_amf0`](Self::build_amf0) into a ready-to-send
    /// [`Message`] (type 19). SO traffic conventionally rides the
    /// NetConnection control stream, so pass `msg_stream_id = 0`
    /// unless your peer expects otherwise.
    pub fn to_message_amf0(&self, msg_stream_id: u32) -> Result<Message> {
        Ok(Message {
            msg_type_id: MSG_SHARED_OBJECT_AMF0,
            msg_stream_id,
            timestamp: 0,
            payload: self.build_amf0()?,
        })
    }
}

// ---------------------------------------------------------------------------
// AMF3 (message type 16) surface
// ---------------------------------------------------------------------------

impl SharedObjectMessage<Amf3Value> {
    /// Parse a message-type-16 body (identical framing, AMF3 values).
    pub fn parse_amf3(payload: &[u8]) -> Result<Self> {
        parse_message(payload, &mut |buf, pos| amf3::decode(buf, pos))
    }

    /// Serialize as a message-type-16 body.
    pub fn build_amf3(&self) -> Result<Vec<u8>> {
        build_message(self, &mut |out, v| amf3::encode(out, v))
    }

    /// Wrap [`build_amf3`](Self::build_amf3) into a ready-to-send
    /// [`Message`] (type 16). See
    /// [`to_message_amf0`](SharedObjectMessage::<Amf0Value>::to_message_amf0)
    /// for the stream-id convention.
    pub fn to_message_amf3(&self, msg_stream_id: u32) -> Result<Message> {
        Ok(Message {
            msg_type_id: MSG_SHARED_OBJECT_AMF3,
            msg_stream_id,
            timestamp: 0,
            payload: self.build_amf3()?,
        })
    }

    /// Bridge every event value onto the AMF0 shape (via
    /// [`Amf3Value::to_amf0`]) so AMF3 SO messages flow through the
    /// same consumer path as AMF0 ones.
    pub fn to_amf0(&self) -> SharedObjectMessage<Amf0Value> {
        let map_pairs = |pairs: &[(String, Amf3Value)]| {
            pairs
                .iter()
                .map(|(k, v)| (k.clone(), v.to_amf0()))
                .collect()
        };
        SharedObjectMessage {
            name: self.name.clone(),
            version: self.version,
            flags: self.flags,
            events: self
                .events
                .iter()
                .map(|e| match e {
                    SoEvent::Use => SoEvent::Use,
                    SoEvent::Release => SoEvent::Release,
                    SoEvent::RequestChange { name, value } => SoEvent::RequestChange {
                        name: name.clone(),
                        value: value.to_amf0(),
                    },
                    SoEvent::Change { pairs } => SoEvent::Change {
                        pairs: map_pairs(pairs),
                    },
                    SoEvent::Success { name } => SoEvent::Success { name: name.clone() },
                    SoEvent::SendMessage { handler, args } => SoEvent::SendMessage {
                        handler: handler.clone(),
                        args: args.iter().map(Amf3Value::to_amf0).collect(),
                    },
                    SoEvent::Status { code, level } => SoEvent::Status {
                        code: code.clone(),
                        level: level.clone(),
                    },
                    SoEvent::Clear => SoEvent::Clear,
                    SoEvent::Remove { name } => SoEvent::Remove { name: name.clone() },
                    SoEvent::RequestRemove { name } => {
                        SoEvent::RequestRemove { name: name.clone() }
                    }
                    SoEvent::UseSuccess => SoEvent::UseSuccess,
                    SoEvent::Unknown { event_type, data } => SoEvent::Unknown {
                        event_type: *event_type,
                        data: data.clone(),
                    },
                })
                .collect(),
        }
    }
}

/// Parse either Shared Object message flavour out of a [`Message`],
/// bridging AMF3 values onto the AMF0 shape — the one-stop entry point
/// for consumers that don't care which encoding the peer picked.
pub fn parse_shared_object(msg: &Message) -> Result<SharedObjectMessage<Amf0Value>> {
    match msg.msg_type_id {
        MSG_SHARED_OBJECT_AMF0 => SharedObjectMessage::parse_amf0(&msg.payload),
        MSG_SHARED_OBJECT_AMF3 => {
            Ok(SharedObjectMessage::<Amf3Value>::parse_amf3(&msg.payload)?.to_amf0())
        }
        other => Err(so_err(&format!(
            "message type {other} is not a shared-object message"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Golden bytes straight from the §1.1/§1.2 tables: header +
    /// one Change event with one string property.
    #[test]
    fn parse_golden_change_message() {
        let mut body = Vec::new();
        body.extend_from_slice(&[0x00, 0x05]); // name length
        body.extend_from_slice(b"lobby"); // name
        body.extend_from_slice(&[0x00, 0x00, 0x00, 0x07]); // version 7
        body.extend_from_slice(&[0x00, 0x00, 0x00, 0x02]); // flags: persistent
        body.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]); // reserved
        body.push(SO_EVENT_CHANGE); // event type 4
                                    // event data: "topic" + AMF0 string "hello"
        let mut ev = Vec::new();
        ev.extend_from_slice(&[0x00, 0x05]);
        ev.extend_from_slice(b"topic");
        ev.push(0x02); // AMF0 string marker
        ev.extend_from_slice(&[0x00, 0x05]);
        ev.extend_from_slice(b"hello");
        body.extend_from_slice(&(ev.len() as u32).to_be_bytes());
        body.extend_from_slice(&ev);

        let so = SharedObjectMessage::parse_amf0(&body).expect("parse");
        assert_eq!(so.name, "lobby");
        assert_eq!(so.version, 7);
        assert!(so.is_persistent());
        assert_eq!(
            so.events,
            vec![SoEvent::Change {
                pairs: vec![("topic".to_owned(), Amf0Value::String("hello".to_owned()))],
            }]
        );

        // And the builder regenerates the exact bytes.
        assert_eq!(so.build_amf0().expect("build"), body);
    }
}
