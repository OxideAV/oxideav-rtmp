//! RTMP chunk stream reader + writer.
//!
//! RTMP splits every message (command / audio / video / control) into
//! one or more fixed-size **chunks**. Each chunk has a 1..=3 byte basic
//! header carrying `(fmt, csid)`, a 0..=11 byte message header whose
//! shape depends on `fmt`, an optional 4-byte extended timestamp, and
//! the payload. Peers negotiate the max per-chunk payload via the
//! `SetChunkSize` protocol control message (default 128, typically
//! bumped to 4096+ after connect).
//!
//! This module is the minimum needed to round-trip real RTMP traffic:
//!
//! * Reader coalesces chunks back into whole `Message`s, auto-handling
//!   fmt 0 / 1 / 2 / 3, extended timestamps, and the per-csid state
//!   tables.
//! * Writer emits messages as a sequence of chunks, preferring the
//!   densest fmt that carries the same `(msg_length, msg_type_id,
//!   msg_stream_id)` as the previous message on that csid (fmt 3 if
//!   nothing changed, else fmt 2 / 1 / 0).
//! * `SetChunkSize` is applied live on the reader when observed, and
//!   exposed as a setter on the writer so callers can acknowledge it
//!   to the peer.
//!
//! Chunk stream id (csid) conventions we use:
//! * `2` — protocol control (SetChunkSize, Ack, WindowAckSize, SetPeerBandwidth, UserControl)
//! * `3` — AMF command messages (connect, createStream, publish, …)
//! * `4` — audio messages
//! * `5` — video messages
//! * `6` — data messages (@setDataFrame / onMetaData)

use std::collections::HashMap;
use std::io::{Read, Write};

use crate::error::{Error, Result};

/// Default max per-chunk payload size, applied until either side
/// sends a `SetChunkSize` message (RTMP spec §5.4.1).
pub const DEFAULT_CHUNK_SIZE: usize = 128;

/// Max value RTMP's `SetChunkSize` field can legally carry (spec:
/// 1..=16_777_215, top bit reserved).
pub const MAX_CHUNK_SIZE: usize = 0x00FF_FFFF;

/// One fully reassembled RTMP message, after chunk-header removal.
#[derive(Debug, Clone)]
pub struct Message {
    pub msg_type_id: u8,
    pub msg_stream_id: u32,
    /// Absolute timestamp in the message-type-specific unit (ms for
    /// audio / video / data / command).
    pub timestamp: u32,
    pub payload: Vec<u8>,
}

/// Typed classification of a message's `msg_stream_id` per Message
/// Formats spec §5 ("Protocol Control Messages MUST have message
/// stream ID 0 (called as control stream)") and §4.1 (3-byte stream
/// ID field).
///
/// The numeric NetStream ids 1..=`0x00FF_FFFF` are the values a server
/// returns from `_result(createStream)`; per the RTMP Commands Messages
/// spec §4.1.3 a freshly created NetStream receives "a stream ID" that
/// the publisher then stamps into every subsequent A/V / metadata
/// message header. The chunk message-stream-id field on the wire is
/// 32-bit little-endian (RTMP Chunk Stream §6.1.2.1) but the §4.1
/// message header layout only allocates 3 bytes for it; values whose
/// top byte is non-zero are reserved and surface here as
/// [`MessageStreamKind::Reserved`] so a caller can refuse them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageStreamKind {
    /// `msg_stream_id == 0` — the "control stream" carrying
    /// NetConnection commands (`connect`, `createStream`, `_result`,
    /// `_error`, `call`) and the protocol-control / user-control
    /// messages (types 1..=6).
    Control,
    /// A NetStream handle (1..=`0x00FF_FFFF`) — the value returned by
    /// `_result(createStream)`, stamped into every audio / video /
    /// data / aggregate message that flows on that NetStream.
    NetStream(u32),
    /// `msg_stream_id` has bit(s) set in the top byte, outside the
    /// §4.1 3-byte field. RTMP Chunk Stream §6.1.2.1 carries the
    /// field as a 32-bit value so receivers see it on the wire, but
    /// the Message Formats spec §4.1 reserves the high byte. Surfaced
    /// so a strict consumer can refuse the message.
    Reserved(u32),
}

impl Message {
    /// Classify [`Self::msg_stream_id`] per Message Formats spec §4.1 /
    /// §5 — `0` is the "control stream", `1..=0x00FF_FFFF` is a
    /// NetStream handle, anything with bits set above the §4.1 3-byte
    /// field is reserved.
    pub fn stream_kind(&self) -> MessageStreamKind {
        match self.msg_stream_id {
            0 => MessageStreamKind::Control,
            id if id & 0xFF00_0000 == 0 => MessageStreamKind::NetStream(id),
            other => MessageStreamKind::Reserved(other),
        }
    }

    /// True iff this message rides the control stream (`msg_stream_id
    /// == 0`). All protocol-control / user-control / NetConnection
    /// command traffic does, per Message Formats spec §5.
    pub fn is_control_stream(&self) -> bool {
        matches!(self.stream_kind(), MessageStreamKind::Control)
    }

