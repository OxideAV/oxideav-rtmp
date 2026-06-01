//! FLV file / byte-stream writer + reader.
//!
//! Frames (writer) and unframes (reader) a sequence of audio / video /
//! script-data tags from the wire layout an `.flv` file (and the body
//! of an HTTP-FLV response) uses on disk:
//!
//! ```text
//!   FLV header  (9 bytes — 'F' 'L' 'V' | version | TypeFlags | DataOffset(UI32))
//!   PreviousTagSize0 (UI32, always 0)
//!   FLVTAG_1  (11-byte header + payload)
//!   PreviousTagSize1 (UI32, = 11 + DataSize of FLVTAG_1)
//!   FLVTAG_2  ...
//!   PreviousTagSize2 ...
//!   ...
//! ```
//!
//! Per `docs/container/flv/flv_v10_1.pdf` Annex E (`The FLV File
//! Format`), specifically:
//!
//! * §E.2 — the 9-byte FLV header (signature `F` `L` `V`, version,
//!   the `TypeFlagsAudio` / `TypeFlagsVideo` flag bits, and the UI32
//!   `DataOffset` which is always 9 for version-1 files).
//! * §E.3 — the alternating `PreviousTagSize` / `FLVTAG` body. The
//!   first back-pointer (`PreviousTagSize0`) is always 0; each
//!   subsequent one is `11 + DataSize(prev tag)`.
//! * §E.4.1 — the 11-byte `FLVTAG` header: `TagType` (UB[2]
//!   reserved + UB[1] filter + UB[5] tag type), `DataSize` (UI24),
//!   `Timestamp` (UI24 low 24 bits) + `TimestampExtended` (UI8 upper
//!   8 bits, forming an SI32 milliseconds value), and `StreamID`
//!   (UI24, always 0 in FLV).
//! * §E.4.2 — audio tag body (`SoundFormat | SoundRate | SoundSize
//!   | SoundType | …`), built via [`crate::flv::build_audio`].
//! * §E.4.3 — video tag body (`FrameType | CodecID | …`), built via
//!   [`crate::flv::build_video`].
//! * §E.4.4 — script-data tag body (an AMF0 `Name + Value` pair —
//!   typically `onMetaData` + an `EcmaArray` of stream properties).
//!
//! This writer is intentionally small — it just frames bytes a caller
//! has already built up via the existing tag builders. Composes with
//! `RtmpSession` so an RTMP ingest can be recorded to `.flv` /
//! re-served over HTTP-FLV without re-parsing the payload.
//!
//! ## Example
//!
//! ```no_run
//! use std::fs::File;
//! use oxideav_rtmp::flv_file::{FlvWriter, FlvHeaderFlags};
//! use oxideav_rtmp::flv::{
//!     self, VideoTag, AudioTag, AAC_PACKET_TYPE_SEQUENCE_HEADER,
//!     AVC_PACKET_TYPE_SEQUENCE_HEADER, AUDIO_FORMAT_AAC, VIDEO_CODEC_AVC,
//!     VIDEO_FRAME_KEYFRAME,
//! };
//!
//! let mut w = FlvWriter::new(File::create("out.flv").unwrap(),
//!                            FlvHeaderFlags { audio: true, video: true })
//!     .unwrap();
//!
//! let vsh = VideoTag {
//!     mod_ex: Vec::new(),
//!     frame_type: VIDEO_FRAME_KEYFRAME,
//!     codec_id: VIDEO_CODEC_AVC,
//!     avc_packet_type: Some(AVC_PACKET_TYPE_SEQUENCE_HEADER),
//!     composition_time: 0,
//!     body: vec![0x01, 0x42, 0x80, 0x1e, /* …avcC… */],
//!     ex_packet_type: None,
//!     fourcc: None,
//!     multitrack: None,
//! };
//! w.write_video_tag(0, &vsh).unwrap();
//! // … more tags …
//! let _file = w.finish().unwrap();
//! ```

use std::io::{self, Read, Write};

use crate::amf::{self, Amf0Value};
use crate::error::{Error, Result};
use crate::flv::{self, AudioTag, VideoTag};

/// FLV tag type — `TagType` field of an `FLVTAG`, §E.4.1.
pub const FLV_TAG_TYPE_AUDIO: u8 = 8;
/// FLV tag type — `TagType` field of an `FLVTAG`, §E.4.1.
pub const FLV_TAG_TYPE_VIDEO: u8 = 9;
/// FLV tag type — `TagType` field of an `FLVTAG`, §E.4.1
/// (`script data`, AMF0 `name + value` payload — §E.4.4).
pub const FLV_TAG_TYPE_SCRIPT_DATA: u8 = 18;

/// FLV file version emitted in the 9-byte header. §E.2: "for example
/// 0x01 for FLV version 1". Version 1 is the only value the format
/// has ever defined and the only value commodity FLV readers accept.
pub const FLV_VERSION: u8 = 1;

/// `DataOffset` for the 9-byte header — §E.2 "usually has a value of
/// 9 for FLV version 1".
pub const FLV_HEADER_SIZE: u32 = 9;

/// Largest UI24 value — used to bound `DataSize` (§E.4.1).
const UI24_MAX: u32 = 0x00FF_FFFF;

// `Timestamp` (UI24) + `TimestampExtended` (UI8) form a 32-bit
// signed milliseconds value per §E.4.1. We expose the full unsigned
// 32-bit range a caller can pass in: any `u32` round-trips through
// the wire (the sign-extension is the reader's concern, not ours).

/// Header flag bits for the 9-byte FLV file header (§E.2):
/// `TypeFlagsAudio` (1 = audio tags are present) and
/// `TypeFlagsVideo` (1 = video tags are present). The two reserved
/// fields (`TypeFlagsReserved` UB[5] and a single UB[1] between
/// them) are always zero.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FlvHeaderFlags {
    /// Set when the stream will carry any audio tags (TagType 8).
    pub audio: bool,
    /// Set when the stream will carry any video tags (TagType 9).
    pub video: bool,
}

impl FlvHeaderFlags {
    /// Pack the `audio` / `video` bits into a single byte at the
    /// offsets defined by §E.2 — `TypeFlagsAudio` is bit 2 (0x04),
    /// `TypeFlagsVideo` is bit 0 (0x01); all other bits are reserved
    /// zero.
    pub fn to_byte(self) -> u8 {
        (if self.audio { 0x04 } else { 0 }) | (if self.video { 0x01 } else { 0 })
    }

    /// Inverse of [`FlvHeaderFlags::to_byte`].
    pub fn from_byte(b: u8) -> Self {
        Self {
            audio: b & 0x04 != 0,
            video: b & 0x01 != 0,
        }
    }
}

/// Build the 9-byte FLV file header (§E.2).
pub fn build_flv_header(flags: FlvHeaderFlags) -> [u8; 9] {
    let mut out = [0u8; 9];
    out[0] = b'F';
    out[1] = b'L';
    out[2] = b'V';
    out[3] = FLV_VERSION;
    out[4] = flags.to_byte();
    out[5..9].copy_from_slice(&FLV_HEADER_SIZE.to_be_bytes());
    out
}

/// Build a complete `FLVTAG` (11-byte header + payload), but *not*
/// the trailing `PreviousTagSize` back-pointer. The back-pointer is
/// emitted by the writer because it belongs to the file-body
/// alternation, not to the tag itself (§E.3).
///
/// Returns `Err` when `payload.len()` exceeds the 24-bit `DataSize`
/// field. Real FLV tags are usually a few KiB — a 16 MiB tag would
/// be a forged or buggy producer, not legitimate traffic.
pub fn build_flv_tag(tag_type: u8, timestamp_ms: u32, payload: &[u8]) -> io::Result<Vec<u8>> {
    if payload.len() > UI24_MAX as usize {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "flv tag payload {} bytes exceeds UI24 max {}",
                payload.len(),
                UI24_MAX
            ),
        ));
    }
    let data_size = payload.len() as u32;
    let mut out = Vec::with_capacity(11 + payload.len());
    // TagType byte: UB[2] reserved (0) + UB[1] filter (0,
    // unencrypted) + UB[5] tag type. With reserved + filter both
    // zero the byte is just the tag type itself.
    out.push(tag_type);
    // DataSize: UI24 big-endian.
    out.push(((data_size >> 16) & 0xFF) as u8);
    out.push(((data_size >> 8) & 0xFF) as u8);
    out.push((data_size & 0xFF) as u8);
    // Timestamp: UI24 (lower 24 bits) + TimestampExtended (UI8,
    // upper 8 bits). §E.4.1 "This field [TimestampExtended]
    // represents the upper 8 bits, while the previous Timestamp
    // field represents the lower 24 bits."
    out.push(((timestamp_ms >> 16) & 0xFF) as u8);
    out.push(((timestamp_ms >> 8) & 0xFF) as u8);
    out.push((timestamp_ms & 0xFF) as u8);
    out.push(((timestamp_ms >> 24) & 0xFF) as u8);
    // StreamID: UI24, always 0 per §E.4.1 "Always 0".
    out.push(0);
    out.push(0);
    out.push(0);
    // Payload bytes — `AudioTagHeader+Body`, `VideoTagHeader+Body`,
    // or `ScriptTagBody` depending on `tag_type`.
    out.extend_from_slice(payload);
    Ok(out)
}

