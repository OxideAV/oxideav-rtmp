//! FLV-tag payload shape for RTMP audio / video messages.
//!
//! Real RTMP always carries H.264 + AAC (plus MP3 / Speex / Nellymoser
//! for audio on legacy flows; we treat those as opaque). The payload
//! layout inside type-8 / type-9 messages matches what an `.flv` file
//! stores in its audio / video tags, so the parsing code is identical
//! to FLV's.
//!
//! Callers of this module work in terms of:
//!
//! * [`VideoTag`] — frame type + codec + AVC packet type + NALU-ish
//!   body. For H.264, the first video message of a stream is an
//!   "AVC sequence header" (= `AVCDecoderConfigurationRecord`, aka
//!   avcC). Every subsequent keyframe / interframe is
//!   `AVCPacketType = 1` with length-prefixed NALUs.
//!
//! * [`AudioTag`] — format + rate/size/channels + AAC packet type +
//!   raw payload. For AAC, the first audio message is the
//!   `AudioSpecificConfig` (2-byte ASC for LC-AAC 44.1k stereo);
//!   subsequent messages carry raw AAC frames.
//!
//! These shapes are stable across every RTMP implementation — OBS,
//! Wirecast, ffmpeg's rtmpproto, node-media-server all emit the same
//! bytes.

use crate::error::{Error, Result};

// §E.4.3 "Video tag body" (FLV 10.1 spec annex E).
// frame type (high nibble of byte 0):
pub const VIDEO_FRAME_KEYFRAME: u8 = 1; // "seekable frame" aka IDR
pub const VIDEO_FRAME_INTER: u8 = 2;
pub const VIDEO_FRAME_DISPOSABLE: u8 = 3; // H.263 only
pub const VIDEO_FRAME_GENERATED_KEY: u8 = 4;
pub const VIDEO_FRAME_INFO: u8 = 5;

// codec id (low nibble of byte 0):
pub const VIDEO_CODEC_H263: u8 = 2;
pub const VIDEO_CODEC_SCREEN: u8 = 3;
pub const VIDEO_CODEC_VP6: u8 = 4;
pub const VIDEO_CODEC_VP6A: u8 = 5;
pub const VIDEO_CODEC_SCREEN_V2: u8 = 6;
pub const VIDEO_CODEC_AVC: u8 = 7; // H.264 — the one anyone uses in 2026

pub const AVC_PACKET_TYPE_SEQUENCE_HEADER: u8 = 0;
pub const AVC_PACKET_TYPE_NALU: u8 = 1;
pub const AVC_PACKET_TYPE_END_OF_SEQUENCE: u8 = 2;

// Enhanced RTMP v1, Table 4 "Extended VideoTagHeader" (Veovera
// Software Organization, 2023-2025). When the high bit of byte 0
// (the IsExHeader flag, value 0x80) is set, the low nibble is a
// `PacketType` rather than a legacy `CodecID`, and the four bytes
// that follow are a FourCC video-codec tag rather than the
// legacy AVC packet-type + composition-time bytes.
//
// IsExHeader sits at bit 7 of the first byte. Pre-2023 FLV
// `FrameType` values never reached 8, so the bit was always zero
// for legacy publishers — Enhanced RTMP repurposes it without
// breaking those clients (per the spec's backwards-compatibility
// note).
pub const VIDEO_IS_EX_HEADER: u8 = 0x80;

// Enhanced RTMP §"Defining Additional Video Codecs", Table 4 row
// `PacketType (i.e. not CodecId) — IF IsExHeader == 1, UB[4]`.
pub const EX_PACKET_TYPE_SEQUENCE_START: u8 = 0;
pub const EX_PACKET_TYPE_CODED_FRAMES: u8 = 1;
pub const EX_PACKET_TYPE_SEQUENCE_END: u8 = 2;
/// `CodedFramesX` — like `CodedFrames` but the SI24
/// `CompositionTime` is implied to be zero and therefore omitted
/// from the wire to save three bytes.
pub const EX_PACKET_TYPE_CODED_FRAMES_X: u8 = 3;
/// `Metadata` — the VideoTagBody carries an AMF-encoded `[name,
/// value]` metadata pair instead of coded video. The only
/// `name` Enhanced RTMP v1 defines is `"colorInfo"` (HDR
/// signalling). When this PacketType is present the `FrameType`
/// flags at the top of the header are required (per spec) to be
/// ignored.
pub const EX_PACKET_TYPE_METADATA: u8 = 4;
/// `MPEG2TSSequenceStart` — sequence-start variant whose body is
/// the codec's MPEG-2-TS-format descriptor (used by AV1's
/// `AV1VideoDescriptor`, mutually exclusive with
/// `PacketTypeSequenceStart` per the 2023-06-07 revision note).
pub const EX_PACKET_TYPE_MPEG2TS_SEQUENCE_START: u8 = 5;

// Enhanced RTMP §"Defining Additional Video Codecs", Table 4
// "Video FourCC" row. FourCCs are read as four ASCII bytes in
// reading order (i.e. `'a','v','0','1'`), interpreted as a UI32
// big-endian for comparison (`0x6176_3031`).
//
// `av01` / `vp09` / `hvc1` were added in Enhanced RTMP v1
// (Veovera 2023). `vp08` (VP8), `avc1` (FourCC-mode AVC/H.264),
// and `vvc1` (VVC/H.266) were added in Enhanced RTMP v2
// (Veovera 2026) — see enhanced-rtmp-v2.pdf §"Enhanced Video"
// `enum VideoFourCc { Vp8, Vp9, Av1, Avc, Hevc, Vvc }`.
pub const FOURCC_AV1: [u8; 4] = *b"av01";
pub const FOURCC_VP9: [u8; 4] = *b"vp09";
pub const FOURCC_HEVC: [u8; 4] = *b"hvc1";
/// Enhanced RTMP v2 — VP8 FourCC. SequenceStart body is a
/// `VPCodecConfigurationRecord` (same shape as VP9). CodedFrames
/// body is one or more full VP8 frames. CTS not on the wire (no
/// B-frames).
pub const FOURCC_VP8: [u8; 4] = *b"vp08";
/// Enhanced RTMP v2 — AVC/H.264 in FourCC mode. SequenceStart body
/// is the `AVCDecoderConfigurationRecord`; CodedFrames body is
/// one or more length-prefixed NALUs. Per
/// enhanced-rtmp-v2.pdf §"ExVideoTagBody" the SI24
/// `compositionTimeOffset` is on the wire for AVC + CodedFrames
/// (parallel to HEVC's row), and implied zero for
/// CodedFramesX.
pub const FOURCC_AVC: [u8; 4] = *b"avc1";
/// Enhanced RTMP v2 — VVC/H.266 FourCC. SequenceStart body is the
/// `VVCDecoderConfigurationRecord` (per ISO/IEC 14496-15:2024
/// §11.2.4.2). CodedFrames body is one or more length-prefixed
/// NALUs. Per §"ExVideoTagBody" the SI24
/// `compositionTimeOffset` is on the wire for VVC + CodedFrames
/// (mirrors AVC + HEVC) and implied zero for CodedFramesX.
pub const FOURCC_VVC: [u8; 4] = *b"vvc1";

// §E.4.2 "Audio tag body".
// sound format (high nibble of byte 0):
pub const AUDIO_FORMAT_PCM_LE: u8 = 0;
pub const AUDIO_FORMAT_ADPCM: u8 = 1;
pub const AUDIO_FORMAT_MP3: u8 = 2;
pub const AUDIO_FORMAT_PCM_LE_8BIT: u8 = 3;
pub const AUDIO_FORMAT_NELLYMOSER_16K_MONO: u8 = 4;
pub const AUDIO_FORMAT_NELLYMOSER_8K_MONO: u8 = 5;
pub const AUDIO_FORMAT_NELLYMOSER: u8 = 6;
pub const AUDIO_FORMAT_G711_ALAW: u8 = 7;
pub const AUDIO_FORMAT_G711_MULAW: u8 = 8;
pub const AUDIO_FORMAT_AAC: u8 = 10;
pub const AUDIO_FORMAT_SPEEX: u8 = 11;

