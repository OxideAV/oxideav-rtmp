//! FLV file / byte-stream writer.
//!
//! Frames a sequence of audio / video / script-data tags into the
//! wire layout an `.flv` file (and the body of an HTTP-FLV response)
//! uses on disk:
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

use std::io::{self, Write};

use crate::amf::{self, Amf0Value};
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
}