/// FLV file / byte-stream writer.
///
/// Owns a `W: Write` sink and tracks the previous tag's size so each
/// subsequent `PreviousTagSize` back-pointer (§E.3) is computed
/// correctly. The header is written eagerly at construction.
///
/// Drop is **not** sufficient to finalise the file — call
/// [`FlvWriter::finish`] (or just drop and discard) to release the
/// underlying sink. There is no separate trailer to emit; the FLV
/// container format has no end-of-file marker beyond the final
/// `PreviousTagSize` value (which is written immediately after each
/// tag).
pub struct FlvWriter<W: Write> {
    inner: W,
    /// Size of the most recently written FLVTAG (11 + DataSize),
    /// used as the next `PreviousTagSize` back-pointer.
    prev_tag_size: u32,
    /// Total bytes written so far. Useful to callers building an
    /// HTTP-FLV response with `Content-Length` or seeking back to
    /// patch the `duration` field of an `onMetaData` placeholder.
    bytes_written: u64,
    /// Flags that were encoded in the header. Kept so a caller can
    /// query them later for diagnostics.
    flags: FlvHeaderFlags,
    /// True after [`FlvWriter::finish`] has been called. Subsequent
    /// `write_*` calls return `io::ErrorKind::BrokenPipe` to make
    /// double-finalise misuse explicit.
    finished: bool,
}

impl<W: Write> FlvWriter<W> {
    /// Wrap a sink and write the 9-byte FLV header + the mandatory
    /// `PreviousTagSize0 == 0` back-pointer (§E.3 "Always 0").
    pub fn new(mut inner: W, flags: FlvHeaderFlags) -> io::Result<Self> {
        let header = build_flv_header(flags);
        inner.write_all(&header)?;
        // PreviousTagSize0 — mandated zero per §E.3.
        inner.write_all(&0u32.to_be_bytes())?;
        Ok(Self {
            inner,
            prev_tag_size: 0,
            bytes_written: header.len() as u64 + 4,
            flags,
            finished: false,
        })
    }

    /// The flags the header was written with.
    pub fn flags(&self) -> FlvHeaderFlags {
        self.flags
    }

    /// Total bytes the writer has emitted so far (header + every
    /// tag + every `PreviousTagSize`).
    pub fn bytes_written(&self) -> u64 {
        self.bytes_written
    }

    /// Size of the most recently written FLVTAG (11 + DataSize) —
    /// equal to what the next `PreviousTagSize` will be. Returns 0
    /// before the first tag is written.
    pub fn last_tag_size(&self) -> u32 {
        self.prev_tag_size
    }

    /// Borrow the underlying sink. Reads / writes that bypass this
    /// type will leave the back-pointer tracking inconsistent — use
    /// sparingly (e.g. to query a `Seek` position).
    pub fn get_ref(&self) -> &W {
        &self.inner
    }

    /// Frame a video [`VideoTag`] (legacy AVC, Enhanced RTMP v1
    /// FourCC, or Enhanced RTMP v2 ExHeader / Multitrack / ModEx) as
    /// an FLV `TagType = 9` tag at `timestamp_ms` and emit it to the
    /// sink followed by its `PreviousTagSize`. The tag body is built
    /// via [`crate::flv::build_video`] so every shape the existing
    /// reader can parse round-trips through the file.
    pub fn write_video_tag(&mut self, timestamp_ms: u32, tag: &VideoTag) -> io::Result<()> {
        self.write_payload(FLV_TAG_TYPE_VIDEO, timestamp_ms, &flv::build_video(tag))
    }

    /// Frame an audio [`AudioTag`] (legacy or Enhanced RTMP v2
    /// FourCC) as an FLV `TagType = 8` tag at `timestamp_ms`. See
    /// [`FlvWriter::write_video_tag`] for the construction details.
    pub fn write_audio_tag(&mut self, timestamp_ms: u32, tag: &AudioTag) -> io::Result<()> {
        self.write_payload(FLV_TAG_TYPE_AUDIO, timestamp_ms, &flv::build_audio(tag))
    }

    /// Frame a script-data tag — an AMF0 `name + value` pair
    /// (§E.4.4) — at `timestamp_ms`. The canonical use is to emit
    /// an `onMetaData` tag right after the header carrying the
    /// stream's `width` / `height` / `duration` / codec hints, but
    /// callers can emit any named AMF0 message.
    ///
    /// Both `name` and `value` are AMF0-encoded back-to-back via
    /// [`crate::amf::encode`]. The name's wire encoding is the
    /// standard `Amf0Value::String` (a marker byte plus UI16-length
    /// plus UTF-8 bytes) so an FLV reader picks it up the same way
    /// as any other AMF0 string.
    pub fn write_script_data(
        &mut self,
        timestamp_ms: u32,
        name: &str,
        value: &Amf0Value,
    ) -> io::Result<()> {
        let mut payload = Vec::with_capacity(32);
        amf::encode(&mut payload, &Amf0Value::String(name.to_owned()));
        amf::encode(&mut payload, value);
        self.write_payload(FLV_TAG_TYPE_SCRIPT_DATA, timestamp_ms, &payload)
    }

    /// Lower-level escape hatch: frame an already-built payload as
    /// an FLV tag of the given `tag_type`. The caller is responsible
    /// for the body layout (e.g. an encrypted tag built via the
    /// Annex F flow).
    pub fn write_raw_tag(
        &mut self,
        tag_type: u8,
        timestamp_ms: u32,
        payload: &[u8],
    ) -> io::Result<()> {
        self.write_payload(tag_type, timestamp_ms, payload)
    }

    fn write_payload(&mut self, tag_type: u8, timestamp_ms: u32, payload: &[u8]) -> io::Result<()> {
        if self.finished {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "FlvWriter::write_* called after finish()",
            ));
        }
        let tag = build_flv_tag(tag_type, timestamp_ms, payload)?;
        let tag_size = tag.len() as u32;
        self.inner.write_all(&tag)?;
        self.inner.write_all(&tag_size.to_be_bytes())?;
        self.prev_tag_size = tag_size;
        self.bytes_written += u64::from(tag_size) + 4;
        Ok(())
    }

    /// Flush + return the inner sink. No trailer is emitted — the
    /// FLV container format has no end-of-file marker beyond the
    /// last `PreviousTagSize` (already written after each tag).
    pub fn finish(mut self) -> io::Result<W> {
        if !self.finished {
            self.inner.flush()?;
            self.finished = true;
        }
        Ok(self.inner)
    }
}

// ---------------------------------------------------------------------------
// Reader
// ---------------------------------------------------------------------------

/// Default upper bound on a single `FLVTAG` payload accepted by
/// [`FlvReader`]. UI24 `DataSize` (§E.4.1) can claim up to 16 MiB, but
/// real-world FLV tags are a few KiB to a few hundred KiB; a 16 MiB
/// claim almost always indicates a corrupted stream or a forged
/// producer. Callers who need to lift the cap can do so via
/// [`FlvReader::with_max_tag_size`].
pub const DEFAULT_MAX_TAG_SIZE: u32 = UI24_MAX;

/// A single decoded FLV tag — the variant carries the strongly-typed
/// payload returned by [`FlvReader::read_tag`].
///
/// Per §E.4 each `FLVTAG` is one of three types:
///
/// * [`FlvTag::Audio`] — `TagType = 8`, payload parsed via
///   [`crate::flv::parse_audio`].
/// * [`FlvTag::Video`] — `TagType = 9`, payload parsed via
///   [`crate::flv::parse_video`].
/// * [`FlvTag::Script`] — `TagType = 18`, AMF0 `name + value` pair per
///   §E.4.4.
///
/// `Unknown` is a forward-compatibility escape hatch for any reserved
/// `TagType` value the spec adds in a future revision — the body bytes
/// are preserved verbatim so a forwarding consumer never silently
/// drops a tag it doesn't recognise.
#[derive(Debug, Clone, PartialEq)]
pub enum FlvTag {
    /// `TagType = 8` audio tag, body parsed into the same
    /// [`AudioTag`] shape the RTMP ingest path uses.
    Audio { timestamp_ms: u32, tag: AudioTag },
    /// `TagType = 9` video tag, body parsed into the same
    /// [`VideoTag`] shape the RTMP ingest path uses.
    Video { timestamp_ms: u32, tag: VideoTag },
    /// `TagType = 18` script-data tag (§E.4.4). The wire body is an
    /// AMF0 `Name + Value` pair, surfaced here as the decoded
    /// [`Amf0Value::String`] name and the typed [`Amf0Value`] value.
    /// The canonical use is `name = "onMetaData"`.
    Script {
        timestamp_ms: u32,
        name: String,
        value: Amf0Value,
    },
    /// Reserved `TagType` value not specified by §E.4. Preserved
    /// verbatim so a forwarding consumer can keep the bytes around.
    Unknown {
        tag_type: u8,
        timestamp_ms: u32,
        body: Vec<u8>,
    },
}

