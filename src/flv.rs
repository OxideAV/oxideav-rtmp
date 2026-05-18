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
pub const FOURCC_AV1: [u8; 4] = *b"av01";
pub const FOURCC_VP9: [u8; 4] = *b"vp09";
pub const FOURCC_HEVC: [u8; 4] = *b"hvc1";

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
/// only emitted on the wire for AVC (`codec_id == 7`) and for
/// HEVC + PacketType=`CodedFrames` (FourCC `hvc1`). For
/// `CodedFramesX` and the non-AVC/HEVC FourCCs (`av01`, `vp09`)
/// the field is zero and not encoded.
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
    /// Enhanced RTMP v1 FourCC video codec tag — the four ASCII
    /// bytes following the header byte when `IsExHeader == 1`.
    /// `None` for legacy tags. Values defined by Veovera so far:
    /// `b"av01"` (AV1), `b"vp09"` (VP9), `b"hvc1"` (HEVC).
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
    /// `VPCodecConfigurationRecord` for `vp09`).
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

        // SI24 CompositionTime is on the wire only for HEVC ×
        // PacketTypeCodedFrames (Enhanced RTMP v1 §"If FourCC
        // == HEVC", `SI24 = [CompositionTime Offset]`). For
        // CodedFramesX the spec says: "CompositionTime Offset
        // is implied to equal zero. This is an optimization to
        // save putting SI24 value on the wire." All other
        // FourCCs (av01, vp09) and all other PacketTypes have
        // no CTS field — the body follows the FourCC directly.
        let needs_cts = fcc == FOURCC_HEVC && packet_type == EX_PACKET_TYPE_CODED_FRAMES;
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
        if fcc == FOURCC_HEVC && packet_type == EX_PACKET_TYPE_CODED_FRAMES {
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioTag {
    pub sound_format: u8,
    /// 0 = 5.5k / 1 = 11k / 2 = 22k / 3 = 44k. Encoded in the FLV
    /// header but overridden for AAC (always 3 by spec).
    pub sound_rate: u8,
    pub sound_size_16bit: bool,
    pub stereo: bool,
    /// `AacSequenceHeader` / `AacRaw`. `None` for non-AAC codecs.
    pub aac_packet_type: Option<u8>,
    pub body: Vec<u8>,
}

pub fn parse_audio(payload: &[u8]) -> Result<AudioTag> {
    if payload.is_empty() {
        return Err(Error::Other("FLV audio tag: empty".into()));
    }
    let b0 = payload[0];
    let sound_format = b0 >> 4;
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
            body: payload[2..].to_vec(),
        })
    } else {
        Ok(AudioTag {
            sound_format,
            sound_rate,
            sound_size_16bit,
            stereo,
            aac_packet_type: None,
            body: payload[1..].to_vec(),
        })
    }
}

pub fn build_audio(tag: &AudioTag) -> Vec<u8> {
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

    #[test]
    fn audio_tag_aac_sequence_header_roundtrip() {
        let tag = AudioTag {
            sound_format: AUDIO_FORMAT_AAC,
            sound_rate: 3,
            sound_size_16bit: true,
            stereo: true,
            aac_packet_type: Some(AAC_PACKET_TYPE_SEQUENCE_HEADER),
            body: vec![0x12, 0x10], // LC-AAC 44.1k stereo AudioSpecificConfig
        };
        let payload = build_audio(&tag);
        assert_eq!(payload[0], 0xAF); // AAC + rate 3 + 16-bit + stereo
        assert_eq!(payload[1], 0); // seq header
        let back = parse_audio(&payload).unwrap();
        assert_eq!(back, tag);
    }
}
