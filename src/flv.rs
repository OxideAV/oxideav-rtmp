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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoTag {
    pub frame_type: u8,
    pub codec_id: u8,
    /// `AvcSequenceHeader` / `AvcNalu` / `AvcEndOfSequence`. `None`
    /// for non-AVC codecs where the first AVC-specific byte doesn't
    /// exist.
    pub avc_packet_type: Option<u8>,
    pub composition_time: i32,
    /// Body: `AVCDecoderConfigurationRecord` for sequence headers, or
    /// a sequence of `[u32 length-BE][NALU bytes]` pairs for NALU
    /// packets.
    pub body: Vec<u8>,
}

impl VideoTag {
    pub fn is_keyframe(&self) -> bool {
        self.frame_type == VIDEO_FRAME_KEYFRAME || self.frame_type == VIDEO_FRAME_GENERATED_KEY
    }
    pub fn is_avc_sequence_header(&self) -> bool {
        self.codec_id == VIDEO_CODEC_AVC
            && self.avc_packet_type == Some(AVC_PACKET_TYPE_SEQUENCE_HEADER)
    }
}

/// Decode the FLV video-tag header from an RTMP video message payload.
pub fn parse_video(payload: &[u8]) -> Result<VideoTag> {
    if payload.is_empty() {
        return Err(Error::Other("FLV video tag: empty".into()));
    }
    let frame_type = payload[0] >> 4;
    let codec_id = payload[0] & 0x0F;
    if codec_id == VIDEO_CODEC_AVC {
        if payload.len() < 5 {
            return Err(Error::Other("FLV/AVC tag: need 5+ bytes".into()));
        }
        let apt = payload[1];
        let cts_raw =
            ((payload[2] as i32) << 16) | ((payload[3] as i32) << 8) | (payload[4] as i32);
        // 24-bit signed — sign-extend.
        let cts = if cts_raw & 0x0080_0000 != 0 {
            cts_raw | -0x0100_0000i32
        } else {
            cts_raw
        };
        Ok(VideoTag {
            frame_type,
            codec_id,
            avc_packet_type: Some(apt),
            composition_time: cts,
            body: payload[5..].to_vec(),
        })
    } else {
        Ok(VideoTag {
            frame_type,
            codec_id,
            avc_packet_type: None,
            composition_time: 0,
            body: payload[1..].to_vec(),
        })
    }
}

/// Build an RTMP video-tag payload. For AVC, writes the 1-byte
/// frame/codec header + AVC packet type + 3-byte composition time.
pub fn build_video(tag: &VideoTag) -> Vec<u8> {
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
        };
        let payload = build_video(&tag);
        let back = parse_video(&payload).unwrap();
        assert_eq!(back.composition_time, -5);
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