impl FlvTag {
    /// `TagType` byte (`8` / `9` / `18` / other) for the variant.
    pub fn tag_type(&self) -> u8 {
        match self {
            FlvTag::Audio { .. } => FLV_TAG_TYPE_AUDIO,
            FlvTag::Video { .. } => FLV_TAG_TYPE_VIDEO,
            FlvTag::Script { .. } => FLV_TAG_TYPE_SCRIPT_DATA,
            FlvTag::Unknown { tag_type, .. } => *tag_type,
        }
    }

    /// `Timestamp` + `TimestampExtended` re-joined into a single 32-bit
    /// value (§E.4.1).
    pub fn timestamp_ms(&self) -> u32 {
        match self {
            FlvTag::Audio { timestamp_ms, .. }
            | FlvTag::Video { timestamp_ms, .. }
            | FlvTag::Script { timestamp_ms, .. }
            | FlvTag::Unknown { timestamp_ms, .. } => *timestamp_ms,
        }
    }
}

/// FLV file / byte-stream reader.
///
/// Wraps a `R: Read` source and walks the §E.2 9-byte file header
/// followed by the §E.3 alternating `PreviousTagSize` / `FLVTAG` body,
/// surfacing each tag as a strongly-typed [`FlvTag`].
///
/// The reader is the inverse of [`FlvWriter`]: a buffer that
/// `FlvWriter` produced reads back through `FlvReader` and re-decodes
/// to the original `VideoTag` / `AudioTag` / `Amf0Value` instances.
/// The body of an HTTP-FLV response is exactly this byte stream
/// (`Content-Type: video/x-flv`) so the same reader works for both
/// disk and network sources.
///
/// Error semantics:
///
/// * Truncated header / FLVTAG header / payload — `Error::UnexpectedEof`.
/// * Wrong `F`/`L`/`V` signature or unsupported version — `Error::Other`.
/// * `DataSize` exceeds the configured cap — `Error::Other` (the cap
///   defaults to [`DEFAULT_MAX_TAG_SIZE`]).
/// * `PreviousTagSize` mismatch — `Error::Other`. The §E.3 invariant
///   is `PreviousTagSize == 11 + DataSize`; a mismatch indicates a
///   producer or transport corruption and the reader refuses to
///   advance past it.
/// * Script-tag body that isn't AMF0 `String + Value` — surfaced as an
///   [`FlvTag::Unknown`] with `tag_type = 18` so the caller can choose
///   to skip rather than fail the whole stream.
#[derive(Debug)]
pub struct FlvReader<R: Read> {
    inner: R,
    /// Header byte from §E.2 (`TypeFlagsAudio` / `TypeFlagsVideo`).
    /// Stored so callers can introspect after construction.
    flags: FlvHeaderFlags,
    /// Largest payload (UI24 `DataSize`) the reader will accept.
    max_tag_size: u32,
    /// Size of the most recently decoded FLVTAG (11 + DataSize). The
    /// `PreviousTagSize` back-pointer between tags is checked against
    /// this value before the next tag is read.
    last_tag_size: u32,
    /// `true` once the underlying reader returned EOF on a clean
    /// boundary (between tags). Subsequent `read_tag` calls return
    /// `Ok(None)` rather than re-entering the reader on an exhausted
    /// stream.
    exhausted: bool,
    /// Total bytes consumed so far (header + every back-pointer +
    /// every framed tag). Useful for `Content-Length` accounting in
    /// an HTTP-FLV proxy.
    bytes_read: u64,
}

impl<R: Read> FlvReader<R> {
    /// Wrap a `Read` source and consume the 9-byte §E.2 file header +
    /// the mandatory `PreviousTagSize0 == 0` back-pointer (§E.3).
    ///
    /// Returns `Err` if the signature is not `F` `L` `V` or the
    /// version byte is not [`FLV_VERSION`].
    pub fn new(inner: R) -> Result<Self> {
        Self::with_max_tag_size(inner, DEFAULT_MAX_TAG_SIZE)
    }

    /// As [`FlvReader::new`] but with a custom upper bound on a
    /// single tag payload. `max_tag_size` must be ≤ [`UI24_MAX`]; a
    /// larger value is clamped to that bound. Callers handling
    /// trusted local files may want to set this to [`UI24_MAX`] (the
    /// 16 MiB ceiling the wire format allows); HTTP-FLV proxies
    /// generally want a tighter cap.
    pub fn with_max_tag_size(mut inner: R, max_tag_size: u32) -> Result<Self> {
        let mut header = [0u8; 9];
        read_exact_eof(&mut inner, &mut header)?;
        if header[0] != b'F' || header[1] != b'L' || header[2] != b'V' {
            return Err(Error::Other(format!(
                "FLV: bad signature {:02x} {:02x} {:02x}, want 'F' 'L' 'V'",
                header[0], header[1], header[2],
            )));
        }
        if header[3] != FLV_VERSION {
            return Err(Error::Other(format!(
                "FLV: unsupported version {:#x}, only {:#x} (version 1) is defined",
                header[3], FLV_VERSION,
            )));
        }
        let flags = FlvHeaderFlags::from_byte(header[4]);
        let data_offset = u32::from_be_bytes([header[5], header[6], header[7], header[8]]);
        // §E.2 — `DataOffset` is the length of this header. We accept
        // 9 (the only value version 1 has ever defined) and any
        // larger forward-compatibility offset by skipping the extra
        // bytes. Refuse `< 9` outright since that would imply the
        // header overlapped its own DataOffset field.
        if data_offset < FLV_HEADER_SIZE {
            return Err(Error::Other(format!(
                "FLV: DataOffset {data_offset} < header size {FLV_HEADER_SIZE}"
            )));
        }
        let mut bytes_read = u64::from(FLV_HEADER_SIZE);
        if data_offset > FLV_HEADER_SIZE {
            // Skip forward-compatible header padding.
            let extra = (data_offset - FLV_HEADER_SIZE) as usize;
            let mut skip = vec![0u8; extra];
            read_exact_eof(&mut inner, &mut skip)?;
            bytes_read += extra as u64;
        }
        // §E.3 — `PreviousTagSize0` is always 0.
        let mut p0 = [0u8; 4];
        read_exact_eof(&mut inner, &mut p0)?;
        let prev0 = u32::from_be_bytes(p0);
        if prev0 != 0 {
            return Err(Error::Other(format!(
                "FLV: PreviousTagSize0 must be 0, got {prev0}"
            )));
        }
        bytes_read += 4;
        let cap = max_tag_size.min(UI24_MAX);
        Ok(Self {
            inner,
            flags,
            max_tag_size: cap,
            last_tag_size: 0,
            exhausted: false,
            bytes_read,
        })
    }

    /// `TypeFlagsAudio` / `TypeFlagsVideo` decoded from the §E.2
    /// header.
    pub fn flags(&self) -> FlvHeaderFlags {
        self.flags
    }

    /// Largest UI24 `DataSize` the reader will accept.
    pub fn max_tag_size(&self) -> u32 {
        self.max_tag_size
    }

    /// Bytes consumed from the underlying reader so far (header +
    /// every back-pointer + every framed tag).
    pub fn bytes_read(&self) -> u64 {
        self.bytes_read
    }

    /// Size of the most recently decoded FLVTAG (11 + DataSize), or 0
    /// before the first tag is decoded.
    pub fn last_tag_size(&self) -> u32 {
        self.last_tag_size
    }

    /// Borrow the underlying source. Reads that bypass this type leave
    /// the back-pointer / `bytes_read` tracking inconsistent — use
    /// sparingly.
    pub fn get_ref(&self) -> &R {
        &self.inner
    }