// Enhanced RTMP v2, "Extended AudioTagHeader" (Veovera Software
// Organization, 2026-01-31). When the high nibble of the FLV
// AudioTagHeader byte (SoundFormat) equals `ExHeader = 9`, the
// low UB[4] is reinterpreted as an `AudioPacketType` rather than
// the legacy SoundRate(UB[2]) | SoundSize(UB[1]) | SoundType(UB[1])
// bit field, and the four bytes that follow are an `AudioFourCc`
// rather than the AAC packet-type marker.
//
// Spec: enhanced-rtmp-v2.pdf §"Enhanced Audio", `Extended
// AudioTagHeader` table (`soundFormat = UB[4] as SoundFormat`,
// `if soundFormat == SoundFormat.ExHeader { audioPacketType =
// UB[4] as AudioPacketType }`). Legacy publishers leave the
// high nibble in `0..=8 / 10..=11 / 14..=15` and the parser /
// builder retain the pre-2023 single-byte format unchanged.
pub const AUDIO_FORMAT_EX_HEADER: u8 = 9;

// AudioPacketType enum from the same Extended AudioTagHeader
// table. The values that carry semantics today:
pub const AUDIO_PACKET_TYPE_SEQUENCE_START: u8 = 0;
pub const AUDIO_PACKET_TYPE_CODED_FRAMES: u8 = 1;
/// `SequenceEnd` — signals end of the audio sequence for the
/// current track. Spec: "AudioPacketType.SequenceEnd is to have no
/// less than the same meaning as a silence message".
pub const AUDIO_PACKET_TYPE_SEQUENCE_END: u8 = 2;
/// `MultichannelConfig` — body specifies AudioChannelOrder +
/// channel count + (optionally) per-channel speaker mapping or a
/// 32-bit AudioChannelFlags mask. See §"ExAudioTagBody" pseudocode
/// for the layout.
pub const AUDIO_PACKET_TYPE_MULTICHANNEL_CONFIG: u8 = 4;
/// `Multitrack` — turns on audio multitrack mode. Body shape
/// (AvMultitrackType + per-track FourCc + track id +
/// sizeOfAudioTrack) is deferred to a follow-up round.
pub const AUDIO_PACKET_TYPE_MULTITRACK: u8 = 5;
/// `ModEx` — modifier/extension marker that introduces a chain
/// of size-prefixed ModEx packets before the real AudioPacketType
/// is read. The only ModEx subtype defined today is
/// `TimestampOffsetNano = 0`. Deferred to a follow-up round.
pub const AUDIO_PACKET_TYPE_MOD_EX: u8 = 7;

// Enhanced RTMP v2 §"Enhanced Audio", `enum AudioFourCc` block.
// FourCCs are read as four ASCII bytes in reading order
// (e.g. `'O','p','u','s'`), interpreted as a big-endian UI32 for
// comparison.
pub const FOURCC_AC3: [u8; 4] = *b"ac-3";
pub const FOURCC_EAC3: [u8; 4] = *b"ec-3";
pub const FOURCC_OPUS: [u8; 4] = *b"Opus";
pub const FOURCC_MP3: [u8; 4] = *b".mp3";
pub const FOURCC_FLAC: [u8; 4] = *b"fLaC";
pub const FOURCC_AAC: [u8; 4] = *b"mp4a";

pub const AAC_PACKET_TYPE_SEQUENCE_HEADER: u8 = 0;
pub const AAC_PACKET_TYPE_RAW: u8 = 1;

/// Decoded FLV video-tag header + payload. For H.264 the
/// `composition_time` is the signed CTS offset (ms) between the
/// decoder timestamp the RTMP chunk carries and the presentation
/// timestamp — callers add this to the chunk ts to get PTS.
///
/// **Legacy-vs-Enhanced-RTMP discriminator.** `fourcc` is the
/// signal: `None` = legacy single-byte `codec_id` framing
/// (`avcC` / H.263 / VP6 / FlashSV); `Some([..])` = Enhanced RTMP
/// (Veovera 2023) where `codec_id` is reserved-zero on the wire,
/// `ex_packet_type` is the `PacketType` low nibble, and `body`
/// follows the per-FourCC shape laid out in
/// `enhanced-rtmp-v1.pdf` §"Defining Additional Video Codecs"
/// (HEVCDecoderConfigurationRecord / `AV1CodecConfigurationRecord`
/// / `VPCodecConfigurationRecord` for `SequenceStart`, NALUs / OBUs /
/// frames for `CodedFrames(X)`, AMF metadata for `Metadata`).
///
/// `composition_time` carries the SI24 CTS in both modes — it is
/// only emitted on the wire for legacy AVC (`codec_id == 7`),
/// and for the three NALU-based Enhanced-RTMP FourCCs paired
/// with PacketType = `CodedFrames`: `hvc1` (HEVC, v1), `avc1`
/// (AVC, v2), `vvc1` (VVC, v2). For `CodedFramesX` and the
/// non-NALU FourCCs (`av01`, `vp09`, `vp08`) the field is zero
/// and not encoded on the wire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoTag {
    pub frame_type: u8,
    pub codec_id: u8,
    /// `AvcSequenceHeader` / `AvcNalu` / `AvcEndOfSequence`. `None`
    /// for non-AVC codecs where the first AVC-specific byte doesn't
    /// exist. Stays `None` for Enhanced RTMP tags too — use
    /// [`VideoTag::ex_packet_type`] instead.
    pub avc_packet_type: Option<u8>,
    pub composition_time: i32,
    /// Body: `AVCDecoderConfigurationRecord` for AVC sequence
    /// headers; a sequence of `[u32 length-BE][NALU bytes]` pairs
    /// for AVC / HEVC NALU packets; AV1 OBUs for `av01`; full VP9
    /// frames for `vp09`; AMF-encoded `[name, value]` pairs for
    /// Enhanced RTMP `PacketTypeMetadata`.
    pub body: Vec<u8>,
    /// Enhanced RTMP v1 `PacketType` nibble (the four bits that
    /// replace `CodecID` when the `IsExHeader` flag is set). One
    /// of `EX_PACKET_TYPE_*`. `None` for legacy tags.
    pub ex_packet_type: Option<u8>,
    /// Enhanced RTMP FourCC video codec tag — the four ASCII
    /// bytes following the header byte when `IsExHeader == 1`.
    /// `None` for legacy tags. Values defined by Veovera so far:
    /// `b"av01"` (AV1, v1), `b"vp09"` (VP9, v1), `b"hvc1"`
    /// (HEVC, v1), `b"vp08"` (VP8, v2), `b"avc1"` (AVC/H.264 in
    /// FourCC mode, v2), `b"vvc1"` (VVC/H.266, v2).
    pub fourcc: Option<[u8; 4]>,
}

impl VideoTag {
    pub fn is_keyframe(&self) -> bool {
        self.frame_type == VIDEO_FRAME_KEYFRAME || self.frame_type == VIDEO_FRAME_GENERATED_KEY
    }
    pub fn is_avc_sequence_header(&self) -> bool {
        self.codec_id == VIDEO_CODEC_AVC
            && self.avc_packet_type == Some(AVC_PACKET_TYPE_SEQUENCE_HEADER)
    }
    /// True when this tag is the FourCC-mode `PacketTypeSequenceStart`
    /// for an Enhanced-RTMP codec (`body` is the codec's
    /// configuration record — `HEVCDecoderConfigurationRecord` for
    /// `hvc1`, `AV1CodecConfigurationRecord` for `av01`,
    /// `VPCodecConfigurationRecord` for `vp09` / `vp08`,
    /// `AVCDecoderConfigurationRecord` for `avc1`,
    /// `VVCDecoderConfigurationRecord` for `vvc1`).
    pub fn is_ex_sequence_header(&self) -> bool {
        self.fourcc.is_some() && self.ex_packet_type == Some(EX_PACKET_TYPE_SEQUENCE_START)
    }
    /// True when this tag carries an Enhanced-RTMP
    /// `PacketTypeMetadata` body (HDR `colorInfo` and the like).
    /// Per Enhanced RTMP v1 the `FrameType` flags above the
    /// PacketType nibble are required to be ignored when this is
    /// set, so callers that classify keyframe vs interframe must
    /// short-circuit on this predicate first.
    pub fn is_ex_metadata(&self) -> bool {
        self.fourcc.is_some() && self.ex_packet_type == Some(EX_PACKET_TYPE_METADATA)
    }
}

// 24-bit signed → i32 sign-extend. The wire format ("FLV
// Composition Time", FLV §E.4.3.1, also Enhanced RTMP HEVC
// CodedFrames row) packs SI24 in three big-endian bytes.
fn sign_extend_si24(raw: i32) -> i32 {
    if raw & 0x0080_0000 != 0 {
        raw | -0x0100_0000i32
    } else {
        raw
    }
}