    /// Validate the spec §5 mandate that "Protocol control messages
    /// MUST have message stream ID 0 (called as control stream)". The
    /// protocol-control range is message type IDs 1..=6 (Set Chunk
    /// Size / Abort / Acknowledgement / User Control / Window Ack Size
    /// / Set Peer Bandwidth) — type id 4 (User Control) is grouped
    /// with the §5 protocol-control set in the same spec section. The
    /// §6.1.2.1 reserved top-byte rule on `msg_stream_id` is also
    /// enforced here so a single accessor catches both spec invariants.
    pub fn validate_protocol_control_invariants(&self) -> Result<()> {
        if matches!(self.stream_kind(), MessageStreamKind::Reserved(_)) {
            return Err(Error::ProtocolViolation(format!(
                "message stream id {:#010x} sets reserved high byte (spec §4.1: 3-byte field)",
                self.msg_stream_id
            )));
        }
        if matches!(self.msg_type_id, 1..=6) && self.msg_stream_id != 0 {
            return Err(Error::ProtocolViolation(format!(
                "protocol-control message type {} carries non-zero msg_stream_id {} (spec §5 requires 0)",
                self.msg_type_id, self.msg_stream_id
            )));
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Per-csid state (reader)
// ---------------------------------------------------------------------------

#[derive(Default, Debug, Clone)]
struct InState {
    msg_type_id: u8,
    msg_stream_id: u32,
    msg_length: u32,
    /// Absolute timestamp in ms. fmt 1/2 add a delta to this; fmt 3
    /// re-uses it verbatim (unless the previous message had an
    /// extended timestamp, in which case we re-read 4 bytes — see
    /// [`ExtendedTsMode`]).
    timestamp: u32,
    last_delta: u32,
    /// Whether the last completed fmt 0/1/2 chunk for this csid used
    /// an extended timestamp. fmt 3 follow-ups must then also read
    /// the 4-byte extended timestamp even though they normally don't.
    last_had_ext_ts: bool,
    /// Partial payload while a multi-chunk message is being received.
    partial: Vec<u8>,
}

pub struct ChunkReader<R: Read> {
    stream: R,
    chunk_size: usize,
    states: HashMap<u32, InState>,
    /// Cumulative number of bytes consumed off the wire — basic
    /// headers, message headers, extended timestamps, and payload all
    /// count. This is the running "sequence number" the §5.3
    /// Acknowledgement reports. It wraps at `u32::MAX` like the wire
    /// field; we keep it as `u32` so [`ack_due`](Self::ack_due) emits
    /// the value verbatim.
    received_bytes: u32,
    /// The window size the *peer* asked us to use (its §5.5 Window
    /// Acknowledgement Size). We owe an Acknowledgement once we have
    /// received this many bytes since the last one. `0` disables the
    /// obligation (no window negotiated yet / peer asked for none).
    window_ack_size: u32,
    /// Value of [`received_bytes`](Self::received_bytes) at the moment
    /// we last emitted an Acknowledgement. The next ack is due once
    /// `received_bytes - last_ack_bytes >= window_ack_size`.
    last_ack_bytes: u32,
}

impl<R: Read> ChunkReader<R> {
    pub fn new(stream: R) -> Self {
        Self {
            stream,
            chunk_size: DEFAULT_CHUNK_SIZE,
            states: HashMap::new(),
            received_bytes: 0,
            window_ack_size: 0,
            last_ack_bytes: 0,
        }
    }

    /// Read exactly `buf.len()` bytes off the wire, charging them to
    /// the §5.3 received-byte sequence counter. Every wire read in the
    /// reader funnels through here so the Acknowledgement sequence
    /// number stays exact regardless of chunk framing.
    fn read_exact_counted(&mut self, buf: &mut [u8]) -> Result<()> {
        self.stream.read_exact(buf)?;
        self.received_bytes = self.received_bytes.wrapping_add(buf.len() as u32);
        Ok(())
    }

    /// Total bytes consumed off the wire so far (the §5.3 sequence
    /// number). Wraps at `u32::MAX` like the wire field.
    pub fn received_bytes(&self) -> u32 {
        self.received_bytes
    }

    /// The peer's current §5.5 Window Acknowledgement Size, or `0` if
    /// none has been negotiated.
    pub fn window_ack_size(&self) -> u32 {
        self.window_ack_size
    }

    /// Record the peer's §5.5 Window Acknowledgement Size. Callers
    /// dispatch the 4-byte big-endian body of an inbound
    /// [`MSG_WINDOW_ACK_SIZE`](crate::message::MSG_WINDOW_ACK_SIZE)
    /// message here; per §5.6 a Set Peer Bandwidth also carries an
    /// output-bandwidth value equal to the window size, so the same
    /// setter applies. Setting it to `0` disables the ack obligation.
    ///
    /// Resetting the window re-bases the "bytes since last ack"
    /// accounting to the current sequence number so a freshly-shrunk
    /// window doesn't make a single already-counted byte instantly
    /// owe an Acknowledgement.
    pub fn set_window_ack_size(&mut self, size: u32) {
        self.window_ack_size = size;
        self.last_ack_bytes = self.received_bytes;
    }

    /// Return the §5.3 Acknowledgement sequence number to emit if one
    /// is now due, advancing the internal "last acked" mark so the
    /// next ack only fires after another full window of bytes.
    ///
    /// Per §5.3 a peer "sends the acknowledgment to the peer after
    /// receiving bytes equal to the window size". Returns `None` when
    /// no window is negotiated (`window_ack_size == 0`) or fewer than
    /// `window_ack_size` bytes have arrived since the last ack. The
    /// caller is expected to call this after each
    /// [`read_message`](Self::read_message) and, when it yields
    /// `Some(seq)`, write a [`build_ack(seq)`](crate::message::build_ack)
    /// back to the peer.
    pub fn ack_due(&mut self) -> Option<u32> {
        if self.window_ack_size == 0 {
            return None;
        }
        // Use wrapping subtraction so a `received_bytes` that wrapped
        // past u32::MAX since the last ack still measures the true
        // gap (the wire counter is defined modulo 2^32).
        let since = self.received_bytes.wrapping_sub(self.last_ack_bytes);
        if since >= self.window_ack_size {
            self.last_ack_bytes = self.received_bytes;
            Some(self.received_bytes)
        } else {
            None
        }
    }

    /// Override the current max chunk payload. Callers typically react
    /// to an incoming `SetChunkSize` control message by propagating
    /// the new value here; the reader itself does NOT auto-apply
    /// SetChunkSize because the control flow lives at the message
    /// layer one level up.
    pub fn set_chunk_size(&mut self, size: usize) {
        self.chunk_size = size.clamp(1, MAX_CHUNK_SIZE);
    }

    pub fn chunk_size(&self) -> usize {
        self.chunk_size
    }

    /// React to an inbound Abort Message (RTMP 1.0 §5.2) by discarding the
    /// partially-received message on the named chunk stream id.
    ///
    /// Per §5.2, the Abort Message tells a receiver that "is waiting for
    /// chunks to complete a message" to "discard the partially received
    /// message over a chunk stream and abort processing of that message."
    /// The sender uses it after transmitting part of a message it has
    /// decided not to finish, so the receiver must drop the half-filled
    /// reassembly buffer rather than splice the abandoned bytes onto the
    /// next message that arrives on the same csid.
    ///
    /// This clears only the in-flight payload bytes; the csid's header
    /// state (last timestamp / type / length / extended-timestamp latch)
    /// is left intact, because a subsequent fmt-1/2/3 chunk on the csid
    /// still relies on it per §5.3.2, and a fmt-0 chunk would overwrite
    /// it anyway. An Abort for a csid that has no in-flight message (or
    /// one this reader has never seen) is a no-op, matching the spec's
    /// "if it is waiting for chunks" precondition. Returns `true` when a
    /// non-empty partial buffer was actually discarded.
    ///
    /// The control flow lives at the message layer one level up — like
    /// [`ChunkReader::set_chunk_size`], the reader does not auto-apply an
    /// inbound Abort; the caller dispatches a
    /// [`MSG_ABORT`](crate::message::MSG_ABORT) message's 4-byte
    /// big-endian chunk stream id here.
    pub fn abort_partial(&mut self, chunk_stream_id: u32) -> bool {
        match self.states.get_mut(&chunk_stream_id) {
            Some(st) if !st.partial.is_empty() => {
                st.partial.clear();
                true
            }
            _ => false,
        }
    }

    /// Borrow the underlying reader (for splitting, timeout config, …).
    pub fn inner_mut(&mut self) -> &mut R {
        &mut self.stream
    }

    /// Read chunks off the wire until one full message is reassembled.
    /// Blocks until at least one complete message is available.
    pub fn read_message(&mut self) -> Result<Message> {
        loop {
            let (csid, fmt) = self.read_basic_header()?;
            match fmt {
                0 => self.read_fmt0_header(csid)?,
                1 => self.read_fmt1_header(csid)?,
                2 => self.read_fmt2_header(csid)?,
                3 => self.read_fmt3_header(csid)?,
                _ => unreachable!("fmt is 2 bits"),
            }
            // Read up to `chunk_size` bytes of payload (or the
            // remaining message length, whichever is smaller). Compute
            // the take size from a short immutable lookup, do the
            // counted wire read into a scratch buffer (so the
            // `&mut self` ack-accounting borrow doesn't overlap the
            // per-csid state borrow), then append to the partial.
            let take = {
                let state = self.states.get(&csid).ok_or_else(|| {
                    Error::InvalidChunk(format!(
                        "fmt {fmt} chunk on csid {csid} without prior fmt-0 state"
                    ))
                })?;
                let need = state.msg_length as usize - state.partial.len();
                need.min(self.chunk_size)
            };
            let mut buf = vec![0u8; take];
            self.read_exact_counted(&mut buf)?;
            let state = self
                .states
                .get_mut(&csid)
                .expect("csid state present (checked immediately above)");
            state.partial.extend_from_slice(&buf);

            if state.partial.len() as u32 >= state.msg_length {
                let payload = std::mem::take(&mut state.partial);
                let msg = Message {
                    msg_type_id: state.msg_type_id,
                    msg_stream_id: state.msg_stream_id,
                    timestamp: state.timestamp,
                    payload,
                };
                return Ok(msg);
            }
        }
    }

    fn read_basic_header(&mut self) -> Result<(u32, u8)> {
        let mut b = [0u8; 1];
        self.read_exact_counted(&mut b)?;
        let fmt = (b[0] >> 6) & 0x03;
        let low = b[0] & 0x3F;
        let csid = match low {
            0 => {
                let mut b1 = [0u8; 1];
                self.read_exact_counted(&mut b1)?;
                b1[0] as u32 + 64
            }
            1 => {
                let mut b2 = [0u8; 2];
                self.read_exact_counted(&mut b2)?;
                // spec: second byte is high order, third byte low — but
                // commodity peers in the wild interpret it the other way;
                // the official spec reads "2nd + 3rd byte * 256" which is
                // little-endian.
                b2[0] as u32 + (b2[1] as u32) * 256 + 64
            }
            other => other as u32,
        };
        Ok((csid, fmt))
    }

    fn read_u24_be(&mut self) -> Result<u32> {
        let mut b = [0u8; 3];
        self.read_exact_counted(&mut b)?;
        Ok(((b[0] as u32) << 16) | ((b[1] as u32) << 8) | (b[2] as u32))
    }

    fn read_u32_le_stream_id(&mut self) -> Result<u32> {
        let mut b = [0u8; 4];
        self.read_exact_counted(&mut b)?;
        // msg_stream_id is explicitly little-endian — only field in
        // the RTMP wire format that is.
        Ok(u32::from_le_bytes(b))
    }

    fn read_fmt0_header(&mut self, csid: u32) -> Result<()> {
        let mut ts = self.read_u24_be()?;
        let len = self.read_u24_be()?;
        let mut t = [0u8; 1];
        self.read_exact_counted(&mut t)?;
        let ty = t[0];
        let stream_id = self.read_u32_le_stream_id()?;
        let had_ext_ts = ts == 0x00FF_FFFF;
        if had_ext_ts {
            ts = self.read_u32_be()?;
        }
        let st = self.states.entry(csid).or_default();
        // fmt 0 wipes any half-received message on this csid.
        st.partial.clear();
        st.msg_type_id = ty;
        st.msg_stream_id = stream_id;
        st.msg_length = len;
        st.timestamp = ts;
        st.last_delta = ts;
        st.last_had_ext_ts = had_ext_ts;
        Ok(())
    }

    fn read_fmt1_header(&mut self, csid: u32) -> Result<()> {
        let mut delta = self.read_u24_be()?;
        let len = self.read_u24_be()?;
        let mut t = [0u8; 1];
        self.read_exact_counted(&mut t)?;
        let ty = t[0];
        let had_ext_ts = delta == 0x00FF_FFFF;
        if had_ext_ts {
            delta = self.read_u32_be()?;
        }
        let st = self
            .states
            .get_mut(&csid)
            .ok_or_else(|| Error::InvalidChunk("fmt 1 without prior fmt 0".into()))?;
        st.msg_type_id = ty;
        st.msg_length = len;
        st.timestamp = st.timestamp.wrapping_add(delta);
        st.last_delta = delta;
        st.last_had_ext_ts = had_ext_ts;
        // fmt 1 also starts a new message.
        st.partial.clear();
        Ok(())
    }

    fn read_fmt2_header(&mut self, csid: u32) -> Result<()> {
        let mut delta = self.read_u24_be()?;
        let had_ext_ts = delta == 0x00FF_FFFF;
        if had_ext_ts {
            delta = self.read_u32_be()?;
        }
        let st = self
            .states
            .get_mut(&csid)
            .ok_or_else(|| Error::InvalidChunk("fmt 2 without prior fmt 0/1".into()))?;
        st.timestamp = st.timestamp.wrapping_add(delta);
        st.last_delta = delta;
        st.last_had_ext_ts = had_ext_ts;
        st.partial.clear();
        Ok(())
    }

    fn read_fmt3_header(&mut self, csid: u32) -> Result<()> {
        // fmt 3 reads no message header normally — but if the last
        // message on this csid used an extended timestamp, the
        // extended-timestamp field is repeated here too. This is the
        // most common source of chunk-stream desync bugs in RTMP
        // implementations.
        let (had_ext_ts, partial_empty, last_delta) = {
            let st = self
                .states
                .get(&csid)
                .ok_or_else(|| Error::InvalidChunk("fmt 3 without prior fmt 0/1/2".into()))?;
            (st.last_had_ext_ts, st.partial.is_empty(), st.last_delta)
        };
        if had_ext_ts {
            let _dup = self.read_u32_be()?;
        }
        // If this is a continuation of a multi-chunk message (partial
        // buffer non-empty), we keep appending; otherwise we start a
        // new message with the same metadata as the previous one and
        // extend the timestamp by the last recorded delta.
        if partial_empty {
            let st = self.states.get_mut(&csid).unwrap();
            st.timestamp = st.timestamp.wrapping_add(last_delta);
        }
        Ok(())
    }

    fn read_u32_be(&mut self) -> Result<u32> {
        let mut b = [0u8; 4];
        self.read_exact_counted(&mut b)?;
        Ok(u32::from_be_bytes(b))
    }
}

// ---------------------------------------------------------------------------
// Writer
// ---------------------------------------------------------------------------

#[derive(Default, Debug, Clone)]
struct OutState {
    msg_type_id: u8,
    msg_stream_id: u32,
    msg_length: u32,
    timestamp: u32,
    last_delta: u32,
    last_had_ext_ts: bool,
    /// True once the first fmt-0 chunk has been emitted on this csid.
    primed: bool,
}

pub struct ChunkWriter<W: Write> {
    stream: W,
    chunk_size: usize,
    states: HashMap<u32, OutState>,
}

impl<W: Write> ChunkWriter<W> {
    pub fn new(stream: W) -> Self {
        Self {
            stream,
            chunk_size: DEFAULT_CHUNK_SIZE,
            states: HashMap::new(),
        }
    }

    pub fn set_chunk_size(&mut self, size: usize) {
        self.chunk_size = size.clamp(1, MAX_CHUNK_SIZE);
    }

    pub fn chunk_size(&self) -> usize {
        self.chunk_size
    }

    pub fn inner_mut(&mut self) -> &mut W {
        &mut self.stream
    }

    pub fn flush(&mut self) -> Result<()> {
        self.stream.flush()?;
        Ok(())
    }

    /// Emit `msg` on `csid`, splitting into chunks if the payload
    /// exceeds the current per-chunk limit. Picks the densest valid
    /// chunk header format:
    ///
    /// * first message on the csid → fmt 0 (full header);
    /// * same (stream, type, length) as previous + monotonic
    ///   timestamp delta → fmt 2 or 3;
    /// * same stream but different (type, length) → fmt 1;
    /// * anything else → fmt 0.
    pub fn write_message(&mut self, csid: u32, msg: &Message) -> Result<()> {
        let payload_len = msg.payload.len() as u32;

        // Snapshot the state fields we need for the header decision
        // and the continuation-chunk timestamp repeat. Dropping the
        // borrow here lets us reach for `self.stream` / `self.chunk_size`
        // below without NLL complaints.
        let (prev_primed, prev_stream_id, prev_type_id, prev_length, prev_timestamp) = {
            let st = self.states.entry(csid).or_default();
            (
                st.primed,
                st.msg_stream_id,
                st.msg_type_id,
                st.msg_length,
                st.timestamp,
            )
        };
        // Query the sticky "most recent fmt-0/1/2 chunk used an
        // extended timestamp" bit and the last registered delta — a
        // fmt-3 head chunk implicitly reuses both (§5.3.1.2.4).
        let (prev_last_had_ext_ts, prev_last_delta) = self
            .states
            .get(&csid)
            .map(|s| (s.last_had_ext_ts, s.last_delta))
            .unwrap_or((false, 0));

        let delta = msg.timestamp.wrapping_sub(prev_timestamp);
        // Pick the most compact fmt we can get away with.
        let fmt = if !prev_primed
            || prev_stream_id != msg.msg_stream_id
            || msg.timestamp < prev_timestamp
        {
            // First message on the csid, a different message stream, or
            // a timestamp that went backwards (rare, e.g. seek in the
            // source): re-prime with a full fmt 0 header.
            0
        } else if prev_type_id != msg.msg_type_id || prev_length != payload_len {
            1
        } else if delta == prev_last_delta {
            // §5.3.1.2.4: a fmt-3 head chunk takes *every* value from
            // the preceding chunk, including the timestamp delta — so
            // it is only valid when this message's delta equals the
            // one already registered on the csid. (The reader will add
            // `last_delta` to its running timestamp.)
            3
        } else {
            2
        };
        let ext_ts_needed = (fmt == 0 && msg.timestamp >= 0x00FF_FFFF)
            || ((fmt == 1 || fmt == 2) && delta >= 0x00FF_FFFF);
        // §5.3.1.3 (December 2012 revision): the extended-timestamp
        // field is present in a Type 3 chunk when the most recent
        // Type 0/1/2 chunk on the csid indicated one. That covers both
        // a fmt-3 *head* chunk (inherits the previous chunk's ext bit)
        // and every continuation chunk of a message whose own head
        // chunk indicated ext.
        let fmt3_ext = if fmt == 3 {
            prev_last_had_ext_ts
        } else {
            ext_ts_needed
        };
        // Value carried by the ext field: the full 32-bit absolute
        // timestamp for fmt 0, the full 32-bit delta for fmt 1/2, and
        // the inherited delta for a fmt-3 head.
        let ext_value = match fmt {
            0 => msg.timestamp,
            1 | 2 => delta,
            _ => prev_last_delta,
        };

        let chunk_size = self.chunk_size;
        let mut first_chunk_done = false;
        let mut cursor = 0usize;
        while cursor < msg.payload.len() || !first_chunk_done {
            let chunk_fmt = if !first_chunk_done { fmt } else { 3 };
            self.write_basic_header(chunk_fmt, csid)?;
            match chunk_fmt {
                0 => {
                    let ts_field = if ext_ts_needed {
                        0x00FF_FFFF
                    } else {
                        msg.timestamp
                    };
                    self.write_u24_be(ts_field)?;
                    self.write_u24_be(payload_len)?;
                    self.stream.write_all(&[msg.msg_type_id])?;
                    self.stream.write_all(&msg.msg_stream_id.to_le_bytes())?;
                    if ext_ts_needed {
                        self.stream.write_all(&msg.timestamp.to_be_bytes())?;
                    }
                }
                1 => {
                    let ts_field = if ext_ts_needed { 0x00FF_FFFF } else { delta };
                    self.write_u24_be(ts_field)?;
                    self.write_u24_be(payload_len)?;
                    self.stream.write_all(&[msg.msg_type_id])?;
                    if ext_ts_needed {
                        // §5.3.1.3: for a Type 1 chunk the field
                        // encodes the full 32-bit *delta*, not the
                        // absolute timestamp.
                        self.stream.write_all(&ext_value.to_be_bytes())?;
                    }
                }
                2 => {
                    let ts_field = if ext_ts_needed { 0x00FF_FFFF } else { delta };
                    self.write_u24_be(ts_field)?;
                    if ext_ts_needed {
                        self.stream.write_all(&ext_value.to_be_bytes())?;
                    }
                }
                3 => {
                    // §5.3.1.3: a Type 3 chunk (head or continuation)
                    // carries the extended-timestamp field when the
                    // most recent Type 0/1/2 chunk on the csid
                    // indicated one. For a fmt-3 head that is the
                    // previous message's sticky bit; for a continuation
                    // it is this message's own head-chunk decision
                    // (`fmt3_ext` equals `ext_ts_needed` for fmt-0/1/2
                    // heads, so one flag covers every chunk of the
                    // message).
                    if fmt3_ext {
                        self.stream.write_all(&ext_value.to_be_bytes())?;
                    }
                }
                _ => unreachable!(),
            }
            let end = (cursor + chunk_size).min(msg.payload.len());
            self.stream.write_all(&msg.payload[cursor..end])?;
            cursor = end;
            first_chunk_done = true;
        }

        // Commit the updated state.
        let st = self.states.entry(csid).or_default();
        st.msg_type_id = msg.msg_type_id;
        st.msg_stream_id = msg.msg_stream_id;
        st.msg_length = payload_len;
        st.timestamp = msg.timestamp;
        st.last_delta = if fmt == 0 { msg.timestamp } else { delta };
        // The sticky bit tracks the most recent fmt-0/1/2 chunk — a
        // fmt-3 message leaves it untouched (mirroring the reader,
        // which never updates it on fmt 3). `fmt3_ext` carries the
        // previous value forward in that case.
        st.last_had_ext_ts = fmt3_ext;
        st.primed = true;
        Ok(())
    }

    fn write_basic_header(&mut self, fmt: u8, csid: u32) -> Result<()> {
        match csid {
            2..=63 => {
                self.stream.write_all(&[(fmt << 6) | (csid as u8)])?;
            }
            64..=319 => {
                self.stream.write_all(&[fmt << 6, (csid - 64) as u8])?;
            }
            320..=65_599 => {
                let v = (csid - 64) as u16;
                self.stream
                    .write_all(&[(fmt << 6) | 1, (v & 0xFF) as u8, (v >> 8) as u8])?;
            }
            other => {
                return Err(Error::ProtocolViolation(format!(
                    "chunk stream id {other} out of range"
                )))
            }
        }
        Ok(())
    }

    fn write_u24_be(&mut self, v: u32) -> Result<()> {
        let v = v & 0x00FF_FFFF;
        self.stream
            .write_all(&[(v >> 16) as u8, (v >> 8) as u8, v as u8])?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// Send one small message through ChunkWriter → ChunkReader and
    /// make sure the payload, timestamp, type id, and stream id all
    /// survive the round-trip.
    #[test]
    fn chunk_roundtrip_short_message() {
        let mut buf = Vec::new();
        {
            let mut w = ChunkWriter::new(&mut buf);
            w.write_message(
                3,
                &Message {
                    msg_type_id: 20,
                    msg_stream_id: 0,
                    timestamp: 12345,
                    payload: b"hello world".to_vec(),
                },
            )
            .unwrap();
        }
        let mut r = ChunkReader::new(Cursor::new(&buf));
        let msg = r.read_message().unwrap();
        assert_eq!(msg.msg_type_id, 20);
        assert_eq!(msg.timestamp, 12345);
        assert_eq!(msg.payload, b"hello world");
    }

    /// Force a multi-chunk payload (small chunk size, bigger message)
    /// and check it still reassembles correctly.
    #[test]
    fn chunk_roundtrip_multi_chunk_message() {
        let payload: Vec<u8> = (0..4096u16).map(|i| (i & 0xFF) as u8).collect();
        let mut buf = Vec::new();
        {
            let mut w = ChunkWriter::new(&mut buf);
            w.set_chunk_size(128);
            w.write_message(
                3,
                &Message {
                    msg_type_id: 9,
                    msg_stream_id: 1,
                    timestamp: 7000,
                    payload: payload.clone(),
                },
            )
            .unwrap();
        }
        let mut r = ChunkReader::new(Cursor::new(&buf));
        r.set_chunk_size(128);
        let msg = r.read_message().unwrap();
        assert_eq!(msg.payload, payload);
        assert_eq!(msg.msg_type_id, 9);
        assert_eq!(msg.timestamp, 7000);
    }

    /// §5.3 Acknowledgement accounting: `received_bytes` counts every
    /// byte the reader consumes off the wire — basic header, message
    /// header, extended timestamp, and payload — so the sequence
    /// number it reports matches the peer's view of "bytes sent".
    #[test]
    fn received_bytes_counts_full_wire_size() {
        let mut buf = Vec::new();
        {
            let mut w = ChunkWriter::new(&mut buf);
            w.write_message(
                3,
                &Message {
                    msg_type_id: 20,
                    msg_stream_id: 0,
                    timestamp: 1000,
                    payload: b"abcdef".to_vec(),
                },
            )
            .unwrap();
        }
        let wire_len = buf.len() as u32;
        let mut r = ChunkReader::new(Cursor::new(&buf));
        assert_eq!(r.received_bytes(), 0);
        let _ = r.read_message().unwrap();
        // The whole single-chunk frame was consumed; nothing left over.
        assert_eq!(r.received_bytes(), wire_len);
    }

    /// With no window negotiated, `ack_due` never fires regardless of
    /// how many bytes flow.
    #[test]
    fn ack_not_due_without_window() {
        let mut buf = Vec::new();
        {
            let mut w = ChunkWriter::new(&mut buf);
            w.write_message(
                3,
                &Message {
                    msg_type_id: 20,
                    msg_stream_id: 0,
                    timestamp: 0,
                    payload: vec![0u8; 500],
                },
            )
            .unwrap();
        }
        let mut r = ChunkReader::new(Cursor::new(&buf));
        let _ = r.read_message().unwrap();
        assert_eq!(r.window_ack_size(), 0);
        assert_eq!(r.ack_due(), None);
    }

    /// §5.3 / §5.5: once a window is set, `ack_due` fires the first
    /// time the received-byte count crosses it, reports the running
    /// sequence number, and re-arms only after another full window.
    #[test]
    fn ack_due_fires_once_per_window() {
        // Two messages, each ~big enough that the first crosses a
        // small window and the second crosses the next one.
        let mut buf = Vec::new();
        {
            let mut w = ChunkWriter::new(&mut buf);
            for ts in [10u32, 20] {
                w.write_message(
                    4,
                    &Message {
                        msg_type_id: 8,
                        msg_stream_id: 1,
                        timestamp: ts,
                        payload: vec![0xAB; 200],
                    },
                )
                .unwrap();
            }
        }
        let mut r = ChunkReader::new(Cursor::new(&buf));
        r.set_window_ack_size(150);
        assert_eq!(r.window_ack_size(), 150);

        let _ = r.read_message().unwrap();
        // First message (≈ 211 bytes ≥ 150) owes an ack at the current
        // sequence number, and it is not due a second time immediately.
        let first = r.ack_due().expect("first ack due after window crossed");
        assert_eq!(first, r.received_bytes());
        assert_eq!(r.ack_due(), None, "ack must not re-fire within a window");

        let _ = r.read_message().unwrap();
        // Second message pushes another full window past the last ack.
        let second = r.ack_due().expect("second ack due after second window");
        assert!(second > first);
        assert_eq!(second, r.received_bytes());
    }

    /// §5.5: shrinking / resetting the window re-bases the
    /// "bytes since last ack" mark to the current sequence so a
    /// single already-counted byte doesn't instantly owe an ack.
    #[test]
    fn set_window_rebases_accounting() {
        let mut buf = Vec::new();
        {
            let mut w = ChunkWriter::new(&mut buf);
            w.write_message(
                4,
                &Message {
                    msg_type_id: 8,
                    msg_stream_id: 1,
                    timestamp: 0,
                    payload: vec![0u8; 400],
                },
            )
            .unwrap();
        }
        let mut r = ChunkReader::new(Cursor::new(&buf));
        let _ = r.read_message().unwrap();
        // Set the window AFTER 400+ bytes already arrived: because the
        // setter re-bases, no ack is immediately due even though the
        // total received exceeds the new window.
        r.set_window_ack_size(100);
        assert_eq!(r.ack_due(), None);
    }

    /// RTMP 1.0 §5.2 Abort Message: after a publisher sends part of a
    /// multi-chunk message and then aborts it, the receiver must discard
    /// the half-filled reassembly buffer for that csid. We build a
    /// two-chunk message, hand the reader only the first chunk (so it is
    /// "waiting for chunks to complete a message" and `read_message`
    /// surfaces `UnexpectedEof`), then assert `abort_partial` reports it
    /// discarded a non-empty buffer.
    #[test]
    fn abort_partial_discards_in_flight_message() {
        let payload: Vec<u8> = (0..200u16).map(|i| (i & 0xFF) as u8).collect();
        let mut full = Vec::new();
        {
            let mut w = ChunkWriter::new(&mut full);
            w.set_chunk_size(128);
            w.write_message(
                5,
                &Message {
                    msg_type_id: 9,
                    msg_stream_id: 1,
                    timestamp: 1000,
                    payload: payload.clone(),
                },
            )
            .unwrap();
        }
        // First chunk = fmt-0 header (12 bytes) + 128 payload bytes. Hand
        // the reader only that prefix so the second chunk never arrives.
        let first_chunk = &full[..12 + 128];
        let mut r = ChunkReader::new(Cursor::new(first_chunk));
        r.set_chunk_size(128);
        // Reading blocks for the missing chunk, hitting EOF — the csid-5
        // partial buffer now holds the first 128 bytes.
        let err = r.read_message().unwrap_err();
        assert!(matches!(err, Error::Io(_) | Error::UnexpectedEof));
        // §5.2 discard: a non-empty partial exists, so abort returns true
        // and clears it; a second abort on the now-empty csid is a no-op.
        assert!(r.abort_partial(5), "first abort should discard 128 bytes");
        assert!(!r.abort_partial(5), "second abort has nothing to discard");
        // An abort for a csid the reader never saw is also a no-op.
        assert!(!r.abort_partial(9));
    }

    fn msg(ty: u8, stream: u32, ts: u32, len: usize) -> Message {
        Message {
            msg_type_id: ty,
            msg_stream_id: stream,
            timestamp: ts,
            payload: vec![0xAA; len],
        }
    }

    fn roundtrip(msgs: &[(u32, Message)], chunk_size: Option<usize>) -> Vec<Message> {
        let mut buf = Vec::new();
        {
            let mut w = ChunkWriter::new(&mut buf);
            if let Some(cs) = chunk_size {
                w.set_chunk_size(cs);
            }
            for (csid, m) in msgs {
                w.write_message(*csid, m).unwrap();
            }
        }
        let mut r = ChunkReader::new(Cursor::new(&buf));
        if let Some(cs) = chunk_size {
            r.set_chunk_size(cs);
        }
        msgs.iter().map(|_| r.read_message().unwrap()).collect()
    }

    /// §5.3.1.2.4: a fmt-3 head chunk reuses the registered delta —
    /// spec worked example: "if the delta between the first message and
    /// the second message is same as the time stamp of first message,
    /// then chunk of type 3 would immediately follow the chunk of
    /// type 0". Constant spacing after fmt 0 → fmt 3, and the reader
    /// reconstructs every timestamp.
    #[test]
    fn constant_spacing_uses_fmt3_and_roundtrips() {
        let msgs: Vec<(u32, Message)> = vec![
            (5, msg(9, 1, 1000, 32)),
            (5, msg(9, 1, 2000, 32)),
            (5, msg(9, 1, 3000, 32)),
        ];
        let mut buf = Vec::new();
        {
            let mut w = ChunkWriter::new(&mut buf);
            for (csid, m) in &msgs {
                w.write_message(*csid, m).unwrap();
            }
        }
        // Second basic header (fmt=3, csid=5) → 0xC5, right after the
        // fmt-0 chunk (1 + 11 header bytes + 32 payload bytes).
        assert_eq!(buf[1 + 11 + 32], 0xC5);
        // Third likewise, another bare basic header + payload later.
        assert_eq!(buf[(1 + 11 + 32) + (1 + 32)], 0xC5);
        let mut r = ChunkReader::new(Cursor::new(&buf));
        for (_, m) in &msgs {
            let got = r.read_message().unwrap();
            assert_eq!(got.timestamp, m.timestamp);
            assert_eq!(got.payload, m.payload);
        }
    }

    /// A repeated timestamp is a delta of 0 — NOT the same delta the
    /// fmt-0 head registered — so the writer must fall back to fmt 2
    /// (delta 0) rather than fmt 3, or the reader would advance the
    /// clock by the stale delta.
    #[test]
    fn same_timestamp_repeat_does_not_desync() {
        let got = roundtrip(&[(5, msg(9, 1, 1000, 32)), (5, msg(9, 1, 1000, 32))], None);
        assert_eq!(got[0].timestamp, 1000);
        assert_eq!(got[1].timestamp, 1000);
    }

    /// §5.3.1.3 edge matrix — a multi-chunk message with an extended
    /// absolute timestamp must repeat the ext field on every fmt-3
    /// continuation chunk (December 2012 revision), or the reader
    /// desyncs 4 bytes into the second chunk.
    #[test]
    fn ext_timestamp_multi_chunk_roundtrip() {
        let payload: Vec<u8> = (0..1000u16).map(|i| (i & 0xFF) as u8).collect();
        let m = Message {
            msg_type_id: 9,
            msg_stream_id: 1,
            timestamp: 0x0100_0000,
            payload: payload.clone(),
        };
        let got = roundtrip(&[(3, m)], Some(128));
        assert_eq!(got[0].timestamp, 0x0100_0000);
        assert_eq!(got[0].payload, payload);
    }

    /// Boundary case: a timestamp of exactly 0xFFFFFF MUST be sent via
    /// the extended field ("greater than or equal to 16777215").
    #[test]
    fn ext_timestamp_exact_boundary() {
        let mut buf = Vec::new();
        {
            let mut w = ChunkWriter::new(&mut buf);
            w.write_message(3, &msg(20, 0, 0x00FF_FFFF, 8)).unwrap();
        }
        // fmt-0 header: ts field must be the 0xFFFFFF sentinel and the
        // 4-byte ext field must follow the 11-byte message header.
        assert_eq!(&buf[1..4], &[0xFF, 0xFF, 0xFF]);
        assert_eq!(&buf[12..16], &[0x00, 0xFF, 0xFF, 0xFF]);
        let mut r = ChunkReader::new(Cursor::new(&buf));
        assert_eq!(r.read_message().unwrap().timestamp, 0x00FF_FFFF);
    }

    /// Extended *delta* matrix: fmt-2 head with a ≥0xFFFFFF delta, then
    /// a fmt-3 head reusing that same extended delta (which must carry
    /// the ext field per the 2012 revision, since the most recent
    /// fmt-0/1/2 chunk indicated one).
    #[test]
    fn ext_delta_fmt2_then_fmt3_head_roundtrip() {
        let got = roundtrip(
            &[
                (5, msg(9, 1, 0, 16)),
                (5, msg(9, 1, 0x0100_0000, 16)),
                (5, msg(9, 1, 0x0200_0000, 16)),
            ],
            None,
        );
        assert_eq!(got[0].timestamp, 0);
        assert_eq!(got[1].timestamp, 0x0100_0000);
        assert_eq!(got[2].timestamp, 0x0200_0000);
    }

    /// Extended delta on a fmt-1 head (message type changes, huge
    /// delta): the ext field must carry the 32-bit *delta*, not the
    /// absolute timestamp.
    #[test]
    fn ext_delta_fmt1_roundtrip() {
        let got = roundtrip(
            &[(5, msg(9, 1, 100, 16)), (5, msg(8, 1, 0x0100_0064, 24))],
            None,
        );
        assert_eq!(got[1].timestamp, 0x0100_0064);
        assert_eq!(got[1].msg_type_id, 8);
    }

    /// An extended-delta head chunk of a multi-chunk message: every
    /// continuation repeats the ext field too.
    #[test]
    fn ext_delta_multi_chunk_roundtrip() {
        let payload: Vec<u8> = (0..600u16).map(|i| (i & 0xFF) as u8).collect();
        let m2 = Message {
            msg_type_id: 9,
            msg_stream_id: 1,
            timestamp: 0x0100_0000,
            payload: payload.clone(),
        };
        let got = roundtrip(&[(5, msg(9, 1, 0, 16)), (5, m2)], Some(128));
        assert_eq!(got[1].timestamp, 0x0100_0000);
        assert_eq!(got[1].payload, payload);
    }

    /// §5.3.1.2.5: separate message streams CAN be multiplexed into one
    /// chunk stream — each switch re-primes with a fmt-0 chunk.
    #[test]
    fn multiplexed_message_streams_one_csid() {
        let got = roundtrip(
            &[
                (3, msg(8, 1, 500, 16)),
                (3, msg(8, 2, 400, 16)),
                (3, msg(8, 1, 600, 16)),
            ],
            None,
        );
        assert_eq!(
            got.iter()
                .map(|m| (m.msg_stream_id, m.timestamp))
                .collect::<Vec<_>>(),
            vec![(1, 500), (2, 400), (1, 600)]
        );
    }

    /// Chunk-level interleave: two csids alternating chunk-by-chunk on
    /// one connection. The reader keeps independent per-csid state, so
    /// the message whose final chunk lands first completes first.
    #[test]
    fn interleaved_csids_chunk_by_chunk() {
        // csid 3 carries a 2-chunk message; csid 4 slots a whole small
        // message between the two chunks. Hand-spliced from two
        // independently-written wires (per-csid state is independent,
        // so the byte streams compose).
        let mut wire_a = Vec::new();
        {
            let mut w = ChunkWriter::new(&mut wire_a);
            w.set_chunk_size(128);
            let payload: Vec<u8> = (0..200u16).map(|i| (i & 0xFF) as u8).collect();
            w.write_message(
                3,
                &Message {
                    msg_type_id: 9,
                    msg_stream_id: 1,
                    timestamp: 10,
                    payload,
                },
            )
            .unwrap();
        }
        let mut wire_b = Vec::new();
        {
            let mut w = ChunkWriter::new(&mut wire_b);
            w.set_chunk_size(128);
            w.write_message(4, &msg(8, 1, 20, 16)).unwrap();
        }
        // wire_a = [12-byte fmt0 header + 128 payload] + [1-byte fmt3
        // header + 72 payload].
        let split = 12 + 128;
        let mut spliced = Vec::new();
        spliced.extend_from_slice(&wire_a[..split]);
        spliced.extend_from_slice(&wire_b);
        spliced.extend_from_slice(&wire_a[split..]);
        let mut r = ChunkReader::new(Cursor::new(&spliced));
        r.set_chunk_size(128);
        let first = r.read_message().unwrap();
        assert_eq!(first.msg_type_id, 8, "csid-4 message completes first");
        assert_eq!(first.timestamp, 20);
        let second = r.read_message().unwrap();
        assert_eq!(second.msg_type_id, 9);
        assert_eq!(second.timestamp, 10);
        assert_eq!(second.payload.len(), 200);
    }
}