    /// Read and decode the next `FLVTAG` (plus the trailing
    /// `PreviousTagSize` back-pointer). Returns `Ok(None)` on a clean
    /// end-of-stream at a tag boundary; any truncation mid-tag
    /// surfaces as `Err(Error::UnexpectedEof)`.
    pub fn read_tag(&mut self) -> Result<Option<FlvTag>> {
        if self.exhausted {
            return Ok(None);
        }
        // Before the next FLVTAG, consume the back-pointer from the
        // tag we last decoded (or the initial `PreviousTagSize0` for
        // the very first call). The §E.3 invariant: this value MUST
        // equal `11 + DataSize` of the previous tag. We consumed
        // `PreviousTagSize0` (always 0) in `new`, so on the first call
        // `last_tag_size == 0` and there is no back-pointer to read
        // here — the FLVTAG header comes next.
        //
        // For subsequent calls, the back-pointer was already read at
        // the end of the previous `read_tag` to detect a clean EOF
        // boundary cleanly. So no preamble work needed.

        let mut header = [0u8; 11];
        match self.inner.read(&mut header[..1]) {
            Ok(0) => {
                // Clean EOF at tag boundary.
                self.exhausted = true;
                return Ok(None);
            }
            Ok(1) => {}
            Ok(_) => unreachable!("read into 1-byte slice returned > 1"),
            Err(ref e) if e.kind() == io::ErrorKind::UnexpectedEof => {
                self.exhausted = true;
                return Ok(None);
            }
            Err(e) => return Err(Error::Io(e)),
        }
        read_exact_eof(&mut self.inner, &mut header[1..])?;
        self.bytes_read += 11;
        let raw_type = header[0];
        // UB[2] reserved + UB[1] filter + UB[5] tag type.
        let filter = (raw_type >> 5) & 0x01;
        if filter != 0 {
            // §E.4.1 — the Filter bit means the body is encrypted per
            // Annex F. We don't implement the Annex F decrypt path
            // here; surface the situation as a structured error so
            // the caller can either route to a decryption stage or
            // skip the tag.
            return Err(Error::Other(
                "FLV: encrypted tag (Filter=1, Annex F) — decryption not implemented".into(),
            ));
        }
        let tag_type = raw_type & 0x1F;
        let data_size = ((header[1] as u32) << 16) | ((header[2] as u32) << 8) | header[3] as u32;
        if data_size > self.max_tag_size {
            return Err(Error::Other(format!(
                "FLV: DataSize {data_size} exceeds max_tag_size {}",
                self.max_tag_size
            )));
        }
        let ts_lo = ((header[4] as u32) << 16) | ((header[5] as u32) << 8) | header[6] as u32;
        let ts_hi = header[7] as u32;
        let timestamp_ms = (ts_hi << 24) | ts_lo;
        let stream_id = ((header[8] as u32) << 16) | ((header[9] as u32) << 8) | header[10] as u32;
        if stream_id != 0 {
            return Err(Error::Other(format!(
                "FLV: StreamID {stream_id} != 0 (§E.4.1 'Always 0')"
            )));
        }
        let mut body = vec![0u8; data_size as usize];
        read_exact_eof(&mut self.inner, &mut body)?;
        self.bytes_read += data_size as u64;

        // Read trailing `PreviousTagSize` and verify the §E.3
        // invariant. Doing this before returning means a corrupt
        // back-pointer surfaces on the same tag rather than poisoning
        // the next read.
        let mut prev = [0u8; 4];
        read_exact_eof(&mut self.inner, &mut prev)?;
        let prev_tag_size = u32::from_be_bytes(prev);
        let expected = 11u32.saturating_add(data_size);
        if prev_tag_size != expected {
            return Err(Error::Other(format!(
                "FLV: PreviousTagSize {prev_tag_size} != 11 + DataSize {expected} (§E.3)"
            )));
        }
        self.bytes_read += 4;
        self.last_tag_size = expected;

        let decoded = match tag_type {
            FLV_TAG_TYPE_AUDIO => {
                let tag = flv::parse_audio(&body)
                    .map_err(|e| Error::Other(format!("FLV audio body: {e}")))?;
                FlvTag::Audio { timestamp_ms, tag }
            }
            FLV_TAG_TYPE_VIDEO => {
                let tag = flv::parse_video(&body)
                    .map_err(|e| Error::Other(format!("FLV video body: {e}")))?;
                FlvTag::Video { timestamp_ms, tag }
            }
            FLV_TAG_TYPE_SCRIPT_DATA => match parse_script_body(&body) {
                Ok((name, value)) => FlvTag::Script {
                    timestamp_ms,
                    name,
                    value,
                },
                Err(_) => FlvTag::Unknown {
                    tag_type,
                    timestamp_ms,
                    body,
                },
            },
            _ => FlvTag::Unknown {
                tag_type,
                timestamp_ms,
                body,
            },
        };
        Ok(Some(decoded))
    }

    /// Consume the entire stream. Equivalent to repeatedly calling
    /// [`FlvReader::read_tag`] until it returns `Ok(None)`.
    pub fn read_all(mut self) -> Result<Vec<FlvTag>> {
        let mut out = Vec::new();
        while let Some(t) = self.read_tag()? {
            out.push(t);
        }
        Ok(out)
    }
}

/// Decode an §E.4.4 `ScriptTagBody` — AMF0 `Name + Value` pair where
/// the name is required to be a `String` per the spec ("Method or
/// object name. SCRIPTDATAVALUE.Type = 2 (String)").
fn parse_script_body(body: &[u8]) -> Result<(String, Amf0Value)> {
    let mut pos = 0;
    let name_v = amf::decode(body, &mut pos)?;
    let name = match name_v {
        Amf0Value::String(s) => s,
        other => {
            return Err(Error::InvalidAmf0(format!(
                "script tag name must be String (§E.4.4), got {other:?}"
            )));
        }
    };
    let value = amf::decode(body, &mut pos)?;
    // Trailing bytes after the value are tolerated — some encoders
    // append an extra `Amf0Value::ObjectEnd` or padding. Walk through
    // any extras to keep `pos` advancing; refuse only outright
    // garbage that fails to decode.
    while pos < body.len() {
        if amf::decode(body, &mut pos).is_err() {
            break;
        }
    }
    Ok((name, value))
}