/// Decode the FLV video-tag header from an RTMP video message payload.
///
/// Recognises both pre-2023 legacy framing (1-byte
/// `frame_type|codec_id` header, optional AVC packet-type +
/// SI24 CTS) and Enhanced RTMP v1 framing (`IsExHeader` flag in
/// bit 7 → 1-byte `is_ex|frame_type|packet_type` header, 4-byte
/// FourCC, optional SI24 CTS for HEVC `CodedFrames`).
///
/// Returns `Err(Error::Other)` on truncation. Per Enhanced RTMP
/// v1 the spec says: "During parsing, logic must gracefully
/// fail if at any point important signaling/flags (ex.
/// FrameType, IsExHeader, ExHeaderInfo) are not understood." —
/// we surface an unknown `ex_packet_type` by returning the raw
/// nibble in the struct (callers decide whether to ignore the
/// tag or fail).
pub fn parse_video(payload: &[u8]) -> Result<VideoTag> {
    if payload.is_empty() {
        return Err(Error::Other("FLV video tag: empty".into()));
    }
    let b0 = payload[0];
    if (b0 & VIDEO_IS_EX_HEADER) != 0 {
        // --- Enhanced RTMP v1 framing ---
        //
        //   byte 0      = IsExHeader(1) | FrameType(3) | PacketType(4)
        //   byte 1..=4  = FourCC (4 ASCII bytes)
        //   byte 5..    = body, with shape depending on FourCC × PacketType
        //
        // Per spec, when PacketType == Metadata the FrameType
        // flags above the nibble are required to be ignored;
        // we still preserve the raw bits in `frame_type` so
        // callers that diff fixtures can see them.
        let frame_type = (b0 >> 4) & 0b0111;
        let packet_type = b0 & 0x0F;
        if payload.len() < 5 {
            return Err(Error::Other(
                "Enhanced RTMP video tag: need 5+ bytes (IsExHeader + FourCC)".into(),
            ));
        }
        let mut fcc = [0u8; 4];
        fcc.copy_from_slice(&payload[1..5]);

        // SI24 CompositionTime is on the wire only for the
        // three NALU-based FourCCs paired with
        // PacketTypeCodedFrames (Enhanced RTMP v1 added HEVC;
        // Enhanced RTMP v2 §"ExVideoTagBody" adds AVC and VVC
        // with the same `compositionTimeOffset = SI24` row in
        // the pseudocode). For CodedFramesX the spec says:
        // "compositionTimeOffset is implied to equal zero. This
        // is an optimization to save putting SI24 value on the
        // wire." All other FourCCs (av01, vp09, vp08) and all
        // other PacketTypes have no CTS field — the body
        // follows the FourCC directly.
        let needs_cts = packet_type == EX_PACKET_TYPE_CODED_FRAMES
            && (fcc == FOURCC_HEVC || fcc == FOURCC_AVC || fcc == FOURCC_VVC);
        let (cts, body_start) = if needs_cts {
            if payload.len() < 8 {
                return Err(Error::Other(
                    "Enhanced RTMP / HEVC CodedFrames: need 8+ bytes for SI24 CTS".into(),
                ));
            }
            let raw =
                ((payload[5] as i32) << 16) | ((payload[6] as i32) << 8) | (payload[7] as i32);
            (sign_extend_si24(raw), 8)
        } else {
            (0, 5)
        };

        Ok(VideoTag {
            frame_type,
            codec_id: 0, // reserved in extended mode; legacy nibble unused.
            avc_packet_type: None,
            composition_time: cts,
            body: payload[body_start..].to_vec(),
            ex_packet_type: Some(packet_type),
            fourcc: Some(fcc),
        })
    } else {
        // --- Legacy pre-2023 framing ---
        let frame_type = b0 >> 4;
        let codec_id = b0 & 0x0F;
        if codec_id == VIDEO_CODEC_AVC {
            if payload.len() < 5 {
                return Err(Error::Other("FLV/AVC tag: need 5+ bytes".into()));
            }
            let apt = payload[1];
            let cts_raw =
                ((payload[2] as i32) << 16) | ((payload[3] as i32) << 8) | (payload[4] as i32);
            Ok(VideoTag {
                frame_type,
                codec_id,
                avc_packet_type: Some(apt),
                composition_time: sign_extend_si24(cts_raw),
                body: payload[5..].to_vec(),
                ex_packet_type: None,
                fourcc: None,
            })
        } else {
            Ok(VideoTag {
                frame_type,
                codec_id,
                avc_packet_type: None,
                composition_time: 0,
                body: payload[1..].to_vec(),
                ex_packet_type: None,
                fourcc: None,
            })
        }
    }
}

/// Build an RTMP video-tag payload.
///
/// Legacy mode (`tag.fourcc.is_none()`): writes the 1-byte
/// frame/codec header + optional AVC packet type + 3-byte
/// composition time, then `body`.
///
/// Enhanced RTMP mode (`tag.fourcc = Some([..])`): writes the
/// `IsExHeader | frame_type | packet_type` byte, the 4-byte
/// FourCC, the SI24 CTS *only* when FourCC == HEVC and
/// PacketType == CodedFrames (matching Enhanced RTMP v1's
/// "CompositionTime Offset is implied to equal zero" exception
/// for `CodedFramesX` and the non-HEVC FourCCs), then `body`.
pub fn build_video(tag: &VideoTag) -> Vec<u8> {
    if let Some(fcc) = tag.fourcc {
        let packet_type = tag.ex_packet_type.unwrap_or(EX_PACKET_TYPE_CODED_FRAMES);
        // Per Enhanced RTMP §"Defining Additional Video Codecs"
        // FrameType is UB[3] (i.e. lives in bits 4..=6 — bit 7
        // is IsExHeader). Mask to 3 bits before packing.
        let head = VIDEO_IS_EX_HEADER | ((tag.frame_type & 0x07) << 4) | (packet_type & 0x0F);
        let mut out = Vec::with_capacity(tag.body.len() + 8);
        out.push(head);
        out.extend_from_slice(&fcc);
        // Mirrors the parse-side `needs_cts` rule: HEVC / AVC /
        // VVC + CodedFrames emit the SI24 composition-time;
        // everything else (CodedFramesX, SequenceStart,
        // SequenceEnd, Metadata, and the non-NALU FourCCs)
        // omits it per Enhanced RTMP v1/v2 §"ExVideoTagBody".
        let cts_on_wire = packet_type == EX_PACKET_TYPE_CODED_FRAMES
            && (fcc == FOURCC_HEVC || fcc == FOURCC_AVC || fcc == FOURCC_VVC);
        if cts_on_wire {
            let cts = tag.composition_time & 0x00FF_FFFF;
            out.extend_from_slice(&[(cts >> 16) as u8, (cts >> 8) as u8, cts as u8]);
        }
        out.extend_from_slice(&tag.body);
        out
    } else {
        let head = (tag.frame_type << 4) | (tag.codec_id & 0x0F);
        let mut out = Vec::with_capacity(tag.body.len() + 5);
        out.push(head);
        if tag.codec_id == VIDEO_CODEC_AVC {
            out.push(tag.avc_packet_type.unwrap_or(AVC_PACKET_TYPE_NALU));
            let cts = tag.composition_time & 0x00FF_FFFF;
            out.extend_from_slice(&[(cts >> 16) as u8, (cts >> 8) as u8, cts as u8]);
        }
        out.extend_from_slice(&tag.body);
        out
    }
}

