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
// Per-csid state tables
// ---------------------------------------------------------------------------

/// Chunk stream ids below this bound are addressed by direct index
/// (they are the ids the one-byte basic header can express, and the
/// ones every commodity peer uses); the rest live in a map.
const DIRECT_CSIDS: usize = 64;

/// Per-chunk-stream state table: direct-indexed for the one-byte
/// basic-header range, hashed for the two- and three-byte forms.
struct CsidTable<T> {
    low: Vec<Option<T>>,
    high: HashMap<u32, T>,
}

impl<T: Default> CsidTable<T> {
    fn new() -> Self {
        Self {
            low: (0..DIRECT_CSIDS).map(|_| None).collect(),
            high: HashMap::new(),
        }
    }

    fn get(&self, csid: u32) -> Option<&T> {
        match self.low.get(csid as usize) {
            Some(slot) => slot.as_ref(),
            None => self.high.get(&csid),
        }
    }

    fn get_mut(&mut self, csid: u32) -> Option<&mut T> {
        match self.low.get_mut(csid as usize) {
            Some(slot) => slot.as_mut(),
            None => self.high.get_mut(&csid),
        }
    }

    fn entry(&mut self, csid: u32) -> &mut T {
        match self.low.get_mut(csid as usize) {
            Some(slot) => slot.get_or_insert_with(T::default),
            None => self.high.entry(csid).or_default(),
        }
    }
}

// ---------------------------------------------------------------------------
// Reader
// ---------------------------------------------------------------------------

#[derive(Default, Debug, Clone)]
struct InState {
    msg_type_id: u8,
    msg_stream_id: u32,
    msg_length: u32,
    /// Absolute timestamp in ms. fmt 1/2 add a delta to this; a fmt 3
    /// chunk that starts a new message adds the last delta again.
    timestamp: u32,
    last_delta: u32,
    /// Whether the most recent fmt 0/1/2 chunk for this csid used an
    /// extended timestamp. fmt 3 chunks then also carry the 4-byte
    /// extended timestamp field (§5.3.1.3).
    last_had_ext_ts: bool,
    /// A message is being reassembled on this csid: its declared
    /// length is charged to the reader's in-flight budget until the
    /// message completes, is aborted, or is superseded.
    assembling: bool,
    /// Partial payload while a multi-chunk message is being received.
    partial: Vec<u8>,
}

/// The chunk whose header has been consumed and whose payload is
/// still (partly) on the wire.
#[derive(Debug, Clone, Copy)]
struct CurrentChunk {
    csid: u32,
    remaining: usize,
}

/// Size of the reader's internal receive buffer.
const READ_BUF_LEN: usize = 16 * 1024;

/// Default ceiling on the summed declared lengths of all messages
/// being reassembled at once — see
/// [`ChunkReader::set_max_in_flight_bytes`].
pub const DEFAULT_MAX_IN_FLIGHT_BYTES: usize = 64 * 1024 * 1024;

/// Largest message length the 24-bit chunk message-header field can
/// declare (§5.3.1.2.1).
pub const MAX_MESSAGE_LENGTH: usize = 0x00FF_FFFF;