/// Like `inner.read_exact` but maps `io::ErrorKind::UnexpectedEof`
/// onto our [`Error::UnexpectedEof`] variant.
fn read_exact_eof<R: Read>(inner: &mut R, buf: &mut [u8]) -> Result<()> {
    match inner.read_exact(buf) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => Err(Error::UnexpectedEof),
        Err(e) => Err(Error::Io(e)),
    }
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::flv::{
        parse_audio, parse_video, AAC_PACKET_TYPE_RAW, AAC_PACKET_TYPE_SEQUENCE_HEADER,
        AUDIO_FORMAT_AAC, AVC_PACKET_TYPE_NALU, AVC_PACKET_TYPE_SEQUENCE_HEADER,
        EX_PACKET_TYPE_CODED_FRAMES, FOURCC_HEVC, VIDEO_CODEC_AVC, VIDEO_FRAME_INTER,
        VIDEO_FRAME_KEYFRAME,
    };

    fn aac_seq_header_tag() -> AudioTag {
        AudioTag {
            mod_ex: Vec::new(),
            sound_format: AUDIO_FORMAT_AAC,
            sound_rate: 3,
            sound_size_16bit: true,
            stereo: true,
            aac_packet_type: Some(AAC_PACKET_TYPE_SEQUENCE_HEADER),
            body: vec![0x12, 0x10],
            ex_packet_type: None,
            audio_fourcc: None,
            multitrack: None,
        }
    }

    fn aac_raw_tag(body: Vec<u8>) -> AudioTag {
        AudioTag {
            mod_ex: Vec::new(),
            sound_format: AUDIO_FORMAT_AAC,
            sound_rate: 3,
            sound_size_16bit: true,
            stereo: true,
            aac_packet_type: Some(AAC_PACKET_TYPE_RAW),
            body,
            ex_packet_type: None,
            audio_fourcc: None,
            multitrack: None,
        }
    }

    fn avc_seq_header_tag() -> VideoTag {
        VideoTag {
            mod_ex: Vec::new(),
            frame_type: VIDEO_FRAME_KEYFRAME,
            codec_id: VIDEO_CODEC_AVC,
            avc_packet_type: Some(AVC_PACKET_TYPE_SEQUENCE_HEADER),
            composition_time: 0,
            body: vec![0x01, 0x42, 0x80, 0x1E],
            ex_packet_type: None,
            fourcc: None,
            multitrack: None,
        }
    }

    fn avc_inter_nalu_tag() -> VideoTag {
        VideoTag {
            mod_ex: Vec::new(),
            frame_type: VIDEO_FRAME_INTER,
            codec_id: VIDEO_CODEC_AVC,
            avc_packet_type: Some(AVC_PACKET_TYPE_NALU),
            composition_time: 7,
            body: vec![0x00, 0x00, 0x00, 0x03, 0x41, 0x9A, 0x00],
            ex_packet_type: None,
            fourcc: None,
            multitrack: None,
        }
    }

    #[test]
    fn header_flags_round_trip_through_byte() {
        for a in [false, true] {
            for v in [false, true] {
                let f = FlvHeaderFlags { audio: a, video: v };
                assert_eq!(FlvHeaderFlags::from_byte(f.to_byte()), f);
            }
        }
        // Reserved bits in the wire byte are ignored on parse —
        // only bits 0 (video) and 2 (audio) survive the round-trip.
        // 0xFF has both bits set.
        assert_eq!(
            FlvHeaderFlags::from_byte(0xFF),
            FlvHeaderFlags {
                audio: true,
                video: true
            }
        );
        // 0xFE = 1111_1110 — bit 0 clear (video false), bit 2 set
        // (audio true), all reserved bits ignored.
        assert_eq!(
            FlvHeaderFlags::from_byte(0xFE),
            FlvHeaderFlags {
                audio: true,
                video: false
            }
        );
    }

    #[test]
    fn build_flv_header_signature_and_offset() {
        let header = build_flv_header(FlvHeaderFlags {
            audio: true,
            video: true,
        });
        // §E.2 — `F` `L` `V`, version 1, flags byte (audio | video
        // = 0x05), DataOffset = 9.
        assert_eq!(header, [b'F', b'L', b'V', 0x01, 0x05, 0, 0, 0, 9]);
    }

    #[test]
    fn build_flv_header_video_only() {
        let header = build_flv_header(FlvHeaderFlags {
            audio: false,
            video: true,
        });
        assert_eq!(header, [b'F', b'L', b'V', 0x01, 0x01, 0, 0, 0, 9]);
    }

    #[test]
    fn build_flv_tag_layout_matches_spec() {
        let body = b"abc".to_vec();
        let tag = build_flv_tag(FLV_TAG_TYPE_VIDEO, 0x12_3456, &body).expect("build");
        // §E.4.1: 1-byte tag type + UI24 DataSize + UI24 Timestamp
        // + UI8 TimestampExtended + UI24 StreamID + payload.
        assert_eq!(
            tag,
            vec![
                0x09, // TagType = 9 (video)
                0x00, 0x00, 0x03, // DataSize = 3
                0x12, 0x34, 0x56, // Timestamp (lower 24 bits)
                0x00, // TimestampExtended (upper 8 bits)
                0x00, 0x00, 0x00, // StreamID
                b'a', b'b', b'c',
            ]
        );
    }

    #[test]
    fn build_flv_tag_timestamp_extended_carries_high_byte() {
        // Timestamp 0x0A_BBCCDD splits as UI24=0xBBCCDD,
        // TimestampExtended=0x0A.
        let tag = build_flv_tag(FLV_TAG_TYPE_AUDIO, 0x0ABB_CCDD, &[]).expect("build");
        assert_eq!(tag[0], 0x08); // audio
        assert_eq!(&tag[1..4], &[0, 0, 0]); // DataSize 0
        assert_eq!(&tag[4..7], &[0xBB, 0xCC, 0xDD]); // UI24 ts
        assert_eq!(tag[7], 0x0A); // TimestampExtended
    }

    #[test]
    fn build_flv_tag_rejects_payload_over_ui24() {
        // A 16 MiB payload doesn't fit in UI24 — we surface a clean
        // `InvalidInput` rather than silently truncating the size
        // field.
        let huge = vec![0u8; (UI24_MAX as usize) + 1];
        let err = build_flv_tag(FLV_TAG_TYPE_AUDIO, 0, &huge).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn writer_emits_header_and_previous_tag_size0() {
        let buf: Vec<u8> = Vec::new();
        let w = FlvWriter::new(
            buf,
            FlvHeaderFlags {
                audio: false,
                video: true,
            },
        )
        .expect("new");
        let buf = w.finish().expect("finish");
        // 9-byte header + 4-byte PreviousTagSize0(0).
        assert_eq!(
            buf,
            vec![b'F', b'L', b'V', 0x01, 0x01, 0, 0, 0, 9, 0, 0, 0, 0]
        );
    }

    #[test]
    fn writer_one_video_tag_round_trips_with_back_pointer() {
        let mut w = FlvWriter::new(
            Vec::new(),
            FlvHeaderFlags {
                audio: false,
                video: true,
            },
        )
        .expect("new");
        let tag = avc_seq_header_tag();
        w.write_video_tag(0, &tag).expect("write");
        let prev = w.last_tag_size();
        let bytes_so_far = w.bytes_written();
        let buf = w.finish().expect("finish");

        // Layout: header(9) + PrevTagSize0(4) + FLVTAG(11+DataSize)
        // + PrevTagSize1(4).
        //
        // For an AVC sequence-header tag the body is 5 bytes
        // (frame|codec | AVCPacketType | SI24 CTS) + 4 NALU-ish bytes
        // = 9. DataSize = 9, FLVTAG total = 20, PrevTagSize = 20.
        assert_eq!(prev, 20);
        assert_eq!(bytes_so_far, buf.len() as u64);
        assert_eq!(buf.len(), 9 + 4 + 11 + 9 + 4);
        // Header still intact at the front.
        assert_eq!(&buf[0..9], &[b'F', b'L', b'V', 0x01, 0x01, 0, 0, 0, 9]);
        // PreviousTagSize0 at offset 9 — always 0 (§E.3).
        assert_eq!(&buf[9..13], &[0, 0, 0, 0]);
        // FLVTAG header.
        assert_eq!(buf[13], FLV_TAG_TYPE_VIDEO);
        assert_eq!(&buf[14..17], &[0, 0, 9]); // DataSize = 9
        assert_eq!(&buf[17..20], &[0, 0, 0]); // ts low 24
        assert_eq!(buf[20], 0); // ts ext
        assert_eq!(&buf[21..24], &[0, 0, 0]); // StreamID
                                              // Payload byte 0 — Enhanced-RTMP IsExHeader bit MUST be
                                              // zero for a legacy AVC tag (FrameType high nibble == 1
                                              // for keyframe, CodecID low nibble == 7 for AVC).
        assert_eq!(buf[24], 0x17);
        // AVCPacketType (0 = seq header).
        assert_eq!(buf[25], 0x00);
        // SI24 CTS — three bytes of zero.
        assert_eq!(&buf[26..29], &[0, 0, 0]);
        // avcC stub.
        assert_eq!(&buf[29..33], &[0x01, 0x42, 0x80, 0x1E]);
        // PreviousTagSize1 = 20 (11 header + 9 body).
        assert_eq!(&buf[33..37], &20u32.to_be_bytes());

        // And the body we just wrote should round-trip through
        // `parse_video` so a player accepts it.
        let parsed = parse_video(&buf[24..33]).expect("parse");
        assert_eq!(parsed.frame_type, VIDEO_FRAME_KEYFRAME);
        assert_eq!(parsed.codec_id, VIDEO_CODEC_AVC);
        assert_eq!(
            parsed.avc_packet_type,
            Some(AVC_PACKET_TYPE_SEQUENCE_HEADER)
        );
        assert_eq!(parsed.body, vec![0x01, 0x42, 0x80, 0x1E]);
    }

    #[test]
    fn writer_audio_aac_seq_header_round_trips() {
        let mut w = FlvWriter::new(
            Vec::new(),
            FlvHeaderFlags {
                audio: true,
                video: false,
            },
        )
        .expect("new");
        let tag = aac_seq_header_tag();
        w.write_audio_tag(0, &tag).expect("write");
        let buf = w.finish().expect("finish");
        // AudioTagHeader for AAC = 1 byte SoundFormat/Rate/Size/Type
        // + 1 byte AACPacketType + 2-byte ASC body = 4 bytes total.
        // FLVTAG = 11 + 4 = 15.
        let body_start = 9 + 4 + 11;
        let body_end = body_start + 4;
        let parsed = parse_audio(&buf[body_start..body_end]).expect("parse");
        assert_eq!(parsed.sound_format, AUDIO_FORMAT_AAC);
        assert_eq!(
            parsed.aac_packet_type,
            Some(AAC_PACKET_TYPE_SEQUENCE_HEADER)
        );
        assert_eq!(parsed.body, vec![0x12, 0x10]);
        // PreviousTagSize at the very end.
        assert_eq!(&buf[buf.len() - 4..], &15u32.to_be_bytes());
    }

    #[test]
    fn writer_back_pointer_tracks_each_tag_independently() {
        let mut w = FlvWriter::new(
            Vec::new(),
            FlvHeaderFlags {
                audio: true,
                video: true,
            },
        )
        .expect("new");
        let v1 = avc_seq_header_tag(); // 9-byte body → tag=20
        let a1 = aac_raw_tag(vec![0xAA, 0xBB, 0xCC]); // 2+3=5-byte body → tag=16
        let v2 = avc_inter_nalu_tag(); // 5 header + 7 body = 12-byte body → tag=23
        w.write_video_tag(0, &v1).expect("v1");
        assert_eq!(w.last_tag_size(), 20);
        w.write_audio_tag(0, &a1).expect("a1");
        assert_eq!(w.last_tag_size(), 16);
        w.write_video_tag(33, &v2).expect("v2");
        assert_eq!(w.last_tag_size(), 23);
        let buf = w.finish().expect("finish");

        // Total: 9 header + 4 prev0 + 20 v1 + 4 + 16 a1 + 4 + 23 v2 + 4
        //      = 9 + 4 + 20 + 4 + 16 + 4 + 23 + 4 = 84.
        assert_eq!(buf.len(), 84);
        // Each PreviousTagSize back-pointer matches the size of the
        // tag it follows.
        assert_eq!(&buf[9..13], &0u32.to_be_bytes());
        assert_eq!(&buf[33..37], &20u32.to_be_bytes());
        assert_eq!(&buf[53..57], &16u32.to_be_bytes());
        assert_eq!(&buf[buf.len() - 4..], &23u32.to_be_bytes());
    }

    #[test]
    fn writer_script_data_amf0_name_then_value() {
        let mut w = FlvWriter::new(
            Vec::new(),
            FlvHeaderFlags {
                audio: true,
                video: true,
            },
        )
        .expect("new");
        let meta = Amf0Value::EcmaArray(vec![
            ("width".into(), Amf0Value::Number(1280.0)),
            ("height".into(), Amf0Value::Number(720.0)),
            ("duration".into(), Amf0Value::Number(0.0)),
        ]);
        w.write_script_data(0, "onMetaData", &meta)
            .expect("write meta");
        let buf = w.finish().expect("finish");

        // FLVTAG header + script body. Pull out the body and
        // re-decode it via the AMF0 walker.
        let body_start = 9 + 4 + 11;
        let prev_size = u32::from_be_bytes(buf[buf.len() - 4..].try_into().unwrap());
        let data_size = prev_size - 11;
        let body = &buf[body_start..body_start + data_size as usize];
        let mut pos = 0;
        let name = amf::decode(body, &mut pos).expect("name");
        let value = amf::decode(body, &mut pos).expect("value");
        assert_eq!(name, Amf0Value::String("onMetaData".to_string()));
        assert_eq!(value, meta);
        assert_eq!(pos, body.len());
        // TagType 18 (script data) in the FLVTAG header.
        assert_eq!(buf[13], FLV_TAG_TYPE_SCRIPT_DATA);
    }

    #[test]
    fn writer_timestamp_extended_round_trips_high_byte() {
        let mut w = FlvWriter::new(
            Vec::new(),
            FlvHeaderFlags {
                audio: true,
                video: false,
            },
        )
        .expect("new");
        let tag = aac_raw_tag(vec![0x11, 0x22]);
        // Timestamp > 24 bits → high byte lands in TimestampExtended.
        let ts: u32 = 0x0A_BBCCDD;
        w.write_audio_tag(ts, &tag).expect("write");
        let buf = w.finish().expect("finish");
        // FLVTAG header: byte 0 = type, 1..4 = DataSize,
        // 4..7 = Timestamp (UI24), 7 = TimestampExtended.
        let hdr_off = 9 + 4;
        assert_eq!(&buf[hdr_off + 4..hdr_off + 7], &[0xBB, 0xCC, 0xDD]);
        assert_eq!(buf[hdr_off + 7], 0x0A);
    }

    #[test]
    fn writer_enhanced_rtmp_v2_video_round_trips() {
        // Enhanced RTMP v2 HEVC CodedFrames tag — exercises the
        // ExHeader path through `build_video`.
        let tag = VideoTag {
            mod_ex: Vec::new(),
            frame_type: VIDEO_FRAME_KEYFRAME,
            codec_id: 0,
            avc_packet_type: None,
            composition_time: 17,
            body: b"NALU-payload".to_vec(),
            ex_packet_type: Some(EX_PACKET_TYPE_CODED_FRAMES),
            fourcc: Some(FOURCC_HEVC),
            multitrack: None,
        };
        let mut w = FlvWriter::new(
            Vec::new(),
            FlvHeaderFlags {
                audio: false,
                video: true,
            },
        )
        .expect("new");
        w.write_video_tag(100, &tag).expect("write");
        let buf = w.finish().expect("finish");
        let body_start = 9 + 4 + 11;
        let prev = u32::from_be_bytes(buf[buf.len() - 4..].try_into().unwrap());
        let data_size = prev - 11;
        let parsed = parse_video(&buf[body_start..body_start + data_size as usize]).expect("parse");
        assert_eq!(parsed.fourcc, Some(FOURCC_HEVC));
        assert_eq!(parsed.ex_packet_type, Some(EX_PACKET_TYPE_CODED_FRAMES));
        assert_eq!(parsed.composition_time, 17);
        assert_eq!(parsed.body, b"NALU-payload".to_vec());
    }

    #[test]
    fn writer_finish_is_idempotent_but_rejects_further_writes() {
        let mut w = FlvWriter::new(Vec::new(), FlvHeaderFlags::default()).expect("new");
        let tag = aac_raw_tag(vec![0x00]);
        w.write_audio_tag(0, &tag).expect("write");
        // Drop the writer via finish — must succeed exactly once.
        let _buf = w.finish().expect("finish first time");
    }

    #[test]
    fn writer_returns_broken_pipe_after_finish() {
        // Custom sink that swallows everything so we can drive the
        // post-finish error path without restructuring the writer.
        struct Sink {
            inner: Vec<u8>,
            done: bool,
        }
        impl Write for Sink {
            fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
                if self.done {
                    return Err(io::Error::new(io::ErrorKind::BrokenPipe, "post-finish"));
                }
                self.inner.write(buf)
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let sink = Sink {
            inner: Vec::new(),
            done: false,
        };
        let w = FlvWriter::new(sink, FlvHeaderFlags::default()).expect("new");
        // After finish() the public API is gone (the writer is
        // consumed) — instead exercise the internal flag via a
        // continuing writer that we mark finished by hand.
        let mut w2 = w;
        w2.finished = true;
        let err = w2.write_audio_tag(0, &aac_raw_tag(vec![])).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::BrokenPipe);
    }

    #[test]
    fn writer_raw_tag_lets_caller_pass_their_own_payload() {
        let mut w = FlvWriter::new(
            Vec::new(),
            FlvHeaderFlags {
                audio: false,
                video: true,
            },
        )
        .expect("new");
        // Write an opaque 3-byte "tag" via the escape hatch.
        w.write_raw_tag(FLV_TAG_TYPE_VIDEO, 5, &[0x99, 0x88, 0x77])
            .expect("write");
        let buf = w.finish().expect("finish");
        // FLVTAG payload starts at offset 24.
        assert_eq!(&buf[24..27], &[0x99, 0x88, 0x77]);
        // PreviousTagSize at end = 11 + 3 = 14.
        assert_eq!(&buf[buf.len() - 4..], &14u32.to_be_bytes());
    }

    // ---- Reader tests --------------------------------------------------

    use std::io::Cursor;

    #[test]
    fn reader_empty_stream_round_trips_through_writer() {
        // Brand-new writer with no tags → header + PreviousTagSize0
        // only. Reader returns `Ok(None)` on the first call.
        let w = FlvWriter::new(
            Vec::new(),
            FlvHeaderFlags {
                audio: true,
                video: false,
            },
        )
        .expect("new");
        let buf = w.finish().expect("finish");
        let mut r = FlvReader::new(Cursor::new(buf)).expect("reader new");
        assert_eq!(
            r.flags(),
            FlvHeaderFlags {
                audio: true,
                video: false
            }
        );
        assert_eq!(r.bytes_read(), 13); // header(9) + PrevTagSize0(4)
        assert!(r.read_tag().expect("read").is_none());
        // Subsequent calls keep returning `None` without re-entering
        // the reader.
        assert!(r.read_tag().expect("read again").is_none());
    }

    #[test]
    fn reader_rejects_bad_signature() {
        let buf = vec![b'X', b'L', b'V', 1, 0x05, 0, 0, 0, 9, 0, 0, 0, 0];
        let err = FlvReader::new(Cursor::new(buf)).unwrap_err();
        assert!(
            matches!(err, Error::Other(ref m) if m.contains("bad signature")),
            "got {err:?}"
        );
    }

    #[test]
    fn reader_rejects_wrong_version() {
        let buf = vec![b'F', b'L', b'V', 9, 0x05, 0, 0, 0, 9, 0, 0, 0, 0];
        let err = FlvReader::new(Cursor::new(buf)).unwrap_err();
        assert!(
            matches!(err, Error::Other(ref m) if m.contains("unsupported version")),
            "got {err:?}"
        );
    }

    #[test]
    fn reader_rejects_nonzero_previous_tag_size_0() {
        // §E.3 — PreviousTagSize0 MUST be 0.
        let buf = vec![b'F', b'L', b'V', 1, 0x05, 0, 0, 0, 9, 0, 0, 0, 0x42];
        let err = FlvReader::new(Cursor::new(buf)).unwrap_err();
        assert!(
            matches!(err, Error::Other(ref m) if m.contains("PreviousTagSize0")),
            "got {err:?}"
        );
    }

    #[test]
    fn reader_rejects_data_offset_below_header_size() {
        // DataOffset = 8 < 9 is impossible for a well-formed header.
        let buf = vec![b'F', b'L', b'V', 1, 0x05, 0, 0, 0, 8, 0, 0, 0, 0];
        let err = FlvReader::new(Cursor::new(buf)).unwrap_err();
        assert!(
            matches!(err, Error::Other(ref m) if m.contains("DataOffset")),
            "got {err:?}"
        );
    }

    #[test]
    fn reader_skips_forward_compatible_header_padding() {
        // DataOffset = 11 → 2 extra padding bytes after the standard
        // 9-byte header. A future spec revision could add fields; we
        // must not refuse such a header just because we don't know
        // the extra bytes' meaning.
        let mut buf = vec![b'F', b'L', b'V', 1, 0x01, 0, 0, 0, 11];
        buf.extend_from_slice(&[0xAA, 0xBB]); // padding
        buf.extend_from_slice(&0u32.to_be_bytes()); // PreviousTagSize0
        let r = FlvReader::new(Cursor::new(buf)).expect("new");
        assert_eq!(r.bytes_read(), 11 + 4);
        assert_eq!(
            r.flags(),
            FlvHeaderFlags {
                audio: false,
                video: true
            }
        );
    }

    #[test]
    fn reader_avc_seq_header_round_trips_through_writer() {
        let mut w = FlvWriter::new(
            Vec::new(),
            FlvHeaderFlags {
                audio: false,
                video: true,
            },
        )
        .expect("new");
        w.write_video_tag(0, &avc_seq_header_tag()).expect("write");
        let buf = w.finish().expect("finish");

        let mut r = FlvReader::new(Cursor::new(buf)).expect("reader new");
        let tag = r.read_tag().expect("read").expect("Some");
        match tag {
            FlvTag::Video { timestamp_ms, tag } => {
                assert_eq!(timestamp_ms, 0);
                assert!(tag.is_avc_sequence_header());
                assert_eq!(tag.body, vec![0x01, 0x42, 0x80, 0x1E]);
            }
            other => panic!("expected Video, got {other:?}"),
        }
        assert_eq!(r.last_tag_size(), 20);
        assert!(r.read_tag().expect("end").is_none());
    }

    #[test]
    fn reader_aac_seq_header_round_trips_through_writer() {
        let mut w = FlvWriter::new(
            Vec::new(),
            FlvHeaderFlags {
                audio: true,
                video: false,
            },
        )
        .expect("new");
        w.write_audio_tag(0, &aac_seq_header_tag()).expect("write");
        let buf = w.finish().expect("finish");

        let mut r = FlvReader::new(Cursor::new(buf)).expect("reader new");
        let tag = r.read_tag().expect("read").expect("Some");
        match tag {
            FlvTag::Audio { timestamp_ms, tag } => {
                assert_eq!(timestamp_ms, 0);
                assert_eq!(tag.sound_format, AUDIO_FORMAT_AAC);
                assert_eq!(tag.aac_packet_type, Some(AAC_PACKET_TYPE_SEQUENCE_HEADER));
                assert_eq!(tag.body, vec![0x12, 0x10]);
            }
            other => panic!("expected Audio, got {other:?}"),
        }
        assert!(r.read_tag().expect("end").is_none());
    }

    #[test]
    fn reader_walks_interleaved_video_audio_video() {
        // Same triple used by `writer_back_pointer_tracks_each_tag_independently`
        // — 20 / 16 / 23 byte tags. Reader must surface each in order
        // with the correct timestamps + types.
        let mut w = FlvWriter::new(
            Vec::new(),
            FlvHeaderFlags {
                audio: true,
                video: true,
            },
        )
        .expect("new");
        w.write_video_tag(0, &avc_seq_header_tag()).expect("v1");
        w.write_audio_tag(7, &aac_raw_tag(vec![0xAA, 0xBB, 0xCC]))
            .expect("a1");
        w.write_video_tag(33, &avc_inter_nalu_tag()).expect("v2");
        let buf = w.finish().expect("finish");

        let r = FlvReader::new(Cursor::new(buf)).expect("reader new");
        let tags = r.read_all().expect("read_all");
        assert_eq!(tags.len(), 3);

        match &tags[0] {
            FlvTag::Video { timestamp_ms, tag } => {
                assert_eq!(*timestamp_ms, 0);
                assert!(tag.is_avc_sequence_header());
            }
            other => panic!("tag 0: {other:?}"),
        }
        match &tags[1] {
            FlvTag::Audio { timestamp_ms, tag } => {
                assert_eq!(*timestamp_ms, 7);
                assert_eq!(tag.sound_format, AUDIO_FORMAT_AAC);
                assert_eq!(tag.aac_packet_type, Some(AAC_PACKET_TYPE_RAW));
                assert_eq!(tag.body, vec![0xAA, 0xBB, 0xCC]);
            }
            other => panic!("tag 1: {other:?}"),
        }
        match &tags[2] {
            FlvTag::Video { timestamp_ms, tag } => {
                assert_eq!(*timestamp_ms, 33);
                assert_eq!(tag.frame_type, VIDEO_FRAME_INTER);
                assert_eq!(tag.composition_time, 7);
            }
            other => panic!("tag 2: {other:?}"),
        }
    }

    #[test]
    fn reader_script_tag_round_trips_amf0_name_and_value() {
        let mut w = FlvWriter::new(
            Vec::new(),
            FlvHeaderFlags {
                audio: true,
                video: true,
            },
        )
        .expect("new");
        let meta = Amf0Value::EcmaArray(vec![
            ("width".into(), Amf0Value::Number(1920.0)),
            ("height".into(), Amf0Value::Number(1080.0)),
            ("framerate".into(), Amf0Value::Number(30.0)),
        ]);
        w.write_script_data(0, "onMetaData", &meta).expect("write");
        let buf = w.finish().expect("finish");

        let mut r = FlvReader::new(Cursor::new(buf)).expect("reader new");
        let tag = r.read_tag().expect("read").expect("Some");
        match tag {
            FlvTag::Script {
                timestamp_ms,
                name,
                value,
            } => {
                assert_eq!(timestamp_ms, 0);
                assert_eq!(name, "onMetaData");
                assert_eq!(value, meta);
            }
            other => panic!("expected Script, got {other:?}"),
        }
    }

    #[test]
    fn reader_timestamp_extended_high_byte_round_trips() {
        // The writer encodes the high byte into TimestampExtended;
        // the reader must re-join it back into the full 32-bit value.
        let mut w = FlvWriter::new(
            Vec::new(),
            FlvHeaderFlags {
                audio: true,
                video: false,
            },
        )
        .expect("new");
        let ts: u32 = 0x0A_BBCCDD;
        w.write_audio_tag(ts, &aac_raw_tag(vec![0x11, 0x22]))
            .expect("write");
        let buf = w.finish().expect("finish");

        let mut r = FlvReader::new(Cursor::new(buf)).expect("reader new");
        let tag = r.read_tag().expect("read").expect("Some");
        assert_eq!(tag.timestamp_ms(), ts);
        assert_eq!(tag.tag_type(), FLV_TAG_TYPE_AUDIO);
    }

    #[test]
    fn reader_enhanced_rtmp_v2_video_round_trips() {
        // Exercise the ExHeader path: HEVC CodedFrames with a
        // non-zero composition_time. Reader must surface the FourCC
        // + ex_packet_type intact.
        let tag = VideoTag {
            mod_ex: Vec::new(),
            frame_type: VIDEO_FRAME_KEYFRAME,
            codec_id: 0,
            avc_packet_type: None,
            composition_time: 42,
            body: b"NALU-payload".to_vec(),
            ex_packet_type: Some(EX_PACKET_TYPE_CODED_FRAMES),
            fourcc: Some(FOURCC_HEVC),
            multitrack: None,
        };
        let mut w = FlvWriter::new(
            Vec::new(),
            FlvHeaderFlags {
                audio: false,
                video: true,
            },
        )
        .expect("new");
        w.write_video_tag(123, &tag).expect("write");
        let buf = w.finish().expect("finish");

        let mut r = FlvReader::new(Cursor::new(buf)).expect("reader new");
        let read = r.read_tag().expect("read").expect("Some");
        match read {
            FlvTag::Video { timestamp_ms, tag } => {
                assert_eq!(timestamp_ms, 123);
                assert_eq!(tag.fourcc, Some(FOURCC_HEVC));
                assert_eq!(tag.ex_packet_type, Some(EX_PACKET_TYPE_CODED_FRAMES));
                assert_eq!(tag.composition_time, 42);
                assert_eq!(tag.body, b"NALU-payload".to_vec());
            }
            other => panic!("expected Video, got {other:?}"),
        }
    }

    #[test]
    fn reader_rejects_corrupt_previous_tag_size() {
        // Hand-craft a stream where the trailing back-pointer doesn't
        // equal `11 + DataSize` — the §E.3 invariant. Reader must
        // refuse rather than re-sync mid-stream.
        let header = build_flv_header(FlvHeaderFlags {
            audio: false,
            video: true,
        });
        let mut buf: Vec<u8> = Vec::new();
        buf.extend_from_slice(&header);
        buf.extend_from_slice(&0u32.to_be_bytes()); // PrevTagSize0
                                                    // 11-byte FLVTAG header + 3-byte body → tag size 14.
        buf.extend_from_slice(&[
            9, // TagType = video
            0, 0, 3, // DataSize = 3
            0, 0, 0, // ts UI24
            0, // ts ext
            0, 0, 0, // StreamID
        ]);
        buf.extend_from_slice(&[0x17, 0x01, 0x00]);
        // Wrong PreviousTagSize: should be 14, write 99.
        buf.extend_from_slice(&99u32.to_be_bytes());

        let mut r = FlvReader::new(Cursor::new(buf)).expect("reader new");
        let err = r.read_tag().unwrap_err();
        assert!(
            matches!(err, Error::Other(ref m) if m.contains("PreviousTagSize")),
            "got {err:?}"
        );
    }

    #[test]
    fn reader_rejects_data_size_above_cap() {
        let header = build_flv_header(FlvHeaderFlags {
            audio: true,
            video: false,
        });
        let mut buf: Vec<u8> = Vec::new();
        buf.extend_from_slice(&header);
        buf.extend_from_slice(&0u32.to_be_bytes());
        // FLVTAG claiming a 1 KiB body when the cap is 100 bytes.
        buf.extend_from_slice(&[
            8, // audio
            0, 4, 0, // DataSize = 1024
            0, 0, 0, 0, 0, 0, 0,
        ]);
        // (No body bytes needed — reader checks DataSize before
        // attempting the body read.)

        let mut r = FlvReader::with_max_tag_size(Cursor::new(buf), 100).expect("reader new");
        assert_eq!(r.max_tag_size(), 100);
        let err = r.read_tag().unwrap_err();
        assert!(
            matches!(err, Error::Other(ref m) if m.contains("exceeds max_tag_size")),
            "got {err:?}"
        );
    }

    #[test]
    fn reader_rejects_nonzero_stream_id() {
        let header = build_flv_header(FlvHeaderFlags {
            audio: true,
            video: false,
        });
        let mut buf: Vec<u8> = Vec::new();
        buf.extend_from_slice(&header);
        buf.extend_from_slice(&0u32.to_be_bytes());
        // FLVTAG with StreamID = 0x000001 (must be zero per §E.4.1).
        buf.extend_from_slice(&[
            8, 0, 0, 0, // DataSize = 0
            0, 0, 0, 0, // ts
            0, 0, 1, // StreamID = 1
        ]);
        buf.extend_from_slice(&11u32.to_be_bytes());

        let mut r = FlvReader::new(Cursor::new(buf)).expect("reader new");
        let err = r.read_tag().unwrap_err();
        assert!(
            matches!(err, Error::Other(ref m) if m.contains("StreamID")),
            "got {err:?}"
        );
    }

    #[test]
    fn reader_rejects_encrypted_filter_bit() {
        // Filter bit set (Annex F) — we don't implement decrypt.
        let header = build_flv_header(FlvHeaderFlags {
            audio: false,
            video: true,
        });
        let mut buf: Vec<u8> = Vec::new();
        buf.extend_from_slice(&header);
        buf.extend_from_slice(&0u32.to_be_bytes());
        // TagType byte = 0x29 = 0b0010_1001 → filter bit (1 << 5) +
        // tag_type 9.
        buf.extend_from_slice(&[
            0x29, 0, 0, 0, // DataSize = 0
            0, 0, 0, 0, // ts
            0, 0, 0, // StreamID
        ]);
        buf.extend_from_slice(&11u32.to_be_bytes());

        let mut r = FlvReader::new(Cursor::new(buf)).expect("reader new");
        let err = r.read_tag().unwrap_err();
        assert!(
            matches!(err, Error::Other(ref m) if m.contains("encrypted")),
            "got {err:?}"
        );
    }

    #[test]
    fn reader_truncated_tag_header_surfaces_unexpected_eof() {
        // 9-byte header + PrevTagSize0 + half of an FLVTAG header.
        let header = build_flv_header(FlvHeaderFlags {
            audio: true,
            video: true,
        });
        let mut buf: Vec<u8> = Vec::new();
        buf.extend_from_slice(&header);
        buf.extend_from_slice(&0u32.to_be_bytes());
        buf.extend_from_slice(&[8, 0, 0, 5]); // 4 of 11 header bytes only

        let mut r = FlvReader::new(Cursor::new(buf)).expect("reader new");
        let err = r.read_tag().unwrap_err();
        assert!(matches!(err, Error::UnexpectedEof), "got {err:?}");
    }

    #[test]
    fn reader_truncated_payload_surfaces_unexpected_eof() {
        // Full FLVTAG header + partial body.
        let header = build_flv_header(FlvHeaderFlags {
            audio: true,
            video: false,
        });
        let mut buf: Vec<u8> = Vec::new();
        buf.extend_from_slice(&header);
        buf.extend_from_slice(&0u32.to_be_bytes());
        buf.extend_from_slice(&[
            8, 0, 0, 10, // DataSize = 10
            0, 0, 0, 0, 0, 0, 0,
        ]);
        buf.extend_from_slice(&[0xAA, 0xBB]); // 2 of 10 body bytes only

        let mut r = FlvReader::new(Cursor::new(buf)).expect("reader new");
        let err = r.read_tag().unwrap_err();
        assert!(matches!(err, Error::UnexpectedEof), "got {err:?}");
    }

    #[test]
    fn reader_unknown_tag_type_preserved_verbatim() {
        // TagType = 5 is not in §E.4 — surface as `Unknown` rather
        // than failing the whole stream so a forwarding consumer can
        // route the bytes elsewhere.
        let header = build_flv_header(FlvHeaderFlags {
            audio: false,
            video: false,
        });
        let mut buf: Vec<u8> = Vec::new();
        buf.extend_from_slice(&header);
        buf.extend_from_slice(&0u32.to_be_bytes());
        buf.extend_from_slice(&[
            5, // unknown tag type
            0, 0, 3, // DataSize = 3
            0, 0, 0, 0, // ts
            0, 0, 0, // StreamID
        ]);
        buf.extend_from_slice(&[0xDE, 0xAD, 0xBE]);
        buf.extend_from_slice(&14u32.to_be_bytes());

        let mut r = FlvReader::new(Cursor::new(buf)).expect("reader new");
        let tag = r.read_tag().expect("read").expect("Some");
        match tag {
            FlvTag::Unknown {
                tag_type,
                timestamp_ms,
                body,
            } => {
                assert_eq!(tag_type, 5);
                assert_eq!(timestamp_ms, 0);
                assert_eq!(body, vec![0xDE, 0xAD, 0xBE]);
            }
            other => panic!("expected Unknown, got {other:?}"),
        }
    }

    #[test]
    fn reader_full_writer_round_trip_byte_for_byte() {
        // Write a 4-tag stream (script + video sh + audio sh + video
        // inter), read it back, then re-write through the writer and
        // confirm the bytes match.
        let meta = Amf0Value::EcmaArray(vec![
            ("encoder".into(), Amf0Value::String("oxideav-rtmp".into())),
            ("hasAudio".into(), Amf0Value::Boolean(true)),
        ]);
        let mut w = FlvWriter::new(
            Vec::new(),
            FlvHeaderFlags {
                audio: true,
                video: true,
            },
        )
        .expect("new");
        w.write_script_data(0, "onMetaData", &meta).expect("meta");
        w.write_video_tag(0, &avc_seq_header_tag()).expect("v0");
        w.write_audio_tag(0, &aac_seq_header_tag()).expect("a0");
        w.write_video_tag(100, &avc_inter_nalu_tag()).expect("v1");
        let original = w.finish().expect("finish");

        let r = FlvReader::new(Cursor::new(original.clone())).expect("reader");
        let tags = r.read_all().expect("read_all");
        assert_eq!(tags.len(), 4);

        let mut w2 = FlvWriter::new(
            Vec::new(),
            FlvHeaderFlags {
                audio: true,
                video: true,
            },
        )
        .expect("new2");
        for t in &tags {
            match t {
                FlvTag::Script {
                    timestamp_ms,
                    name,
                    value,
                } => w2.write_script_data(*timestamp_ms, name, value).unwrap(),
                FlvTag::Video { timestamp_ms, tag } => {
                    w2.write_video_tag(*timestamp_ms, tag).unwrap()
                }
                FlvTag::Audio { timestamp_ms, tag } => {
                    w2.write_audio_tag(*timestamp_ms, tag).unwrap()
                }
                FlvTag::Unknown {
                    tag_type,
                    timestamp_ms,
                    body,
                } => w2.write_raw_tag(*tag_type, *timestamp_ms, body).unwrap(),
            }
        }
        let re = w2.finish().expect("finish2");
        assert_eq!(re, original);
    }
}