/// Decoded FLV audio-tag header + payload.
///
/// **Legacy-vs-Enhanced-RTMP discriminator.** `audio_fourcc` is
/// the signal: `None` = legacy pre-2023 single-byte framing
/// (`SoundFormat | SoundRate | SoundSize | SoundType`, optional
/// AAC packet-type marker); `Some([..])` = Enhanced RTMP v2
/// (Veovera 2026) where `sound_format` is reserved-9 (`ExHeader`)
/// on the wire, `ex_packet_type` is the `AudioPacketType` low
/// nibble, `audio_fourcc` is the four ASCII bytes that follow,
/// and `body` is the per-FourCC × per-PacketType payload defined
/// in `enhanced-rtmp-v2.pdf` §"Enhanced Audio" (the
/// `ExAudioTagBody` table).
///
/// The legacy bit-field fields `sound_rate`, `sound_size_16bit`
/// and `stereo` are not interpreted in Enhanced mode — the spec
/// says: "if (soundFormat == SoundFormat.ExHeader) we switch into
/// FOURCC audio mode as defined below. This means that soundRate,
/// soundSize and soundType bits are not interpreted, instead the
/// UB[4] bits are interpreted as an AudioPacketType". We zero
/// them on parse for tags that arrive in Enhanced mode so callers
/// don't accidentally read them as audio configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioTag {
    pub sound_format: u8,
    /// 0 = 5.5k / 1 = 11k / 2 = 22k / 3 = 44k. Encoded in the FLV
    /// header but overridden for AAC (always 3 by spec). Ignored
    /// and forced to zero in Enhanced mode (`audio_fourcc.is_some()`).
    pub sound_rate: u8,
    pub sound_size_16bit: bool,
    pub stereo: bool,
    /// `AacSequenceHeader` / `AacRaw`. `None` for non-AAC codecs
    /// and for all Enhanced-mode tags (use [`AudioTag::ex_packet_type`]
    /// instead).
    pub aac_packet_type: Option<u8>,
    /// Enhanced RTMP v2 `AudioPacketType` nibble (the four bits
    /// that replace SoundRate|SoundSize|SoundType when
    /// `sound_format == AUDIO_FORMAT_EX_HEADER`). One of
    /// `AUDIO_PACKET_TYPE_*`. `None` for legacy tags.
    pub ex_packet_type: Option<u8>,
    /// Enhanced RTMP v2 FourCC audio codec tag — the four ASCII
    /// bytes following the header byte when `sound_format ==
    /// AUDIO_FORMAT_EX_HEADER`. `None` for legacy tags. Values
    /// defined by Veovera so far: `b"Opus"`, `b"fLaC"`, `b"ac-3"`,
    /// `b"ec-3"`, `b".mp3"`, `b"mp4a"` (AAC, added FOURCC
    /// signalling).
    pub audio_fourcc: Option<[u8; 4]>,
    /// Body: per-FourCC `…SequenceHeader` for
    /// `PacketTypeSequenceStart` (`OpusSequenceHeader` /
    /// `FlacSequenceHeader` / `AacSequenceHeader`); per-FourCC
    /// `…CodedData` for `PacketTypeCodedFrames` (`Ac3CodedData`,
    /// `OpusCodedData`, `Mp3CodedData`, `AacCodedData`,
    /// `FlacCodedData`); empty for `SequenceEnd`.
    pub body: Vec<u8>,
}

impl AudioTag {
    /// True when this tag is an Enhanced-RTMP v2 tag (the
    /// SoundFormat nibble was `ExHeader = 9` on the wire and the
    /// four-byte FourCC + AudioPacketType were decoded into
    /// [`audio_fourcc`][AudioTag::audio_fourcc] /
    /// [`ex_packet_type`][AudioTag::ex_packet_type]).
    pub fn is_enhanced(&self) -> bool {
        self.audio_fourcc.is_some()
    }
    /// True when this tag is a legacy AAC sequence-header
    /// (`AudioSpecificConfig` payload) — `sound_format = 10`,
    /// `aac_packet_type = 0`.
    pub fn is_aac_sequence_header(&self) -> bool {
        self.sound_format == AUDIO_FORMAT_AAC
            && self.aac_packet_type == Some(AAC_PACKET_TYPE_SEQUENCE_HEADER)
    }
    /// True when this tag is the Enhanced-RTMP v2
    /// `PacketTypeSequenceStart` for a FourCC audio codec — body
    /// is the codec's sequence header per `ExAudioTagBody`
    /// (`OpusSequenceHeader` / `FlacSequenceHeader` /
    /// `AacSequenceHeader` ASC; AC-3 / E-AC-3 / MP3 have no
    /// SequenceStart shape defined in v2).
    pub fn is_ex_sequence_header(&self) -> bool {
        self.audio_fourcc.is_some() && self.ex_packet_type == Some(AUDIO_PACKET_TYPE_SEQUENCE_START)
    }
}

/// Decode the FLV audio-tag header from an RTMP audio message
/// payload.
///
/// Recognises both legacy pre-2023 framing (1-byte
/// `SoundFormat|SoundRate|SoundSize|SoundType` header, optional
/// AAC packet-type marker) and Enhanced RTMP v2 framing
/// (`SoundFormat == ExHeader = 9` → 1-byte
/// `ExHeader|AudioPacketType` header, 4-byte FourCC, per-FourCC
/// body).
///
/// Returns `Err(Error::Other)` on truncation. Per Enhanced RTMP
/// v2: "During the parsing process, the logic MUST handle
/// unexpected or unknown elements gracefully. Specifically, if
/// any critical signaling or flags (e.g., AudioPacketType and
/// AudioFourCc) are not recognized, the system MUST fail in a
/// controlled and predictable manner." We surface an unknown
/// `ex_packet_type` / FourCC by returning the raw bytes in the
/// struct (callers decide whether to ignore the tag or fail).
///
/// NOTE: the `Multitrack` and `ModEx` AudioPacketTypes have
/// nested layouts (per-track FourCC + size-prefixed track chunks,
/// or a chain of `modExDataSize + modExData + modExType`
/// preludes). They are deferred to a follow-up round; if the
/// nibble decodes to either value, the body is preserved verbatim
/// and the caller is expected to skip the message rather than
/// interpret it as a normal coded-frame tag.
pub fn parse_audio(payload: &[u8]) -> Result<AudioTag> {
    if payload.is_empty() {
        return Err(Error::Other("FLV audio tag: empty".into()));
    }
    let b0 = payload[0];
    let sound_format = b0 >> 4;
    if sound_format == AUDIO_FORMAT_EX_HEADER {
        // --- Enhanced RTMP v2 framing ---
        //
        //   byte 0     = SoundFormat=9(4) | AudioPacketType(4)
        //   byte 1..=4 = AudioFourCc (4 ASCII bytes)
        //   byte 5..   = body, per (FourCc, PacketType) per
        //                §"ExAudioTagBody"
        //
        // Per spec the legacy bit-field SoundRate/SoundSize/
        // SoundType are NOT interpreted in this mode — zero them
        // on the parsed struct so a downstream consumer that
        // (incorrectly) keys off them gets a clearly-zero answer
        // instead of an arbitrary alias of the AudioPacketType
        // nibble.
        let packet_type = b0 & 0x0F;
        if payload.len() < 5 {
            return Err(Error::Other(
                "Enhanced RTMP audio tag: need 5+ bytes (ExHeader + FourCC)".into(),
            ));
        }
        let mut fcc = [0u8; 4];
        fcc.copy_from_slice(&payload[1..5]);
        Ok(AudioTag {
            sound_format,
            sound_rate: 0,
            sound_size_16bit: false,
            stereo: false,
            aac_packet_type: None,
            ex_packet_type: Some(packet_type),
            audio_fourcc: Some(fcc),
            body: payload[5..].to_vec(),
        })
    } else {
        // --- Legacy pre-2023 framing ---
        let sound_rate = (b0 >> 2) & 0x03;
        let sound_size_16bit = (b0 & 0x02) != 0;
        let stereo = (b0 & 0x01) != 0;
        if sound_format == AUDIO_FORMAT_AAC {
            if payload.len() < 2 {
                return Err(Error::Other("FLV/AAC tag: need 2+ bytes".into()));
            }
            Ok(AudioTag {
                sound_format,
                sound_rate,
                sound_size_16bit,
                stereo,
                aac_packet_type: Some(payload[1]),
                ex_packet_type: None,
                audio_fourcc: None,
                body: payload[2..].to_vec(),
            })
        } else {
            Ok(AudioTag {
                sound_format,
                sound_rate,
                sound_size_16bit,
                stereo,
                aac_packet_type: None,
                ex_packet_type: None,
                audio_fourcc: None,
                body: payload[1..].to_vec(),
            })
        }
    }
}