/// Reassembles RTMP messages from a chunk stream.
///
/// The reader keeps a small internal receive buffer, so it may pull
/// more bytes off `R` than the message it returns needed; anything
/// read ahead is served to the following calls. A read error that
/// leaves the stream usable — notably the `WouldBlock` / `TimedOut`
/// of a socket read timeout — does not lose data: every byte received
/// so far is kept, and the next [`read_message`](Self::read_message)
/// resumes exactly where the failed one stopped.
pub struct ChunkReader<R: Read> {
    stream: R,
    buf: Box<[u8]>,
    pos: usize,
    end: usize,
    chunk_size: usize,
    states: CsidTable<InState>,
    current: Option<CurrentChunk>,
    /// Sum of the declared lengths of the messages currently being
    /// reassembled.
    in_flight_bytes: usize,
    max_message_size: usize,
    max_in_flight_bytes: usize,
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

fn eof() -> Error {
    Error::Io(std::io::Error::from(std::io::ErrorKind::UnexpectedEof))
}

impl<R: Read> ChunkReader<R> {
    pub fn new(stream: R) -> Self {
        Self {
            stream,
            buf: vec![0u8; READ_BUF_LEN].into_boxed_slice(),
            pos: 0,
            end: 0,
            chunk_size: DEFAULT_CHUNK_SIZE,
            states: CsidTable::new(),
            current: None,
            in_flight_bytes: 0,
            max_message_size: MAX_MESSAGE_LENGTH,
            max_in_flight_bytes: DEFAULT_MAX_IN_FLIGHT_BYTES,
            received_bytes: 0,
            window_ack_size: 0,
            last_ack_bytes: 0,
        }
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

    /// Refuse any message whose declared length exceeds `bytes`
    /// (clamped to the 24-bit wire maximum, which is also the
    /// default). A server that only expects ordinary audio / video /
    /// command traffic can lower this to shed hostile length fields
    /// before any payload is buffered.
    pub fn set_max_message_size(&mut self, bytes: usize) {
        self.max_message_size = bytes.min(MAX_MESSAGE_LENGTH);
    }

    /// The current per-message length ceiling.
    pub fn max_message_size(&self) -> usize {
        self.max_message_size
    }

    /// Bound the memory a peer can pin by starting messages it never
    /// finishes: the declared lengths of all messages being
    /// reassembled at once (one per chunk stream id) may not sum past
    /// `bytes`. A chunk that would cross the bound is an
    /// [`Error::InvalidChunk`]. Default
    /// [`DEFAULT_MAX_IN_FLIGHT_BYTES`]. Values below one maximum-size
    /// message are honoured as given, so they also cap the largest
    /// acceptable message.
    pub fn set_max_in_flight_bytes(&mut self, bytes: usize) {
        self.max_in_flight_bytes = bytes;
    }

    /// The current in-flight reassembly budget.
    pub fn max_in_flight_bytes(&self) -> usize {
        self.max_in_flight_bytes
    }

    /// `true` when the reader sits exactly on a message boundary:
    /// no chunk is half-read, no message is partially reassembled on
    /// any chunk stream, and nothing is buffered. An end-of-file
    /// reported by [`read_message`](Self::read_message) while idle is
    /// an orderly close between messages; otherwise the peer vanished
    /// mid-message.
    pub fn is_idle(&self) -> bool {
        self.current.is_none() && self.in_flight_bytes == 0 && self.pos == self.end
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
        if matches!(self.current, Some(c) if c.csid == chunk_stream_id) {
            // The abort names the chunk stream whose chunk payload is
            // still arriving; those bytes stay on the wire and must be
            // skipped as framing, so leave that chunk alone.
            return false;
        }
        match self.states.get_mut(chunk_stream_id) {
            Some(st) if st.assembling => {
                st.assembling = false;
                self.in_flight_bytes -= st.msg_length as usize;
                let had = !st.partial.is_empty();
                st.partial = Vec::new();
                had
            }
            _ => false,
        }
    }

    /// Borrow the underlying reader (for splitting, timeout config, …).
    /// Do not read from it directly: the chunk reader may already
    /// hold bytes it pulled ahead.
    pub fn inner_mut(&mut self) -> &mut R {
        &mut self.stream
    }

    /// Read chunks off the wire until one full message is reassembled.
    /// Blocks until at least one complete message is available.
    pub fn read_message(&mut self) -> Result<Message> {
        loop {
            if self.current.is_none() {
                self.begin_chunk()?;
            }
            if let Some(msg) = self.pull_payload()? {
                return Ok(msg);
            }
        }
    }

    /// One `read` into the free tail of the receive buffer.
    fn fill(&mut self) -> Result<()> {
        if self.pos == self.end {
            self.pos = 0;
            self.end = 0;
        } else if self.end == self.buf.len() {
            self.buf.copy_within(self.pos..self.end, 0);
            self.end -= self.pos;
            self.pos = 0;
        }
        loop {
            match self.stream.read(&mut self.buf[self.end..]) {
                Ok(0) => return Err(eof()),
                Ok(n) => {
                    self.end += n;
                    return Ok(());
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return Err(Error::Io(e)),
            }
        }
    }

    /// Make at least `n` unread bytes available in the receive buffer
    /// (`n` is a chunk-header size, far below the buffer length).
    fn ensure(&mut self, n: usize) -> Result<()> {
        while self.end - self.pos < n {
            self.fill()?;
        }
        Ok(())
    }

    /// Parse one complete chunk header (basic header, message header
    /// and extended timestamp) and register the chunk as current.
    /// Nothing is consumed until the whole header is buffered, so a
    /// read error part-way leaves the reader resumable.
    fn begin_chunk(&mut self) -> Result<()> {
        self.ensure(1)?;
        let b0 = self.buf[self.pos];
        let fmt = b0 >> 6;
        let basic_len = match b0 & 0x3F {
            0 => 2,
            1 => 3,
            _ => 1,
        };
        self.ensure(basic_len)?;
        let p = self.pos;
        let csid = match b0 & 0x3F {
            0 => self.buf[p + 1] as u32 + 64,
            // §5.3.1.1: "(the third byte) * 256 + (the second byte) + 64".
            1 => self.buf[p + 1] as u32 + (self.buf[p + 2] as u32) * 256 + 64,
            low => low as u32,
        };
        let header_len = match fmt {
            0 => 11,
            1 => 7,
            2 => 3,
            _ => 0,
        };
        self.ensure(basic_len + header_len)?;
        let h = self.pos + basic_len;
        let u24 = |b: &[u8]| ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | (b[2] as u32);
        let has_ext = if fmt == 3 {
            self.states
                .get(csid)
                .ok_or_else(|| Error::InvalidChunk("fmt 3 without prior fmt 0/1/2".into()))?
                .last_had_ext_ts
        } else {
            u24(&self.buf[h..h + 3]) == 0x00FF_FFFF
        };
        let total = basic_len + header_len + if has_ext { 4 } else { 0 };
        self.ensure(total)?;
        let h = self.pos + basic_len;
        let hdr = &self.buf[h..h + header_len];
        let ext_at = h + header_len;
        let ext = if has_ext {
            u32::from_be_bytes([
                self.buf[ext_at],
                self.buf[ext_at + 1],
                self.buf[ext_at + 2],
                self.buf[ext_at + 3],
            ])
        } else {
            0
        };

        // Does this chunk start a new message, and with what header?
        let (starts_message, length) = match fmt {
            0 => {
                let ts = if has_ext { ext } else { u24(&hdr[0..3]) };
                let len = u24(&hdr[3..6]);
                let ty = hdr[6];
                // msg_stream_id is little-endian — the only such field
                // in the chunk format (§5.3.1.2.1).
                let stream_id = u32::from_le_bytes([hdr[7], hdr[8], hdr[9], hdr[10]]);
                let st = self.states.entry(csid);
                st.msg_type_id = ty;
                st.msg_stream_id = stream_id;
                st.timestamp = ts;
                st.last_delta = ts;
                st.last_had_ext_ts = has_ext;
                (true, len)
            }
            1 => {
                let delta = if has_ext { ext } else { u24(&hdr[0..3]) };
                let len = u24(&hdr[3..6]);
                let ty = hdr[6];
                let st = self
                    .states
                    .get_mut(csid)
                    .ok_or_else(|| Error::InvalidChunk("fmt 1 without prior fmt 0".into()))?;
                st.msg_type_id = ty;
                st.timestamp = st.timestamp.wrapping_add(delta);
                st.last_delta = delta;
                st.last_had_ext_ts = has_ext;
                (true, len)
            }
            2 => {
                let delta = if has_ext { ext } else { u24(&hdr[0..3]) };
                let st = self
                    .states
                    .get_mut(csid)
                    .ok_or_else(|| Error::InvalidChunk("fmt 2 without prior fmt 0/1".into()))?;
                st.timestamp = st.timestamp.wrapping_add(delta);
                st.last_delta = delta;
                st.last_had_ext_ts = has_ext;
                (true, st.msg_length)
            }
            _ => {
                // fmt 3 carries no message header. When the most recent
                // fmt 0/1/2 chunk on the csid used an extended
                // timestamp the 4-byte field is repeated here (already
                // accounted for in `total`). A fmt 3 chunk either
                // continues the message being reassembled or starts a
                // new one with the previous header, advancing the
                // timestamp by the registered delta (§5.3.1.2.4).
                let st = self
                    .states
                    .get_mut(csid)
                    .expect("csid state present (checked above)");
                if st.assembling {
                    (false, st.msg_length)
                } else {
                    st.timestamp = st.timestamp.wrapping_add(st.last_delta);
                    (true, st.msg_length)
                }
            }
        };

        self.pos += total;
        self.received_bytes = self.received_bytes.wrapping_add(total as u32);

        let max_message = self.max_message_size.min(self.max_in_flight_bytes);
        let st = self
            .states
            .get_mut(csid)
            .expect("csid state present (set above)");
        if starts_message {
            // A fresh header supersedes any half-received message on
            // this csid.
            if st.assembling {
                st.assembling = false;
                self.in_flight_bytes -= st.msg_length as usize;
            }
            st.partial.clear();
            st.msg_length = length;
            let len = length as usize;
            if len > max_message {
                return Err(Error::InvalidChunk(format!(
                    "message length {len} on csid {csid} exceeds the {max_message}-byte limit"
                )));
            }
            if self.in_flight_bytes + len > self.max_in_flight_bytes {
                return Err(Error::InvalidChunk(format!(
                    "partially received messages would exceed the {}-byte reassembly budget",
                    self.max_in_flight_bytes
                )));
            }
            st.assembling = true;
            self.in_flight_bytes += len;
            // One exact allocation per message; bounded by the budget
            // checked just above.
            st.partial.reserve_exact(len);
        }
        let need = st.msg_length as usize - st.partial.len();
        self.current = Some(CurrentChunk {
            csid,
            remaining: need.min(self.chunk_size),
        });
        Ok(())
    }

    /// Move the current chunk's payload into its message. Returns the
    /// message when this chunk completed it, `None` when the chunk is
    /// done but the message needs more chunks.
    fn pull_payload(&mut self) -> Result<Option<Message>> {
        let Some(mut cur) = self.current else {
            return Ok(None);
        };
        while cur.remaining > 0 {
            if self.pos < self.end {
                let n = cur.remaining.min(self.end - self.pos);
                let st = self
                    .states
                    .get_mut(cur.csid)
                    .expect("csid state present for the current chunk");
                st.partial
                    .extend_from_slice(&self.buf[self.pos..self.pos + n]);
                self.pos += n;
                cur.remaining -= n;
                self.received_bytes = self.received_bytes.wrapping_add(n as u32);
                self.current = Some(cur);
            } else if cur.remaining >= READ_BUF_LEN {
                // Large payload: read straight into the message buffer.
                let st = self
                    .states
                    .get_mut(cur.csid)
                    .expect("csid state present for the current chunk");
                let before = st.partial.len();
                let res = (&mut self.stream)
                    .take(cur.remaining as u64)
                    .read_to_end(&mut st.partial);
                // `read_to_end` keeps whatever arrived before an error.
                let got = st.partial.len() - before;
                cur.remaining -= got;
                self.received_bytes = self.received_bytes.wrapping_add(got as u32);
                self.current = Some(cur);
                res.map_err(Error::Io)?;
                if cur.remaining > 0 {
                    return Err(eof());
                }
            } else {
                self.fill()?;
            }
        }
        self.current = None;
        let st = self
            .states
            .get_mut(cur.csid)
            .expect("csid state present for the current chunk");
        if st.partial.len() >= st.msg_length as usize {
            st.assembling = false;
            self.in_flight_bytes -= st.msg_length as usize;
            return Ok(Some(Message {
                msg_type_id: st.msg_type_id,
                msg_stream_id: st.msg_stream_id,
                timestamp: st.timestamp,
                payload: std::mem::take(&mut st.partial),
            }));
        }
        Ok(None)
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

/// The writer hands its staging buffer to the stream once this many
/// bytes have accumulated, so one message costs one `write` per
/// 64 KiB rather than several per chunk.
const WRITE_STAGE_LEN: usize = 64 * 1024;

/// Splits RTMP messages into chunks.
///
/// Every [`write_message`](Self::write_message) call hands the whole
/// message to the underlying stream before it returns (in as few
/// `write` calls as possible); nothing is held back between calls.
pub struct ChunkWriter<W: Write> {
    stream: W,
    chunk_size: usize,
    states: CsidTable<OutState>,
    /// Staging buffer the chunk headers + payload slices are laid out
    /// in before they are written.
    stage: Vec<u8>,
}

impl<W: Write> ChunkWriter<W> {
    pub fn new(stream: W) -> Self {
        Self {
            stream,
            chunk_size: DEFAULT_CHUNK_SIZE,
            states: CsidTable::new(),
            stage: Vec::new(),
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
        if !(2..=65_599).contains(&csid) {
            return Err(Error::ProtocolViolation(format!(
                "chunk stream id {csid} out of range"
            )));
        }
        if msg.payload.len() > MAX_MESSAGE_LENGTH {
            return Err(Error::ProtocolViolation(format!(
                "message payload of {} bytes exceeds the 24-bit length field",
                msg.payload.len()
            )));
        }
        let payload_len = msg.payload.len() as u32;

        let prev = self.states.entry(csid).clone();
        let delta = msg.timestamp.wrapping_sub(prev.timestamp);
        // Pick the most compact fmt we can get away with.
        let fmt = if !prev.primed
            || prev.msg_stream_id != msg.msg_stream_id
            || msg.timestamp < prev.timestamp
        {
            // First message on the csid, a different message stream, or
            // a timestamp that went backwards (rare, e.g. seek in the
            // source): re-prime with a full fmt 0 header.
            0
        } else if prev.msg_type_id != msg.msg_type_id || prev.msg_length != payload_len {
            1
        } else if delta == prev.last_delta {
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
            prev.last_had_ext_ts
        } else {
            ext_ts_needed
        };
        // Value carried by the ext field: the full 32-bit absolute
        // timestamp for fmt 0, the full 32-bit delta for fmt 1/2, and
        // the inherited delta for a fmt-3 head.
        let ext_value = match fmt {
            0 => msg.timestamp,
            1 | 2 => delta,
            _ => prev.last_delta,
        };

        let chunk_size = self.chunk_size;
        let mut stage = std::mem::take(&mut self.stage);
        stage.clear();
        let mut first_chunk_done = false;
        let mut cursor = 0usize;
        let mut result = Ok(());
        while cursor < msg.payload.len() || !first_chunk_done {
            let chunk_fmt = if !first_chunk_done { fmt } else { 3 };
            push_basic_header(&mut stage, chunk_fmt, csid);
            match chunk_fmt {
                0 => {
                    let ts_field = if ext_ts_needed {
                        0x00FF_FFFF
                    } else {
                        msg.timestamp
                    };
                    push_u24_be(&mut stage, ts_field);
                    push_u24_be(&mut stage, payload_len);
                    stage.push(msg.msg_type_id);
                    stage.extend_from_slice(&msg.msg_stream_id.to_le_bytes());
                    if ext_ts_needed {
                        stage.extend_from_slice(&msg.timestamp.to_be_bytes());
                    }
                }
                1 => {
                    let ts_field = if ext_ts_needed { 0x00FF_FFFF } else { delta };
                    push_u24_be(&mut stage, ts_field);
                    push_u24_be(&mut stage, payload_len);
                    stage.push(msg.msg_type_id);
                    if ext_ts_needed {
                        // §5.3.1.3: for a Type 1 chunk the field
                        // encodes the full 32-bit *delta*, not the
                        // absolute timestamp.
                        stage.extend_from_slice(&ext_value.to_be_bytes());
                    }
                }
                2 => {
                    let ts_field = if ext_ts_needed { 0x00FF_FFFF } else { delta };
                    push_u24_be(&mut stage, ts_field);
                    if ext_ts_needed {
                        stage.extend_from_slice(&ext_value.to_be_bytes());
                    }
                }
                _ => {
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
                        stage.extend_from_slice(&ext_value.to_be_bytes());
                    }
                }
            }
            let end = (cursor + chunk_size).min(msg.payload.len());
            let piece = &msg.payload[cursor..end];
            if piece.len() >= WRITE_STAGE_LEN {
                // A chunk this large gains nothing from staging: send
                // the pending headers, then the payload slice itself.
                result = self
                    .stream
                    .write_all(&stage)
                    .and_then(|()| self.stream.write_all(piece));
                stage.clear();
            } else {
                stage.extend_from_slice(piece);
                if stage.len() >= WRITE_STAGE_LEN {
                    result = self.stream.write_all(&stage);
                    stage.clear();
                }
            }
            if result.is_err() {
                break;
            }
            cursor = end;
            first_chunk_done = true;
        }
        if result.is_ok() && !stage.is_empty() {
            result = self.stream.write_all(&stage);
        }
        stage.clear();
        self.stage = stage;
        result?;

        // Commit the updated state.
        let st = self.states.entry(csid);
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
}

/// Append a chunk basic header (§5.3.1.1) for a csid already
/// validated to lie in 2..=65599.
fn push_basic_header(out: &mut Vec<u8>, fmt: u8, csid: u32) {
    match csid {
        2..=63 => out.push((fmt << 6) | (csid as u8)),
        64..=319 => out.extend_from_slice(&[fmt << 6, (csid - 64) as u8]),
        _ => {
            let v = (csid - 64) as u16;
            out.extend_from_slice(&[(fmt << 6) | 1, (v & 0xFF) as u8, (v >> 8) as u8]);
        }
    }
}

fn push_u24_be(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&[(v >> 16) as u8, (v >> 8) as u8, v as u8]);
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

    /// A reader whose source hands out one byte per call and reports
    /// a read timeout before every byte — the worst case a socket
    /// with a short read timeout can produce.
    struct Stutter {
        data: Vec<u8>,
        pos: usize,
        ready: bool,
    }

    impl Read for Stutter {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if self.pos == self.data.len() {
                return Ok(0);
            }
            if !self.ready {
                self.ready = true;
                return Err(std::io::ErrorKind::WouldBlock.into());
            }
            self.ready = false;
            buf[0] = self.data[self.pos];
            self.pos += 1;
            Ok(1)
        }
    }

    /// A read timeout anywhere — inside a basic header, a message
    /// header, an extended timestamp or a payload — loses nothing:
    /// retrying `read_message` resumes where the failed call stopped.
    #[test]
    fn read_timeouts_are_resumable_at_every_byte() {
        // 128: every payload goes through the receive buffer; 65 536:
        // the large message takes the direct-to-message read path.
        for chunk_size in [128usize, 65_536] {
            let mut wire = Vec::new();
            let sent = vec![
                (3u32, msg(20, 0, 5, 40)),
                (70, msg(9, 1, 0x0100_0000, 300)),
                (400, msg(8, 1, 0x0100_0010, 0)),
                (70, msg(9, 1, 0x0200_0000, 300)),
                (5, msg(9, 1, 77, 17_000)),
            ];
            {
                let mut w = ChunkWriter::new(&mut wire);
                w.set_chunk_size(chunk_size);
                for (csid, m) in &sent {
                    w.write_message(*csid, m).unwrap();
                }
            }
            let wire_len = wire.len();
            let mut r = ChunkReader::new(Stutter {
                data: wire,
                pos: 0,
                ready: false,
            });
            r.set_chunk_size(chunk_size);
            let mut got = Vec::new();
            let mut timeouts = 0usize;
            while got.len() < sent.len() {
                match r.read_message() {
                    Ok(m) => got.push(m),
                    Err(Error::Io(e)) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        timeouts += 1
                    }
                    Err(e) => panic!("unexpected error: {e}"),
                }
            }
            assert_eq!(timeouts, wire_len, "one timeout per wire byte");
            for ((_, want), have) in sent.iter().zip(&got) {
                assert_eq!(have.msg_type_id, want.msg_type_id);
                assert_eq!(have.msg_stream_id, want.msg_stream_id);
                assert_eq!(have.timestamp, want.timestamp);
                assert_eq!(have.payload, want.payload);
            }
            assert_eq!(r.received_bytes() as usize, wire_len);
            assert!(r.is_idle());
        }
    }

    /// `is_idle` is true exactly on message boundaries.
    #[test]
    fn idle_only_between_messages() {
        let mut wire = Vec::new();
        {
            let mut w = ChunkWriter::new(&mut wire);
            w.write_message(5, &msg(9, 1, 0, 300)).unwrap();
        }
        // Cut inside the second chunk: EOF arrives mid-message.
        let cut = &wire[..wire.len() - 10];
        let mut r = ChunkReader::new(Cursor::new(cut));
        assert!(r.is_idle());
        assert!(r.read_message().is_err());
        assert!(
            !r.is_idle(),
            "a truncated message leaves the reader mid-flight"
        );
        // The whole message: idle again afterwards, and the EOF that
        // follows is a clean boundary.
        let mut r = ChunkReader::new(Cursor::new(&wire));
        r.read_message().unwrap();
        assert!(r.is_idle());
        assert!(r.read_message().is_err());
        assert!(r.is_idle());
    }

    /// A declared length above the configured ceiling is refused from
    /// the 12 header bytes alone — before any payload is buffered.
    #[test]
    fn oversize_message_length_is_refused_from_the_header() {
        // fmt 0, csid 3, ts 0, length 0xFFFFFF, type 9, stream 1.
        let header = [3u8, 0, 0, 0, 0xFF, 0xFF, 0xFF, 9, 1, 0, 0, 0];
        let mut r = ChunkReader::new(Cursor::new(&header[..]));
        r.set_max_message_size(1 << 20);
        assert_eq!(r.max_message_size(), 1 << 20);
        assert!(matches!(r.read_message(), Err(Error::InvalidChunk(_))));
        // At the default ceiling the same header is acceptable and the
        // reader simply waits for payload (EOF here).
        let mut r = ChunkReader::new(Cursor::new(&header[..]));
        assert!(matches!(r.read_message(), Err(Error::Io(_))));
    }

    /// Messages started on many chunk streams and never finished
    /// cannot pin more than the in-flight budget.
    #[test]
    fn in_flight_budget_bounds_unfinished_messages() {
        // Each csid: fmt-0 header declaring 60 000 bytes, then one
        // 128-byte chunk of it.
        let mut wire = Vec::new();
        for csid in 3u8..40 {
            wire.extend_from_slice(&[csid, 0, 0, 0, 0x00, 0xEA, 0x60, 9, 1, 0, 0, 0]);
            wire.extend_from_slice(&[0x5A; 128]);
        }
        // An unrelated short message at the end.
        wire.extend_from_slice(&[2, 0, 0, 0, 0, 0, 4, 1, 0, 0, 0, 0, 0, 0, 16, 0]);
        // Generous budget: every partial is held, the short message
        // still comes through.
        let mut r = ChunkReader::new(Cursor::new(&wire));
        let m = r.read_message().unwrap();
        assert_eq!((m.msg_type_id, m.payload.len()), (1, 4));
        // 256 KiB budget: the fifth 60 000-byte message crosses it.
        let mut r = ChunkReader::new(Cursor::new(&wire));
        r.set_max_in_flight_bytes(256 * 1024);
        assert_eq!(r.max_in_flight_bytes(), 256 * 1024);
        match r.read_message() {
            Err(Error::InvalidChunk(m)) => assert!(m.contains("reassembly budget"), "{m}"),
            other => panic!("expected the budget error, got {other:?}"),
        }
    }

    /// Finishing, superseding or aborting a message returns its bytes
    /// to the budget.
    #[test]
    fn in_flight_budget_is_released() {
        let mut wire = Vec::new();
        for round in 0..50u8 {
            // Start a 60 000-byte message on csid 3 (first chunk only)…
            wire.extend_from_slice(&[3, 0, 0, 0, 0x00, 0xEA, 0x60, 9, 1, 0, 0, 0]);
            wire.extend_from_slice(&[round; 128]);
            // …and supersede it with a complete 4-byte one.
            wire.extend_from_slice(&[3, 0, 0, 0, 0, 0, 4, 9, 1, 0, 0, 0, 1, 2, 3, round]);
        }
        let mut r = ChunkReader::new(Cursor::new(&wire));
        r.set_max_in_flight_bytes(64 * 1024);
        for round in 0..50u8 {
            let m = r.read_message().unwrap();
            assert_eq!(m.payload, vec![1, 2, 3, round]);
        }
        assert!(r.is_idle());
    }

    /// The writer refuses what the wire cannot express instead of
    /// emitting a corrupt header.
    #[test]
    fn writer_rejects_unrepresentable_messages() {
        let mut out = Vec::new();
        let mut w = ChunkWriter::new(&mut out);
        for csid in [0u32, 1, 65_600, u32::MAX] {
            assert!(matches!(
                w.write_message(csid, &msg(9, 1, 0, 4)),
                Err(Error::ProtocolViolation(_))
            ));
        }
        let huge = Message {
            msg_type_id: 9,
            msg_stream_id: 1,
            timestamp: 0,
            payload: vec![0; MAX_MESSAGE_LENGTH + 1],
        };
        assert!(matches!(
            w.write_message(5, &huge),
            Err(Error::ProtocolViolation(_))
        ));
        assert!(out.is_empty(), "nothing reaches the wire on a refusal");
    }

    /// Two- and three-byte basic headers at the edges of their ranges
    /// (§5.3.1.1: ids 64–319 and 64–65599).
    #[test]
    fn extended_csid_forms_roundtrip() {
        let ids = [2u32, 63, 64, 65, 319, 320, 321, 1000, 65_598, 65_599];
        let mut wire = Vec::new();
        {
            let mut w = ChunkWriter::new(&mut wire);
            w.set_chunk_size(50);
            for (i, csid) in ids.iter().enumerate() {
                w.write_message(*csid, &msg(9, 1, i as u32 * 10, 120))
                    .unwrap();
            }
        }
        let mut r = ChunkReader::new(Cursor::new(&wire));
        r.set_chunk_size(50);
        for i in 0..ids.len() {
            let m = r.read_message().unwrap();
            assert_eq!(m.timestamp, i as u32 * 10);
            assert_eq!(m.payload.len(), 120);
        }
        assert!(r.is_idle());
    }
}
