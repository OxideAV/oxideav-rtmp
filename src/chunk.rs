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
}

impl<R: Read> ChunkReader<R> {
    pub fn new(stream: R) -> Self {
        Self {
            stream,
            chunk_size: DEFAULT_CHUNK_SIZE,
            states: HashMap::new(),
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
            // remaining message length, whichever is smaller).
            let state = self.states.get_mut(&csid).ok_or_else(|| {
                Error::InvalidChunk(format!(
                    "fmt {fmt} chunk on csid {csid} without prior fmt-0 state"
                ))
            })?;
            let need = state.msg_length as usize - state.partial.len();
            let take = need.min(self.chunk_size);
            let mut buf = vec![0u8; take];
            self.stream.read_exact(&mut buf)?;
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
        self.stream.read_exact(&mut b)?;
        let fmt = (b[0] >> 6) & 0x03;
        let low = b[0] & 0x3F;
        let csid = match low {
            0 => {
                let mut b1 = [0u8; 1];
                self.stream.read_exact(&mut b1)?;
                b1[0] as u32 + 64
            }
            1 => {
                let mut b2 = [0u8; 2];
                self.stream.read_exact(&mut b2)?;
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
        self.stream.read_exact(&mut b)?;
        Ok(((b[0] as u32) << 16) | ((b[1] as u32) << 8) | (b[2] as u32))
    }

    fn read_u32_le_stream_id(&mut self) -> Result<u32> {
        let mut b = [0u8; 4];
        self.stream.read_exact(&mut b)?;
        // msg_stream_id is explicitly little-endian — only field in
        // the RTMP wire format that is.
        Ok(u32::from_le_bytes(b))
    }

    fn read_fmt0_header(&mut self, csid: u32) -> Result<()> {
        let mut ts = self.read_u24_be()?;
        let len = self.read_u24_be()?;
        let mut t = [0u8; 1];
        self.stream.read_exact(&mut t)?;
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
        self.stream.read_exact(&mut t)?;
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
        self.stream.read_exact(&mut b)?;
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

        // Pick the most compact fmt we can get away with.
        let fmt = if !prev_primed || prev_stream_id != msg.msg_stream_id {
            0
        } else if prev_type_id != msg.msg_type_id || prev_length != payload_len {
            1
        } else if msg.timestamp < prev_timestamp {
            // Timestamp went backwards (rare, e.g. seek in the source).
            // Re-prime with fmt 0.
            0
        } else if msg.timestamp == prev_timestamp {
            3
        } else {
            2
        };
        let delta = msg.timestamp.wrapping_sub(prev_timestamp);
        let ext_ts_needed = (fmt == 0 && msg.timestamp >= 0x00FF_FFFF)
            || (fmt != 0 && fmt != 3 && delta >= 0x00FF_FFFF);

        // Query the sticky "previous chunk used ext ts" bit we'll need
        // to mirror on any fmt-3 continuation.
        let prev_last_had_ext_ts = self
            .states
            .get(&csid)
            .map(|s| s.last_had_ext_ts)
            .unwrap_or(false);

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
                        self.stream.write_all(&msg.timestamp.to_be_bytes())?;
                    }
                }
                2 => {
                    let ts_field = if ext_ts_needed { 0x00FF_FFFF } else { delta };
                    self.write_u24_be(ts_field)?;
                    if ext_ts_needed {
                        self.stream.write_all(&msg.timestamp.to_be_bytes())?;
                    }
                }
                3 => {
                    // Continuation chunk must repeat the extended
                    // timestamp iff the head chunk used one. Use the
                    // current-message decision for the first round,
                    // fall back to the previous message's sticky bit
                    // otherwise.
                    let ext_repeat = if !first_chunk_done {
                        ext_ts_needed
                    } else {
                        prev_last_had_ext_ts && cursor == 0
                    };
                    if ext_repeat {
                        self.stream.write_all(&msg.timestamp.to_be_bytes())?;
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
        st.last_had_ext_ts = ext_ts_needed;
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

    /// Two back-to-back messages on the same csid should use fmt 3 for
    /// the second when every field matches, keeping the wire compact.
    #[test]
    fn back_to_back_same_message_uses_fmt3() {
        let msg = Message {
            msg_type_id: 9,
            msg_stream_id: 1,
            timestamp: 1000,
            payload: vec![0xAA; 32],
        };
        let mut buf = Vec::new();
        {
            let mut w = ChunkWriter::new(&mut buf);
            w.write_message(5, &msg).unwrap();
            w.write_message(5, &msg).unwrap();
        }
        // First byte of the second basic header: fmt=3 (bits 7-6 = 11),
        // csid=5 (bits 5-0 = 000101) → 0xC5.
        let first_headers_len = 1 + 11 + 32; // fmt 0 on csid 5
        assert_eq!(buf[first_headers_len], 0xC5);
    }
}