/// Build an RTMP audio-tag payload.
///
/// Legacy mode (`tag.audio_fourcc.is_none()`): writes the 1-byte
/// `SoundFormat|SoundRate|SoundSize|SoundType` header + optional
/// 1-byte AAC packet type, then `body`.
///
/// Enhanced RTMP v2 mode (`tag.audio_fourcc = Some([..])`):
/// writes a 1-byte `ExHeader(9) | AudioPacketType` header
/// (regardless of the value sitting in `tag.sound_format` — the
/// spec mandates SoundFormat == 9 for this layout), the 4-byte
/// FourCC, then `body`. The legacy SoundRate / SoundSize /
/// SoundType bits are dropped per spec.
pub fn build_audio(tag: &AudioTag) -> Vec<u8> {
    if let Some(fcc) = tag.audio_fourcc {
        let packet_type = tag.ex_packet_type.unwrap_or(AUDIO_PACKET_TYPE_CODED_FRAMES);
        let head = (AUDIO_FORMAT_EX_HEADER << 4) | (packet_type & 0x0F);
        let mut out = Vec::with_capacity(tag.body.len() + 5);
        out.push(head);
        out.extend_from_slice(&fcc);
        out.extend_from_slice(&tag.body);
        out
    } else {
        let b0 = (tag.sound_format << 4)
            | ((tag.sound_rate & 0x03) << 2)
            | (if tag.sound_size_16bit { 0x02 } else { 0 })
            | (if tag.stereo { 0x01 } else { 0 });
        let mut out = Vec::with_capacity(tag.body.len() + 2);
        out.push(b0);
        if tag.sound_format == AUDIO_FORMAT_AAC {
            out.push(tag.aac_packet_type.unwrap_or(AAC_PACKET_TYPE_RAW));
        }
        out.extend_from_slice(&tag.body);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn video_tag_avc_nalu_roundtrip() {
        let tag = VideoTag {
            frame_type: VIDEO_FRAME_KEYFRAME,
            codec_id: VIDEO_CODEC_AVC,
            avc_packet_type: Some(AVC_PACKET_TYPE_NALU),
            composition_time: 42,
            body: b"\x00\x00\x00\x05hello".to_vec(),
            ex_packet_type: None,
            fourcc: None,
        };
        let payload = build_video(&tag);
        assert_eq!(payload[0], 0x17); // keyframe + AVC
        let back = parse_video(&payload).unwrap();
        assert_eq!(back, tag);
    }

    #[test]
    fn video_tag_negative_cts_sign_extends() {
        let tag = VideoTag {
            frame_type: VIDEO_FRAME_INTER,
            codec_id: VIDEO_CODEC_AVC,
            avc_packet_type: Some(AVC_PACKET_TYPE_NALU),
            composition_time: -5,
            body: vec![0x01],
            ex_packet_type: None,
            fourcc: None,
        };
        let payload = build_video(&tag);
        let back = parse_video(&payload).unwrap();
        assert_eq!(back.composition_time, -5);
    }

    // ------- Enhanced RTMP v1 (Veovera 2023) round-trips -------

    #[test]
    fn ex_video_tag_hevc_sequence_start_roundtrip() {
        // SequenceStart: HEVCDecoderConfigurationRecord in body,
        // no SI24 CTS on the wire.
        let tag = VideoTag {
            frame_type: VIDEO_FRAME_KEYFRAME,
            codec_id: 0,
            avc_packet_type: None,
            composition_time: 0,
            body: b"\x01dummy-hvcc".to_vec(),
            ex_packet_type: Some(EX_PACKET_TYPE_SEQUENCE_START),
            fourcc: Some(FOURCC_HEVC),
        };
        let payload = build_video(&tag);
        // Header byte: IsExHeader(1) | FrameType(001) | PacketType(0000)
        // = 0b1001_0000 = 0x90.
        assert_eq!(payload[0], 0x90);
        assert_eq!(&payload[1..5], b"hvc1");
        // No SI24 between FourCC and body for SequenceStart.
        assert_eq!(&payload[5..], b"\x01dummy-hvcc");

        let back = parse_video(&payload).unwrap();
        assert_eq!(back, tag);
        assert!(back.is_ex_sequence_header());
        assert!(back.is_keyframe());
    }

    #[test]
    fn ex_video_tag_hevc_coded_frames_carries_cts() {
        // CodedFrames is the only Enhanced RTMP shape that
        // keeps the SI24 CTS on the wire (per Table 4's HEVC
        // pseudocode).
        let tag = VideoTag {
            frame_type: VIDEO_FRAME_INTER,
            codec_id: 0,
            avc_packet_type: None,
            composition_time: -33,
            body: b"\x00\x00\x00\x04NALU".to_vec(),
            ex_packet_type: Some(EX_PACKET_TYPE_CODED_FRAMES),
            fourcc: Some(FOURCC_HEVC),
        };
        let payload = build_video(&tag);
        // IsExHeader=1 | FrameType=2 | PacketType=1 = 0b1010_0001 = 0xA1.
        assert_eq!(payload[0], 0xA1);
        assert_eq!(&payload[1..5], b"hvc1");
        // SI24(-33) two's complement = 0xFFFFDF; truncated to
        // 24 bits = 0xFFFFDF — three bytes 0xFF 0xFF 0xDF.
        assert_eq!(&payload[5..8], &[0xFF, 0xFF, 0xDF]);
        assert_eq!(&payload[8..], b"\x00\x00\x00\x04NALU");

        let back = parse_video(&payload).unwrap();
        assert_eq!(back, tag);
        assert_eq!(back.composition_time, -33);
    }

    #[test]
    fn ex_video_tag_hevc_coded_frames_x_omits_cts() {
        // CodedFramesX is the SI24=0 optimisation — three
        // bytes off the wire vs CodedFrames.
        let tag = VideoTag {
            frame_type: VIDEO_FRAME_INTER,
            codec_id: 0,
            avc_packet_type: None,
            composition_time: 0,
            body: b"\x00\x00\x00\x04NALU".to_vec(),
            ex_packet_type: Some(EX_PACKET_TYPE_CODED_FRAMES_X),
            fourcc: Some(FOURCC_HEVC),
        };
        let payload = build_video(&tag);
        // IsExHeader=1 | FrameType=2 | PacketType=3 = 0xA3.
        assert_eq!(payload[0], 0xA3);
        assert_eq!(&payload[1..5], b"hvc1");
        // Body follows the FourCC directly — no SI24 bytes.
        assert_eq!(&payload[5..], b"\x00\x00\x00\x04NALU");
        // Total length saved is exactly 3 bytes vs the
        // CodedFrames form (1-byte header + 4-byte FourCC +
        // 8-byte body, no SI24).
        assert_eq!(payload.len(), 1 + 4 + 8);

        let back = parse_video(&payload).unwrap();
        assert_eq!(back, tag);
    }

    #[test]
    fn ex_video_tag_av1_sequence_start_no_cts() {
        // AV1 SequenceStart body is the
        // AV1CodecConfigurationRecord (per spec). No CTS.
        let tag = VideoTag {
            frame_type: VIDEO_FRAME_KEYFRAME,
            codec_id: 0,
            avc_packet_type: None,
            composition_time: 0,
            body: b"\x81\x05\x0c\x00".to_vec(),
            ex_packet_type: Some(EX_PACKET_TYPE_SEQUENCE_START),
            fourcc: Some(FOURCC_AV1),
        };
        let payload = build_video(&tag);
        assert_eq!(payload[0], 0x90);
        assert_eq!(&payload[1..5], b"av01");
        assert_eq!(&payload[5..], b"\x81\x05\x0c\x00");

        let back = parse_video(&payload).unwrap();
        assert_eq!(back, tag);
        assert!(back.is_ex_sequence_header());
    }

    #[test]
    fn ex_video_tag_av1_coded_frames_obus() {
        // AV1 CodedFrames body is "one or more OBUs which MUST
        // represent a single temporal unit" (Enhanced RTMP v1
        // §"If FourCC == AV1"). Still no CTS — only HEVC keeps
        // composition-time on the wire.
        let tag = VideoTag {
            frame_type: VIDEO_FRAME_KEYFRAME,
            codec_id: 0,
            avc_packet_type: None,
            composition_time: 0,
            body: b"\x0a\x0b\x0cobu-stub".to_vec(),
            ex_packet_type: Some(EX_PACKET_TYPE_CODED_FRAMES),
            fourcc: Some(FOURCC_AV1),
        };
        let payload = build_video(&tag);
        // IsExHeader=1 | FrameType=1 | PacketType=1 = 0x91.
        assert_eq!(payload[0], 0x91);
        assert_eq!(&payload[1..5], b"av01");
        // Body immediately follows FourCC (no SI24 for AV1).
        assert_eq!(&payload[5..], b"\x0a\x0b\x0cobu-stub");

        let back = parse_video(&payload).unwrap();
        assert_eq!(back, tag);
    }

    #[test]
    fn ex_video_tag_vp9_coded_frames_full_frame() {
        // VP9 CodedFrames body "MUST contain full frames"
        // (Enhanced RTMP v1 §"If FourCC == VP9").
        let tag = VideoTag {
            frame_type: VIDEO_FRAME_KEYFRAME,
            codec_id: 0,
            avc_packet_type: None,
            composition_time: 0,
            body: b"vp9-frame-bytes".to_vec(),
            ex_packet_type: Some(EX_PACKET_TYPE_CODED_FRAMES),
            fourcc: Some(FOURCC_VP9),
        };
        let payload = build_video(&tag);
        assert_eq!(payload[0], 0x91);
        assert_eq!(&payload[1..5], b"vp09");
        assert_eq!(&payload[5..], b"vp9-frame-bytes");

        let back = parse_video(&payload).unwrap();
        assert_eq!(back, tag);
    }

    #[test]
    fn ex_video_tag_sequence_end_empty_body() {
        // SequenceEnd carries no codec data — body is empty.
        let tag = VideoTag {
            frame_type: VIDEO_FRAME_KEYFRAME,
            codec_id: 0,
            avc_packet_type: None,
            composition_time: 0,
            body: vec![],
            ex_packet_type: Some(EX_PACKET_TYPE_SEQUENCE_END),
            fourcc: Some(FOURCC_HEVC),
        };
        let payload = build_video(&tag);
        // IsExHeader=1 | FrameType=1 | PacketType=2 = 0x92.
        assert_eq!(payload[0], 0x92);
        assert_eq!(&payload[1..5], b"hvc1");
        assert_eq!(payload.len(), 5);

        let back = parse_video(&payload).unwrap();
        assert_eq!(back, tag);
    }

    #[test]
    fn ex_video_tag_metadata_carries_amf_body() {
        // PacketTypeMetadata: body is an AMF-encoded `[name,
        // value]` pair (only `"colorInfo"` is defined in v1).
        // Spec says: "presence of PacketTypeMetadata means
        // that FrameType flags at the top of this table should
        // be ignored." We still preserve the bits — caller
        // policy decides.
        let tag = VideoTag {
            frame_type: VIDEO_FRAME_INFO, // would be "ignored" per spec
            codec_id: 0,
            avc_packet_type: None,
            composition_time: 0,
            body: b"amf-stub".to_vec(),
            ex_packet_type: Some(EX_PACKET_TYPE_METADATA),
            fourcc: Some(FOURCC_HEVC),
        };
        let payload = build_video(&tag);
        // IsExHeader=1 | FrameType=5 | PacketType=4 = 0xD4.
        assert_eq!(payload[0], 0xD4);
        let back = parse_video(&payload).unwrap();
        assert_eq!(back, tag);
        assert!(back.is_ex_metadata());
    }

    #[test]
    fn legacy_avc_high_frame_type_bit_was_always_zero() {
        // Sanity-check the Enhanced RTMP backwards-compat
        // claim: pre-2023 FrameType values 1..=5 all leave bit
        // 7 of the header byte clear, so a parser that branches
        // on IsExHeader == 1 never mis-detects legacy traffic
        // as Enhanced.
        for ft in [
            VIDEO_FRAME_KEYFRAME,
            VIDEO_FRAME_INTER,
            VIDEO_FRAME_DISPOSABLE,
            VIDEO_FRAME_GENERATED_KEY,
            VIDEO_FRAME_INFO,
        ] {
            let tag = VideoTag {
                frame_type: ft,
                codec_id: VIDEO_CODEC_AVC,
                avc_packet_type: Some(AVC_PACKET_TYPE_NALU),
                composition_time: 0,
                body: vec![0x00],
                ex_packet_type: None,
                fourcc: None,
            };
            let payload = build_video(&tag);
            assert_eq!(payload[0] & VIDEO_IS_EX_HEADER, 0, "ft={ft}");
        }
    }

    // ------- Enhanced RTMP v2 (Veovera 2026) new video FourCCs -------

    #[test]
    fn ex_video_tag_vp8_sequence_start_carries_vp_config_record() {
        // VP8 SequenceStart body is a `VPCodecConfigurationRecord`
        // (same shape as VP9 — per enhanced-rtmp-v2.pdf §"Enhanced
        // Video" the pseudocode is `vp8Header =
        // [VPCodecConfigurationRecord]`). No CTS — VP8 has no
        // B-frames.
        let tag = VideoTag {
            frame_type: VIDEO_FRAME_KEYFRAME,
            codec_id: 0,
            avc_packet_type: None,
            composition_time: 0,
            body: vec![
                0x01, 0x00, 0x08, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            ],
            ex_packet_type: Some(EX_PACKET_TYPE_SEQUENCE_START),
            fourcc: Some(FOURCC_VP8),
        };
        let payload = build_video(&tag);
        // IsExHeader=1 | FrameType=1 (key) | PacketType=0 = 0x90.
        assert_eq!(payload[0], 0x90);
        assert_eq!(&payload[1..5], b"vp08");
        assert_eq!(&payload[5..], &tag.body[..]);

        let back = parse_video(&payload).unwrap();
        assert_eq!(back, tag);
        assert!(back.is_ex_sequence_header());
    }

    #[test]
    fn ex_video_tag_vp8_coded_frames_no_cts() {
        // VP8 CodedFrames body is one or more full frames; no CTS
        // on the wire (no B-frame ordering).
        let tag = VideoTag {
            frame_type: VIDEO_FRAME_INTER,
            codec_id: 0,
            avc_packet_type: None,
            composition_time: 0,
            body: b"vp8-frame-bytes".to_vec(),
            ex_packet_type: Some(EX_PACKET_TYPE_CODED_FRAMES),
            fourcc: Some(FOURCC_VP8),
        };
        let payload = build_video(&tag);
        // IsExHeader=1 | FrameType=2 | PacketType=1 = 0xA1.
        assert_eq!(payload[0], 0xA1);
        assert_eq!(&payload[1..5], b"vp08");
        // Body immediately follows FourCC — no SI24 phantom.
        assert_eq!(&payload[5..], b"vp8-frame-bytes");
        let back = parse_video(&payload).unwrap();
        assert_eq!(back, tag);
    }

    #[test]
    fn ex_video_tag_avc_fourcc_sequence_start_carries_avcc() {
        // FourCC-mode AVC SequenceStart body is the
        // `AVCDecoderConfigurationRecord` (per ISO/IEC 14496-15
        // §5.3.4.1, cited verbatim by enhanced-rtmp-v2.pdf
        // §"Enhanced Video"). No CTS on SequenceStart for any
        // FourCC, AVC included.
        let tag = VideoTag {
            frame_type: VIDEO_FRAME_KEYFRAME,
            codec_id: 0,
            avc_packet_type: None,
            composition_time: 0,
            body: b"\x01\x42\xc0\x1edummy-avcc".to_vec(),
            ex_packet_type: Some(EX_PACKET_TYPE_SEQUENCE_START),
            fourcc: Some(FOURCC_AVC),
        };
        let payload = build_video(&tag);
        // IsExHeader=1 | FrameType=1 | PacketType=0 = 0x90.
        assert_eq!(payload[0], 0x90);
        assert_eq!(&payload[1..5], b"avc1");
        // No SI24 — body follows FourCC directly.
        assert_eq!(&payload[5..], b"\x01\x42\xc0\x1edummy-avcc");
        let back = parse_video(&payload).unwrap();
        assert_eq!(back, tag);
        assert!(back.is_ex_sequence_header());
    }

    #[test]
    fn ex_video_tag_avc_fourcc_coded_frames_carries_si24_cts() {
        // FourCC-mode AVC CodedFrames carries SI24
        // `compositionTimeOffset` exactly like HEVC. Tested with a
        // negative offset (-100) to also exercise the sign-extend
        // path through both build and parse.
        let tag = VideoTag {
            frame_type: VIDEO_FRAME_INTER,
            codec_id: 0,
            avc_packet_type: None,
            composition_time: -100,
            body: b"\x00\x00\x00\x05nalu1".to_vec(),
            ex_packet_type: Some(EX_PACKET_TYPE_CODED_FRAMES),
            fourcc: Some(FOURCC_AVC),
        };
        let payload = build_video(&tag);
        // IsExHeader=1 | FrameType=2 | PacketType=1 = 0xA1.
        assert_eq!(payload[0], 0xA1);
        assert_eq!(&payload[1..5], b"avc1");
        // SI24(-100) = 0xFFFF9C two's complement.
        assert_eq!(&payload[5..8], &[0xFF, 0xFF, 0x9C]);
        assert_eq!(&payload[8..], b"\x00\x00\x00\x05nalu1");
        let back = parse_video(&payload).unwrap();
        assert_eq!(back, tag);
        assert_eq!(back.composition_time, -100);
    }

    #[test]
    fn ex_video_tag_avc_fourcc_coded_frames_x_omits_cts() {
        // CodedFramesX optimisation — same as HEVC: no SI24 on the
        // wire, three bytes saved.
        let tag = VideoTag {
            frame_type: VIDEO_FRAME_INTER,
            codec_id: 0,
            avc_packet_type: None,
            composition_time: 0,
            body: b"\x00\x00\x00\x05nalu2".to_vec(),
            ex_packet_type: Some(EX_PACKET_TYPE_CODED_FRAMES_X),
            fourcc: Some(FOURCC_AVC),
        };
        let payload = build_video(&tag);
        // IsExHeader=1 | FrameType=2 | PacketType=3 = 0xA3.
        assert_eq!(payload[0], 0xA3);
        assert_eq!(&payload[1..5], b"avc1");
        // Body follows immediately — no SI24.
        assert_eq!(&payload[5..], b"\x00\x00\x00\x05nalu2");
        assert_eq!(payload.len(), 1 + 4 + 9);
        let back = parse_video(&payload).unwrap();
        assert_eq!(back, tag);
    }

    #[test]
    fn ex_video_tag_vvc_sequence_start_carries_vvcc() {
        // VVC SequenceStart body is `VVCDecoderConfigurationRecord`
        // (per ISO/IEC 14496-15:2024 §11.2.4.2). No CTS on
        // SequenceStart.
        let tag = VideoTag {
            frame_type: VIDEO_FRAME_KEYFRAME,
            codec_id: 0,
            avc_packet_type: None,
            composition_time: 0,
            body: b"\xff\xfcdummy-vvcc".to_vec(),
            ex_packet_type: Some(EX_PACKET_TYPE_SEQUENCE_START),
            fourcc: Some(FOURCC_VVC),
        };
        let payload = build_video(&tag);
        assert_eq!(payload[0], 0x90);
        assert_eq!(&payload[1..5], b"vvc1");
        assert_eq!(&payload[5..], b"\xff\xfcdummy-vvcc");
        let back = parse_video(&payload).unwrap();
        assert_eq!(back, tag);
        assert!(back.is_ex_sequence_header());
    }

    #[test]
    fn ex_video_tag_vvc_coded_frames_carries_si24_cts() {
        // VVC CodedFrames carries SI24 like HEVC and AVC — covers
        // the §"ExVideoTagBody" pseudocode `if (videoFourCc ==
        // VideoFourCc.Vvc) { compositionTimeOffset = SI24 }`.
        let tag = VideoTag {
            frame_type: VIDEO_FRAME_KEYFRAME,
            codec_id: 0,
            avc_packet_type: None,
            composition_time: 17,
            body: b"\x00\x00\x00\x06h266ku".to_vec(),
            ex_packet_type: Some(EX_PACKET_TYPE_CODED_FRAMES),
            fourcc: Some(FOURCC_VVC),
        };
        let payload = build_video(&tag);
        // IsExHeader=1 | FrameType=1 | PacketType=1 = 0x91.
        assert_eq!(payload[0], 0x91);
        assert_eq!(&payload[1..5], b"vvc1");
        // SI24(17) = 0x000011.
        assert_eq!(&payload[5..8], &[0x00, 0x00, 0x11]);
        assert_eq!(&payload[8..], b"\x00\x00\x00\x06h266ku");
        let back = parse_video(&payload).unwrap();
        assert_eq!(back, tag);
        assert_eq!(back.composition_time, 17);
    }

    #[test]
    fn ex_video_tag_vvc_coded_frames_x_omits_cts() {
        let tag = VideoTag {
            frame_type: VIDEO_FRAME_INTER,
            codec_id: 0,
            avc_packet_type: None,
            composition_time: 0,
            body: b"\x00\x00\x00\x03vvc".to_vec(),
            ex_packet_type: Some(EX_PACKET_TYPE_CODED_FRAMES_X),
            fourcc: Some(FOURCC_VVC),
        };
        let payload = build_video(&tag);
        // IsExHeader=1 | FrameType=2 | PacketType=3 = 0xA3.
        assert_eq!(payload[0], 0xA3);
        assert_eq!(&payload[1..5], b"vvc1");
        assert_eq!(&payload[5..], b"\x00\x00\x00\x03vvc");
        let back = parse_video(&payload).unwrap();
        assert_eq!(back, tag);
    }

    #[test]
    fn ex_video_tag_avc_fourcc_coded_frames_truncated_si24_errors() {
        // §"ExVideoTagBody" guarantees the SI24 follows the
        // FourCC for AVC + CodedFrames. A wire stream missing
        // those three bytes must fail in a controlled manner
        // per "the system MUST fail in a controlled and
        // predictable manner".
        let truncated = [
            0xA1, // IsExHeader=1 | FrameType=2 | PacketType=1
            b'a', b'v', b'c', b'1', // FourCC
            0xFF, 0xFF, // only two of three SI24 bytes
        ];
        assert!(parse_video(&truncated).is_err());
    }

    #[test]
    fn ex_video_tag_v2_fourccs_are_distinct_from_v1_set() {
        // Wire-byte distinctness check: each v2 FourCC must
        // round-trip independently of the v1 set so a multiplexer
        // can't accidentally alias one to another.
        for &fcc in &[FOURCC_VP8, FOURCC_AVC, FOURCC_VVC] {
            let tag = VideoTag {
                frame_type: VIDEO_FRAME_KEYFRAME,
                codec_id: 0,
                avc_packet_type: None,
                composition_time: 0,
                body: vec![0xDE, 0xAD, 0xBE, 0xEF],
                ex_packet_type: Some(EX_PACKET_TYPE_SEQUENCE_END),
                fourcc: Some(fcc),
            };
            let payload = build_video(&tag);
            // SequenceEnd: ExHeader byte + FourCC, no body
            // expected, but we ship a stub for the round-trip
            // check.
            assert_eq!(&payload[1..5], &fcc[..]);
            let back = parse_video(&payload).unwrap();
            assert_eq!(back, tag);
            assert!(!matches!(fcc, FOURCC_AV1 | FOURCC_VP9 | FOURCC_HEVC));
        }
    }

    #[test]
    fn audio_tag_aac_sequence_header_roundtrip() {
        let tag = AudioTag {
            sound_format: AUDIO_FORMAT_AAC,
            sound_rate: 3,
            sound_size_16bit: true,
            stereo: true,
            aac_packet_type: Some(AAC_PACKET_TYPE_SEQUENCE_HEADER),
            body: vec![0x12, 0x10], // LC-AAC 44.1k stereo AudioSpecificConfig
            ex_packet_type: None,
            audio_fourcc: None,
        };
        let payload = build_audio(&tag);
        assert_eq!(payload[0], 0xAF); // AAC + rate 3 + 16-bit + stereo
        assert_eq!(payload[1], 0); // seq header
        let back = parse_audio(&payload).unwrap();
        assert_eq!(back, tag);
        assert!(back.is_aac_sequence_header());
        assert!(!back.is_enhanced());
    }

    // ------- Enhanced RTMP v2 (Veovera 2026) round-trips -------

    #[test]
    fn ex_audio_tag_opus_sequence_start_roundtrip() {
        // SequenceStart for Opus: body is the Opus ID header (a
        // valid one starts with the 8-byte "OpusHead" magic per
        // RFC 7845 §5.1; we use a tiny stub here since the
        // framing layer doesn't validate codec-payload internals).
        let tag = AudioTag {
            sound_format: AUDIO_FORMAT_EX_HEADER,
            sound_rate: 0,
            sound_size_16bit: false,
            stereo: false,
            aac_packet_type: None,
            ex_packet_type: Some(AUDIO_PACKET_TYPE_SEQUENCE_START),
            audio_fourcc: Some(FOURCC_OPUS),
            body: b"OpusHead\x01\x02".to_vec(),
        };
        let payload = build_audio(&tag);
        // Header byte: ExHeader(9) << 4 | PacketType(0) = 0x90.
        assert_eq!(payload[0], 0x90);
        assert_eq!(&payload[1..5], b"Opus");
        assert_eq!(&payload[5..], b"OpusHead\x01\x02");

        let back = parse_audio(&payload).unwrap();
        assert_eq!(back, tag);
        assert!(back.is_ex_sequence_header());
        assert!(back.is_enhanced());
        // Legacy bit-field is suppressed in Enhanced mode.
        assert_eq!(back.sound_rate, 0);
        assert!(!back.sound_size_16bit);
        assert!(!back.stereo);
    }

    #[test]
    fn ex_audio_tag_opus_coded_frames_carries_self_delimited_packets() {
        // Enhanced RTMP v2: "Body contains Opus packets [...] The
        // first (N - 1) Opus packets, if any, are packed one after
        // another using the self-delimiting framing from Appendix
        // B of [RFC6716]. The remaining Opus packet is packed at
        // the end of the Ogg packet using the regular,
        // undelimited framing from Section 3 of [RFC6716]." The
        // framing layer treats the body as opaque bytes.
        let tag = AudioTag {
            sound_format: AUDIO_FORMAT_EX_HEADER,
            sound_rate: 0,
            sound_size_16bit: false,
            stereo: false,
            aac_packet_type: None,
            ex_packet_type: Some(AUDIO_PACKET_TYPE_CODED_FRAMES),
            audio_fourcc: Some(FOURCC_OPUS),
            body: b"opus-frame-bytes".to_vec(),
        };
        let payload = build_audio(&tag);
        // ExHeader=9 | CodedFrames=1 = 0x91.
        assert_eq!(payload[0], 0x91);
        assert_eq!(&payload[1..5], b"Opus");
        assert_eq!(&payload[5..], b"opus-frame-bytes");

        let back = parse_audio(&payload).unwrap();
        assert_eq!(back, tag);
    }

    #[test]
    fn ex_audio_tag_flac_sequence_start_roundtrip() {
        // FLAC SequenceStart body: "The bytes 0x66 0x4C 0x61 0x43
        // ('fLaC' in ASCII) signature // Followed by a metadata
        // block (called the STREAMINFO block) as described in
        // section 7 of the FLAC specification." The framing layer
        // treats this as opaque.
        let tag = AudioTag {
            sound_format: AUDIO_FORMAT_EX_HEADER,
            sound_rate: 0,
            sound_size_16bit: false,
            stereo: false,
            aac_packet_type: None,
            ex_packet_type: Some(AUDIO_PACKET_TYPE_SEQUENCE_START),
            audio_fourcc: Some(FOURCC_FLAC),
            body: b"fLaC\x80\x00\x00\x22streaminfo".to_vec(),
        };
        let payload = build_audio(&tag);
        assert_eq!(payload[0], 0x90);
        assert_eq!(&payload[1..5], b"fLaC");
        assert_eq!(&payload[5..], b"fLaC\x80\x00\x00\x22streaminfo");

        let back = parse_audio(&payload).unwrap();
        assert_eq!(back, tag);
        assert!(back.is_ex_sequence_header());
    }

    #[test]
    fn ex_audio_tag_ac3_coded_frames_roundtrip() {
        // AC-3: "Body contains audio data as defined by the
        // bitstream syntax in the ATSC standard for Digital Audio
        // Compression (AC-3, E-AC-3)." No SequenceStart shape is
        // defined for AC-3 in v2 — only CodedFrames carries data.
        let tag = AudioTag {
            sound_format: AUDIO_FORMAT_EX_HEADER,
            sound_rate: 0,
            sound_size_16bit: false,
            stereo: false,
            aac_packet_type: None,
            ex_packet_type: Some(AUDIO_PACKET_TYPE_CODED_FRAMES),
            audio_fourcc: Some(FOURCC_AC3),
            body: vec![0x0B, 0x77, 0x12, 0x34, 0x56, 0x78], // AC-3 sync + stub
        };
        let payload = build_audio(&tag);
        assert_eq!(payload[0], 0x91);
        assert_eq!(&payload[1..5], b"ac-3");
        assert_eq!(&payload[5..], &[0x0B, 0x77, 0x12, 0x34, 0x56, 0x78]);

        let back = parse_audio(&payload).unwrap();
        assert_eq!(back, tag);
    }

    #[test]
    fn ex_audio_tag_eac3_coded_frames_roundtrip() {
        let tag = AudioTag {
            sound_format: AUDIO_FORMAT_EX_HEADER,
            sound_rate: 0,
            sound_size_16bit: false,
            stereo: false,
            aac_packet_type: None,
            ex_packet_type: Some(AUDIO_PACKET_TYPE_CODED_FRAMES),
            audio_fourcc: Some(FOURCC_EAC3),
            body: vec![0x0B, 0x77, 0xAB, 0xCD],
        };
        let payload = build_audio(&tag);
        assert_eq!(payload[0], 0x91);
        assert_eq!(&payload[1..5], b"ec-3");
        let back = parse_audio(&payload).unwrap();
        assert_eq!(back, tag);
    }

    #[test]
    fn ex_audio_tag_mp3_coded_frames_roundtrip() {
        // MP3 (added FOURCC signalling): "An Mp3 audio stream is
        // built up from a succession of smaller parts called
        // frames. Each frame is a data block with its own header
        // and audio information."
        let tag = AudioTag {
            sound_format: AUDIO_FORMAT_EX_HEADER,
            sound_rate: 0,
            sound_size_16bit: false,
            stereo: false,
            aac_packet_type: None,
            ex_packet_type: Some(AUDIO_PACKET_TYPE_CODED_FRAMES),
            audio_fourcc: Some(FOURCC_MP3),
            body: vec![0xFF, 0xFB, 0x90, 0x00], // MP3 sync header stub
        };
        let payload = build_audio(&tag);
        assert_eq!(payload[0], 0x91);
        assert_eq!(&payload[1..5], b".mp3");
        let back = parse_audio(&payload).unwrap();
        assert_eq!(back, tag);
    }

    #[test]
    fn ex_audio_tag_aac_fourcc_sequence_start() {
        // AAC with FourCC signalling is the v2 way to carry AAC
        // alongside the other FourCC codecs. Body for
        // SequenceStart is AudioSpecificConfig per ISO/IEC
        // 14496-3 — same shape as the legacy AacSequenceHeader,
        // but reached via FourCC instead of the legacy
        // SoundFormat=10 / AACPacketType=0 path.
        let tag = AudioTag {
            sound_format: AUDIO_FORMAT_EX_HEADER,
            sound_rate: 0,
            sound_size_16bit: false,
            stereo: false,
            aac_packet_type: None,
            ex_packet_type: Some(AUDIO_PACKET_TYPE_SEQUENCE_START),
            audio_fourcc: Some(FOURCC_AAC),
            body: vec![0x12, 0x10], // LC-AAC 44.1k stereo ASC
        };
        let payload = build_audio(&tag);
        assert_eq!(payload[0], 0x90);
        assert_eq!(&payload[1..5], b"mp4a");
        assert_eq!(&payload[5..], &[0x12, 0x10]);

        let back = parse_audio(&payload).unwrap();
        assert_eq!(back, tag);
        assert!(back.is_ex_sequence_header());
        // The legacy `is_aac_sequence_header` predicate stays
        // false because the legacy SoundFormat/AacPacketType
        // discriminator isn't on the wire.
        assert!(!back.is_aac_sequence_header());
    }

    #[test]
    fn ex_audio_tag_sequence_end_empty_body() {
        let tag = AudioTag {
            sound_format: AUDIO_FORMAT_EX_HEADER,
            sound_rate: 0,
            sound_size_16bit: false,
            stereo: false,
            aac_packet_type: None,
            ex_packet_type: Some(AUDIO_PACKET_TYPE_SEQUENCE_END),
            audio_fourcc: Some(FOURCC_OPUS),
            body: vec![],
        };
        let payload = build_audio(&tag);
        // ExHeader=9 | SequenceEnd=2 = 0x92.
        assert_eq!(payload[0], 0x92);
        assert_eq!(&payload[1..5], b"Opus");
        assert_eq!(payload.len(), 5);

        let back = parse_audio(&payload).unwrap();
        assert_eq!(back, tag);
    }

    #[test]
    fn ex_audio_tag_truncated_fourcc_errors() {
        // ExHeader byte alone is not enough — the FourCC follows.
        // Per spec, the parser MUST fail in a controlled manner.
        let truncated = [0x90, b'O', b'p', b'u']; // missing one byte of FourCC
        assert!(parse_audio(&truncated).is_err());
        let just_header = [0x90];
        assert!(parse_audio(&just_header).is_err());
    }

    #[test]
    fn legacy_audio_high_nibble_never_collides_with_ex_header() {
        // Sanity-check the v2 backwards-compatibility claim:
        // every legacy SoundFormat value lies outside
        // {9 = ExHeader}, so a parser branching on
        // `sound_format == ExHeader` never mis-detects a legacy
        // tag as Enhanced.
        for sf in [
            AUDIO_FORMAT_PCM_LE,
            AUDIO_FORMAT_ADPCM,
            AUDIO_FORMAT_MP3,
            AUDIO_FORMAT_PCM_LE_8BIT,
            AUDIO_FORMAT_NELLYMOSER_16K_MONO,
            AUDIO_FORMAT_NELLYMOSER_8K_MONO,
            AUDIO_FORMAT_NELLYMOSER,
            AUDIO_FORMAT_G711_ALAW,
            AUDIO_FORMAT_G711_MULAW,
            AUDIO_FORMAT_AAC,
            AUDIO_FORMAT_SPEEX,
        ] {
            assert_ne!(sf, AUDIO_FORMAT_EX_HEADER, "sf={sf}");
        }
    }
}
