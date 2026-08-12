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
//! These shapes are stable across every commodity RTMP implementation
//! we have interoperated with.

use crate::amf::{self, Amf0Value};
use crate::error::{Error, Result};

// §E.4.3 "Video tag body" (FLV 10.1 spec annex E).
// frame type (high nibble of byte 0):
pub const VIDEO_FRAME_KEYFRAME: u8 = 1; // "seekable frame" aka IDR
pub const VIDEO_FRAME_INTER: u8 = 2;
pub const VIDEO_FRAME_DISPOSABLE: u8 = 3; // H.263 only
pub const VIDEO_FRAME_GENERATED_KEY: u8 = 4;
pub const VIDEO_FRAME_INFO: u8 = 5;
/// `VideoFrameType.Command = 5` — the Enhanced-RTMP v2 name for the
/// legacy "video info/command frame" FrameType value. When the
/// FrameType nibble is this value (and, in Enhanced mode, the
/// PacketType is *not* `Metadata`), the VideoTagBody carries **no
/// coded video** — instead a single `UI8` [`VideoCommand`][crate::flv]
/// follows the header (legacy: after the `frame_type|codec_id` byte;
/// Enhanced: after the FourCC). It is the same wire value as
/// [`VIDEO_FRAME_INFO`]; the two names are interchangeable aliases of
/// the on-wire `5`. (`video_file_format_spec_v10_1.pdf` §E.4.3.1
/// "VideoTagBody" / `enhanced-rtmp-v2.pdf` §"ExVideoTagHeader" +
/// `enum VideoFrameType`.)
pub const VIDEO_FRAME_COMMAND: u8 = 5;

// VideoCommand (`enum VideoCommand`, enhanced-rtmp-v2.pdf §"Enhanced
// Video"; same UI8 meanings as the legacy FLV §E.4.3.1 command byte).
// Present in the VideoTagBody only when FrameType == VIDEO_FRAME_COMMAND.
/// `VideoCommand.StartSeek = 0` — start of a client-side seeking video
/// frame sequence.
pub const VIDEO_COMMAND_START_SEEK: u8 = 0;
/// `VideoCommand.EndSeek = 1` — end of a client-side seeking video
/// frame sequence.
pub const VIDEO_COMMAND_END_SEEK: u8 = 1;

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
/// `Multitrack` — turns on video multitrack mode. After this
/// PacketType nibble the next byte packs `multitrackType (UB[4]) |
/// realPacketType (UB[4])`, optionally followed by a shared FourCC
/// (when `multitrackType != ManyTracksManyCodecs`), then a sequence
/// of tracks each carrying `(FourCC if ManyTracksManyCodecs) |
/// trackId(UI8) | (sizeOfVideoTrack(UI24) if not OneTrack) | body`.
/// Decoded by [`Multitrack`] / [`MultitrackTrack`] via
/// [`VideoTag::multitrack`].
pub const EX_PACKET_TYPE_MULTITRACK: u8 = 6;
/// `ModEx` — modifier/extension marker that introduces a chain of
/// size-prefixed ModEx packets before the *real* VideoPacketType is
/// read (`enhanced-rtmp-v2.pdf` §"ExVideoTagHeader" — the
/// `while (videoPacketType == VideoPacketType.ModEx)` loop). One of
/// these chains can carry high-precision timestamps
/// (`TimestampOffsetNano`) or other future per-message modifiers.
pub const EX_PACKET_TYPE_MOD_EX: u8 = 7;

/// `enum VideoPacketModExType` / `enum AudioPacketModExType`
/// (`enhanced-rtmp-v2.pdf` §"ExVideoTagHeader" / §"ExAudioTagHeader").
/// `TimestampOffsetNano = 0` is the only subtype defined today: the
/// ModEx data carries a `bytesToUI24` nanosecond offset (0..=999_999
/// ns) added to the current media message's presentation time
/// without altering the core RTMP millisecond timestamp.
pub const MOD_EX_TYPE_TIMESTAMP_OFFSET_NANO: u8 = 0;

/// One entry in the Enhanced RTMP v2 ModEx prelude chain
/// (`enhanced-rtmp-v2.pdf` §"ExVideoTagHeader" / §"ExAudioTagHeader").
///
/// On the wire each entry is `modExDataSize` (1-byte `UI8 + 1`, or a
/// 16-bit `UI16 + 1` escape when the 8-bit form would be 256),
/// followed by `modExDataSize` bytes of `modExData`, then a single
/// byte whose high nibble is the [`mod_ex_type`][ModEx::mod_ex_type]
/// (`UB[4]`) and whose low nibble is the *next* PacketType (`UB[4]`).
/// The decoded struct keeps only the per-entry payload; the trailing
/// nibble byte is reconstructed from the chain order + the tag's real
/// packet type when re-encoding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModEx {
    /// `AudioPacketModExType` / `VideoPacketModExType` — the high
    /// nibble of the byte that follows the data. One of
    /// `MOD_EX_TYPE_*` (only `TimestampOffsetNano = 0` defined today).
    pub mod_ex_type: u8,
    /// Raw `modExData` bytes (1..=65536 bytes). For
    /// `TimestampOffsetNano` this is at least 3 bytes whose first
    /// three big-endian bytes are the UI24 nanosecond offset.
    pub data: Vec<u8>,
}

impl ModEx {
    /// Decode the `TimestampOffsetNano` value (`bytesToUI24` of the
    /// first three `data` bytes) when this entry is that subtype.
    /// Returns `None` for any other `mod_ex_type` or if `data` is
    /// shorter than the spec-mandated three bytes.
    pub fn timestamp_offset_nano(&self) -> Option<u32> {
        if self.mod_ex_type != MOD_EX_TYPE_TIMESTAMP_OFFSET_NANO || self.data.len() < 3 {
            return None;
        }
        Some(((self.data[0] as u32) << 16) | ((self.data[1] as u32) << 8) | (self.data[2] as u32))
    }

    /// Build a `TimestampOffsetNano` ModEx entry from a nanosecond
    /// offset (0..=999_999 ns per spec) encoded as a `bytesToUI24`
    /// 3-byte big-endian payload.
    pub fn timestamp_offset_nano_entry(nano: u32) -> ModEx {
        ModEx {
            mod_ex_type: MOD_EX_TYPE_TIMESTAMP_OFFSET_NANO,
            data: vec![(nano >> 16) as u8, (nano >> 8) as u8, nano as u8],
        }
    }
}

/// Parse a ModEx prelude chain starting at `payload[start]` (the
/// `payload[start - 1]` low nibble was already decoded as
/// `PacketType.ModEx`). Returns the decoded entries, the *real*
/// PacketType nibble that terminates the chain, and the offset of
/// the first byte after the chain.
///
/// Per `enhanced-rtmp-v2.pdf` the loop is identical for audio and
/// video: read `modExDataSize` (`UI8 + 1`, escaping to `UI16 + 1`
/// when the 8-bit form would be 256), read that many data bytes,
/// then read one nibble byte (`modExType:UB[4] | packetType:UB[4]`)
/// — repeating while the new packetType is again `ModEx`.
fn parse_mod_ex_chain(
    payload: &[u8],
    start: usize,
    mod_ex_value: u8,
    what: &str,
) -> Result<(Vec<ModEx>, u8, usize)> {
    let mut pos = start;
    let mut chain = Vec::new();
    loop {
        // modExDataSize = UI8 + 1
        if pos >= payload.len() {
            return Err(Error::Other(format!(
                "Enhanced RTMP {what} ModEx: truncated reading modExDataSize"
            )));
        }
        let mut size = payload[pos] as usize + 1;
        pos += 1;
        // If the 8-bit form maxes out (== 256), a UI16 + 1 follows.
        if size == 256 {
            if pos + 2 > payload.len() {
                return Err(Error::Other(format!(
                    "Enhanced RTMP {what} ModEx: truncated reading 16-bit modExDataSize"
                )));
            }
            size = (((payload[pos] as usize) << 8) | (payload[pos + 1] as usize)) + 1;
            pos += 2;
        }
        // modExData = UI8[modExDataSize]
        if pos + size > payload.len() {
            return Err(Error::Other(format!(
                "Enhanced RTMP {what} ModEx: truncated reading {size}-byte modExData"
            )));
        }
        let data = payload[pos..pos + size].to_vec();
        pos += size;
        // nibble byte: modExType (UB[4], high) | packetType (UB[4], low)
        if pos >= payload.len() {
            return Err(Error::Other(format!(
                "Enhanced RTMP {what} ModEx: truncated reading modExType/packetType nibble"
            )));
        }
        let nibble = payload[pos];
        pos += 1;
        let mod_ex_type = (nibble >> 4) & 0x0F;
        let next_packet_type = nibble & 0x0F;
        chain.push(ModEx { mod_ex_type, data });
        if next_packet_type != mod_ex_value {
            return Ok((chain, next_packet_type, pos));
        }
        // Another ModEx entry follows.
    }
}

/// Append a ModEx prelude chain to `out`. Each entry writes the
/// `modExDataSize` (`UI8 + 1`, or the `0xFF` + `UI16 + 1` escape
/// when the data is 257..=65536 bytes), the data bytes, and a nibble
/// byte whose high nibble is the entry's `mod_ex_type` and whose low
/// nibble is the PacketType to read *next* — `ModEx` for every entry
/// except the last, whose low nibble is the real `packet_type`.
fn build_mod_ex_chain(out: &mut Vec<u8>, chain: &[ModEx], mod_ex_value: u8, real_packet_type: u8) {
    for (i, entry) in chain.iter().enumerate() {
        let len = entry.data.len();
        // UI8 form covers 1..=255 bytes (stored as len - 1, 0..=254).
        // A stored UI8 of 255 means modExDataSize == 256, which the
        // parser reads as the "switch to UI16" escape — so 256..=65536
        // bytes always take the escape form (UI16 = len - 1).
        if (1..=255).contains(&len) {
            out.push((len - 1) as u8);
        } else {
            // UI16 escape: emit 0xFF (the 8-bit 256 sentinel), then
            // (len - 1) as UI16. len is clamped to the 16-bit range.
            out.push(0xFF);
            let v16 = (len.saturating_sub(1)).min(0xFFFF) as u16;
            out.push((v16 >> 8) as u8);
            out.push(v16 as u8);
        }
        out.extend_from_slice(&entry.data);
        // The terminating nibble byte points at the *next* packet
        // type: ModEx while more entries follow, the real type last.
        let next = if i + 1 < chain.len() {
            mod_ex_value
        } else {
            real_packet_type
        };
        out.push(((entry.mod_ex_type & 0x0F) << 4) | (next & 0x0F));
    }
}

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
/// for the layout. The body shape is decoded by
/// [`MultichannelConfig`]; see [`AudioTag::multichannel_config`] for
/// the lift / round-trip helpers.
pub const AUDIO_PACKET_TYPE_MULTICHANNEL_CONFIG: u8 = 4;
/// `Multitrack` — turns on audio multitrack mode. After this
/// PacketType nibble the next byte packs `multitrackType (UB[4]) |
/// realPacketType (UB[4])`, optionally followed by a shared FourCC
/// (when `multitrackType != ManyTracksManyCodecs`), then a sequence
/// of tracks each carrying `(FourCC if ManyTracksManyCodecs) |
/// trackId(UI8) | (sizeOfAudioTrack(UI24) if not OneTrack) | body`.
/// Decoded by [`Multitrack`] / [`MultitrackTrack`] via
/// [`AudioTag::multitrack`].
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

// ---------------------------------------------------------------------------
// MultichannelConfig — Enhanced RTMP v2 §"ExAudioTagBody"
// ---------------------------------------------------------------------------
//
// When AudioPacketType == MultichannelConfig (= 4) the per-packet body
// has the layout:
//
//   audioChannelOrder = UI8 as AudioChannelOrder
//   channelCount      = UI8
//   if (audioChannelOrder == Custom)  audioChannelMapping = UI8[channelCount]
//   if (audioChannelOrder == Native)  audioChannelFlags   = UI32
//   if (audioChannelOrder == Unspecified) nothing further
//
// This block is sent on a separate `MultichannelConfig` audio message and
// applies to the surrounding sequence; it does NOT itself carry codec
// bitstream bytes.

/// AudioChannelOrder discriminator (UI8) per enhanced-rtmp-v2.pdf
/// §"ExAudioTagBody" `enum AudioChannelOrder`: only the channel count
/// is specified, channel order is left to the codec / app.
pub const AUDIO_CHANNEL_ORDER_UNSPECIFIED: u8 = 0;
/// AudioChannelOrder.Native: the channels are in the order defined by
/// the AudioChannel enum; an `AudioChannelFlags` UI32 mask follows the
/// channel count, with bits indexing into [`audio_channel_mask`].
pub const AUDIO_CHANNEL_ORDER_NATIVE: u8 = 1;
/// AudioChannelOrder.Custom: each channel's speaker assignment is
/// spelled out by `audioChannelMapping = UI8[channelCount]`, where each
/// UI8 is an `AudioChannel` value (see [`audio_channel`]).
pub const AUDIO_CHANNEL_ORDER_CUSTOM: u8 = 2;

/// `AudioChannel` enum values (UI8) per enhanced-rtmp-v2.pdf
/// §"ExAudioTagBody" — speaker positions used for
/// `AudioChannelOrder.Custom` mappings. The numeric values match the
/// spec table 1:1 and align with the bit indices in
/// [`audio_channel_mask`].
pub mod audio_channel {
    pub const FRONT_LEFT: u8 = 0;
    pub const FRONT_RIGHT: u8 = 1;
    pub const FRONT_CENTER: u8 = 2;
    pub const LOW_FREQUENCY1: u8 = 3;
    pub const BACK_LEFT: u8 = 4;
    pub const BACK_RIGHT: u8 = 5;
    pub const FRONT_LEFT_CENTER: u8 = 6;
    pub const FRONT_RIGHT_CENTER: u8 = 7;
    pub const BACK_CENTER: u8 = 8;
    pub const SIDE_LEFT: u8 = 9;
    pub const SIDE_RIGHT: u8 = 10;
    pub const TOP_CENTER: u8 = 11;
    pub const TOP_FRONT_LEFT: u8 = 12;
    pub const TOP_FRONT_CENTER: u8 = 13;
    pub const TOP_FRONT_RIGHT: u8 = 14;
    pub const TOP_BACK_LEFT: u8 = 15;
    pub const TOP_BACK_CENTER: u8 = 16;
    pub const TOP_BACK_RIGHT: u8 = 17;
    // mappings completing 22.2 multichannel audio (SMPTE ST 2036-2-2008)
    pub const LOW_FREQUENCY2: u8 = 18;
    pub const TOP_SIDE_LEFT: u8 = 19;
    pub const TOP_SIDE_RIGHT: u8 = 20;
    pub const BOTTOM_FRONT_CENTER: u8 = 21;
    pub const BOTTOM_FRONT_LEFT: u8 = 22;
    pub const BOTTOM_FRONT_RIGHT: u8 = 23;
    /// Channel is empty / can be safely skipped.
    pub const UNUSED: u8 = 0xfe;
    /// Channel contains data, but its speaker configuration is unknown.
    pub const UNKNOWN: u8 = 0xff;
}

/// `AudioChannelMask` bitmask values (UI32) per enhanced-rtmp-v2.pdf
/// §"ExAudioTagBody" — used with `AudioChannelOrder.Native` to indicate
/// which channels of the standard layout are present.
pub mod audio_channel_mask {
    pub const FRONT_LEFT: u32 = 0x000001;
    pub const FRONT_RIGHT: u32 = 0x000002;
    pub const FRONT_CENTER: u32 = 0x000004;
    pub const LOW_FREQUENCY1: u32 = 0x000008;
    pub const BACK_LEFT: u32 = 0x000010;
    pub const BACK_RIGHT: u32 = 0x000020;
    pub const FRONT_LEFT_CENTER: u32 = 0x000040;
    pub const FRONT_RIGHT_CENTER: u32 = 0x000080;
    pub const BACK_CENTER: u32 = 0x000100;
    pub const SIDE_LEFT: u32 = 0x000200;
    pub const SIDE_RIGHT: u32 = 0x000400;
    pub const TOP_CENTER: u32 = 0x000800;
    pub const TOP_FRONT_LEFT: u32 = 0x001000;
    pub const TOP_FRONT_CENTER: u32 = 0x002000;
    pub const TOP_FRONT_RIGHT: u32 = 0x004000;
    pub const TOP_BACK_LEFT: u32 = 0x008000;
    pub const TOP_BACK_CENTER: u32 = 0x010000;
    pub const TOP_BACK_RIGHT: u32 = 0x020000;
    // 22.2 surround additions
    pub const LOW_FREQUENCY2: u32 = 0x040000;
    pub const TOP_SIDE_LEFT: u32 = 0x080000;
    pub const TOP_SIDE_RIGHT: u32 = 0x100000;
    pub const BOTTOM_FRONT_CENTER: u32 = 0x200000;
    pub const BOTTOM_FRONT_LEFT: u32 = 0x400000;
    pub const BOTTOM_FRONT_RIGHT: u32 = 0x800000;
}

/// Decoded body of an Enhanced RTMP v2
/// `AudioPacketType.MultichannelConfig` message
/// (enhanced-rtmp-v2.pdf §"ExAudioTagBody"). The body sits in
/// [`AudioTag::body`] verbatim on parse; callers can lift it into this
/// strongly-typed view via [`MultichannelConfig::parse`] and round-trip
/// back through [`MultichannelConfig::encode`] / [`AudioTag::multichannel_config_tag`].
///
/// Per spec the body length depends on `audio_channel_order`:
///   - `Unspecified` (`0`): 2 bytes (`order`, `channel_count`).
///   - `Native` (`1`): 6 bytes (`order`, `channel_count`, UI32 flags).
///   - `Custom` (`2`): `2 + channel_count` bytes (mapping is a UI8 per
///     channel).
///
/// Any UI8 `audio_channel_order` value that is not one of those three
/// surfaces as [`MultichannelConfigOrder::Reserved`] — the parser does
/// not invent a layout, and the build path will encode just the
/// `(order, channel_count)` prefix, leaving any trailing bytes to the
/// caller via [`MultichannelConfig::extra`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MultichannelConfig {
    /// The full discriminator union from the spec table. See
    /// [`MultichannelConfigOrder`] for the shape per variant.
    pub order: MultichannelConfigOrder,
    /// Number of channels in the multichannel stream. UI8 on the wire,
    /// so values 0..=255 are representable.
    pub channel_count: u8,
    /// Trailing bytes preserved verbatim when [`order`](MultichannelConfig::order) is
    /// [`MultichannelConfigOrder::Reserved`] (forward-compat with
    /// future spec additions). Empty for the three recognised orders.
    pub extra: Vec<u8>,
}

/// Discriminated union of the per-`audioChannelOrder` body shape from
/// `ExAudioTagBody`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MultichannelConfigOrder {
    /// `AudioChannelOrder.Unspecified` — only the channel count is
    /// specified, no trailing per-channel data.
    Unspecified,
    /// `AudioChannelOrder.Native` — channels appear in the order
    /// defined by the `AudioChannel` enum. The 32-bit
    /// `audioChannelFlags` mask reports which of the standard channels
    /// are present; bit positions match [`audio_channel_mask`].
    Native { flags: u32 },
    /// `AudioChannelOrder.Custom` — `audioChannelMapping[channelCount]`
    /// names the speaker (an `AudioChannel` value) for each channel,
    /// in stream order. Length equals
    /// [`MultichannelConfig::channel_count`].
    Custom { mapping: Vec<u8> },
    /// A reserved / forward-compat `audioChannelOrder` value the parser
    /// did not recognise. The raw discriminator byte is preserved here
    /// so callers can pass the message through unchanged; trailing
    /// body bytes (if any) sit in [`MultichannelConfig::extra`].
    Reserved(u8),
}

impl MultichannelConfigOrder {
    /// UI8 discriminator value as it appears on the wire.
    pub fn as_u8(&self) -> u8 {
        match self {
            MultichannelConfigOrder::Unspecified => AUDIO_CHANNEL_ORDER_UNSPECIFIED,
            MultichannelConfigOrder::Native { .. } => AUDIO_CHANNEL_ORDER_NATIVE,
            MultichannelConfigOrder::Custom { .. } => AUDIO_CHANNEL_ORDER_CUSTOM,
            MultichannelConfigOrder::Reserved(v) => *v,
        }
    }
}

impl MultichannelConfig {
    /// Parse the body bytes of an `AudioPacketType.MultichannelConfig`
    /// audio message (the bytes that sit in [`AudioTag::body`] after a
    /// successful [`parse_audio`] call). Returns `Err(Error::Other)` on
    /// truncation; an unrecognised `audioChannelOrder` does NOT trigger
    /// an error — it is preserved as [`MultichannelConfigOrder::Reserved`]
    /// and any trailing bytes flow through [`MultichannelConfig::extra`].
    pub fn parse(body: &[u8]) -> Result<MultichannelConfig> {
        if body.len() < 2 {
            return Err(Error::Other(
                "MultichannelConfig: need 2 bytes (order + channelCount)".into(),
            ));
        }
        let order_byte = body[0];
        let channel_count = body[1];
        match order_byte {
            AUDIO_CHANNEL_ORDER_UNSPECIFIED => {
                if body.len() != 2 {
                    return Err(Error::Other(
                        "MultichannelConfig.Unspecified: trailing bytes after channelCount".into(),
                    ));
                }
                Ok(MultichannelConfig {
                    order: MultichannelConfigOrder::Unspecified,
                    channel_count,
                    extra: Vec::new(),
                })
            }
            AUDIO_CHANNEL_ORDER_NATIVE => {
                if body.len() != 6 {
                    return Err(Error::Other(
                        "MultichannelConfig.Native: need 6 bytes (order + count + UI32 flags)"
                            .into(),
                    ));
                }
                let flags = u32::from_be_bytes([body[2], body[3], body[4], body[5]]);
                Ok(MultichannelConfig {
                    order: MultichannelConfigOrder::Native { flags },
                    channel_count,
                    extra: Vec::new(),
                })
            }
            AUDIO_CHANNEL_ORDER_CUSTOM => {
                let need = 2 + channel_count as usize;
                if body.len() != need {
                    return Err(Error::Other(format!(
                        "MultichannelConfig.Custom: need {need} bytes for channelCount={channel_count}, got {}",
                        body.len()
                    )));
                }
                Ok(MultichannelConfig {
                    order: MultichannelConfigOrder::Custom {
                        mapping: body[2..need].to_vec(),
                    },
                    channel_count,
                    extra: Vec::new(),
                })
            }
            other => Ok(MultichannelConfig {
                order: MultichannelConfigOrder::Reserved(other),
                channel_count,
                extra: body[2..].to_vec(),
            }),
        }
    }

    /// Serialise to the byte layout `parse` consumes. The output is
    /// what [`AudioTag::body`] needs to hold when constructing an
    /// outgoing `MultichannelConfig` message.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(8);
        out.push(self.order.as_u8());
        out.push(self.channel_count);
        match &self.order {
            MultichannelConfigOrder::Unspecified => {}
            MultichannelConfigOrder::Native { flags } => {
                out.extend_from_slice(&flags.to_be_bytes());
            }
            MultichannelConfigOrder::Custom { mapping } => {
                out.extend_from_slice(mapping);
            }
            MultichannelConfigOrder::Reserved(_) => {
                out.extend_from_slice(&self.extra);
            }
        }
        out
    }
}

// ---------------------------------------------------------------------------
// ColorInfo — Enhanced RTMP §"Metadata Frame" (VideoPacketType.Metadata)
// ---------------------------------------------------------------------------
//
// A `VideoPacketType.Metadata` (= 4) video message carries an AMF-encoded
// sequence of `[name, value]` pairs. The only `name` Enhanced RTMP defines is
// `"colorInfo"`, whose `value` is an Object with three optional sub-objects
// describing HDR metadata for a BT.2020 (Rec. 2020) source:
//
//   type ColorInfo = {
//     colorConfig: {
//       bitDepth:                number,  // 8 | 10 | 12
//       colorPrimaries:          number,  // H.273 enumeration [0-255]
//       transferCharacteristics: number,  // H.273 enumeration [0-255]
//       matrixCoefficients:      number,  // H.273 enumeration [0-255]
//     },
//     hdrCll: { maxFall: number, maxCLL: number },          // cd/m2
//     hdrMdcv: {                                             // ST 2086:2018
//       redX, redY, greenX, greenY, blueX, blueY,
//       whitePointX, whitePointY,
//       maxLuminance, minLuminance,                          // cd/m2 (nits)
//     },
//   }
//
// Every property is OPTIONAL on the wire (the spec marks the sub-objects
// SHOULD/RECOMMENDED, never MUST), so a partial colorInfo — e.g. only
// colorConfig — must round-trip. We therefore model each field as
// `Option<f64>` (AMF's native numeric type is the IEEE-754 double, so storing
// f64 keeps the round-trip byte-exact instead of coercing to an integer that
// would lose a fractional luminance value). A missing sub-object is `None`;
// an empty `{}` object is `Some(default)` (all fields `None`), preserving the
// spec's "reset to original color state via an empty object" signal distinct
// from "sub-object absent".

/// `colorConfig` sub-object of [`ColorInfo`]. Bit depth plus the three
/// ITU-T H.273 / ISO-IEC 23091-4 enumeration indices (colour primaries,
/// transfer characteristics, matrix coefficients). Stored as `Option<f64>`
/// to round-trip a partial object byte-for-byte; use [`ColorConfig::is_empty`]
/// to detect the all-absent case.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ColorConfig {
    /// Bits per colour channel. SHOULD be 8, 10 or 12.
    pub bit_depth: Option<f64>,
    /// Colour primaries — H.273 "Colour primaries" table index [0-255].
    pub color_primaries: Option<f64>,
    /// Transfer characteristics — H.273 table index [0-255] (e.g. PQ, HLG).
    pub transfer_characteristics: Option<f64>,
    /// Matrix coefficients — H.273 table index [0-255].
    pub matrix_coefficients: Option<f64>,
}

impl ColorConfig {
    /// True when no field is set (an absent or `{}` `colorConfig`).
    pub fn is_empty(&self) -> bool {
        self.bit_depth.is_none()
            && self.color_primaries.is_none()
            && self.transfer_characteristics.is_none()
            && self.matrix_coefficients.is_none()
    }
}

/// `hdrCll` sub-object of [`ColorInfo`] — content light level. Both values
/// are in cd/m2 (nits), spec range `[0.0001, 10000]`.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct HdrCll {
    /// Maximum frame-average light level over the playback sequence.
    pub max_fall: Option<f64>,
    /// Maximum light level of any single pixel over the playback sequence.
    pub max_cll: Option<f64>,
}

impl HdrCll {
    /// True when neither value is set.
    pub fn is_empty(&self) -> bool {
        self.max_fall.is_none() && self.max_cll.is_none()
    }
}

/// `hdrMdcv` sub-object of [`ColorInfo`] — mastering display colour volume
/// per SMPTE ST 2086:2018. Chromaticity coordinates are CIE-1931 xy; the
/// luminance pair is in cd/m2 (nits).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct HdrMdcv {
    pub red_x: Option<f64>,
    pub red_y: Option<f64>,
    pub green_x: Option<f64>,
    pub green_y: Option<f64>,
    pub blue_x: Option<f64>,
    pub blue_y: Option<f64>,
    pub white_point_x: Option<f64>,
    pub white_point_y: Option<f64>,
    /// Max display luminance of the mastering display, range `[5, 10000]`.
    pub max_luminance: Option<f64>,
    /// Min display luminance of the mastering display, range `[0.0001, 5]`.
    pub min_luminance: Option<f64>,
}

impl HdrMdcv {
    /// True when no coordinate or luminance field is set.
    pub fn is_empty(&self) -> bool {
        self.red_x.is_none()
            && self.red_y.is_none()
            && self.green_x.is_none()
            && self.green_y.is_none()
            && self.blue_x.is_none()
            && self.blue_y.is_none()
            && self.white_point_x.is_none()
            && self.white_point_y.is_none()
            && self.max_luminance.is_none()
            && self.min_luminance.is_none()
    }
}

/// Strongly-typed view of the `"colorInfo"` HDR metadata object carried in a
/// `VideoPacketType.Metadata` video message (Enhanced RTMP §"Metadata Frame").
///
/// Each of the three sub-objects (`colorConfig`, `hdrCll`, `hdrMdcv`) is
/// `Option`: `None` means the property was absent from the wire object,
/// `Some(..)` (possibly all-`None` inside) means it was present. This
/// distinction matters because the spec's "reset to the original color state"
/// signal is an empty object `{}` — distinct from omitting `colorInfo`
/// altogether (which sends `Undefined`, surfaced as [`ColorInfo::is_reset`]).
///
/// Lift it from a parsed metadata [`VideoTag`] with
/// [`VideoTag::color_info`], and rebuild an outgoing tag with
/// [`VideoTag::color_info_tag`].
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ColorInfo {
    pub color_config: Option<ColorConfig>,
    pub hdr_cll: Option<HdrCll>,
    pub hdr_mdcv: Option<HdrMdcv>,
}

impl ColorInfo {
    /// Decode a `colorInfo` value (the AMF `value` half of the
    /// `["colorInfo", value]` pair) from an already-decoded [`Amf0Value`].
    ///
    /// * An [`Amf0Value::Object`] / [`Amf0Value::EcmaArray`] is walked for
    ///   the three sub-objects.
    /// * [`Amf0Value::Undefined`] (the spec's RECOMMENDED reset signal) or
    ///   an empty object decode to the all-`None` reset state.
    /// * Any other AMF type is rejected with [`Error::Other`].
    pub fn from_amf0(value: &Amf0Value) -> Result<ColorInfo> {
        match value {
            Amf0Value::Undefined | Amf0Value::Null => Ok(ColorInfo::default()),
            Amf0Value::Object(_) | Amf0Value::EcmaArray(_) => Ok(ColorInfo {
                color_config: value.get("colorConfig").map(parse_color_config),
                hdr_cll: value.get("hdrCll").map(parse_hdr_cll),
                hdr_mdcv: value.get("hdrMdcv").map(parse_hdr_mdcv),
            }),
            _ => Err(Error::Other(
                "colorInfo: value must be an Object/ECMA array or Undefined".into(),
            )),
        }
    }

    /// True when this is the reset signal — no sub-object present. Encodes as
    /// `Undefined` per the spec's RECOMMENDED reset approach (see
    /// [`ColorInfo::to_amf0`]).
    pub fn is_reset(&self) -> bool {
        self.color_config.is_none() && self.hdr_cll.is_none() && self.hdr_mdcv.is_none()
    }

    /// Encode to the AMF `value` half of the `["colorInfo", value]` pair.
    ///
    /// The reset state ([`ColorInfo::is_reset`]) encodes as
    /// [`Amf0Value::Undefined`] — the spec's RECOMMENDED reset form. A
    /// present-but-empty sub-object encodes as an empty `{}` object so the
    /// presence bit round-trips.
    pub fn to_amf0(&self) -> Amf0Value {
        if self.is_reset() {
            return Amf0Value::Undefined;
        }
        let mut obj: Vec<(String, Amf0Value)> = Vec::new();
        if let Some(cc) = &self.color_config {
            obj.push(("colorConfig".into(), color_config_to_amf0(cc)));
        }
        if let Some(cll) = &self.hdr_cll {
            obj.push(("hdrCll".into(), hdr_cll_to_amf0(cll)));
        }
        if let Some(mdcv) = &self.hdr_mdcv {
            obj.push(("hdrMdcv".into(), hdr_mdcv_to_amf0(mdcv)));
        }
        Amf0Value::Object(obj)
    }
}

fn opt_num(v: &Amf0Value, key: &str) -> Option<f64> {
    v.get(key).and_then(Amf0Value::as_f64)
}

fn parse_color_config(v: &Amf0Value) -> ColorConfig {
    ColorConfig {
        bit_depth: opt_num(v, "bitDepth"),
        color_primaries: opt_num(v, "colorPrimaries"),
        transfer_characteristics: opt_num(v, "transferCharacteristics"),
        matrix_coefficients: opt_num(v, "matrixCoefficients"),
    }
}

fn parse_hdr_cll(v: &Amf0Value) -> HdrCll {
    HdrCll {
        max_fall: opt_num(v, "maxFall"),
        max_cll: opt_num(v, "maxCLL"),
    }
}

fn parse_hdr_mdcv(v: &Amf0Value) -> HdrMdcv {
    HdrMdcv {
        red_x: opt_num(v, "redX"),
        red_y: opt_num(v, "redY"),
        green_x: opt_num(v, "greenX"),
        green_y: opt_num(v, "greenY"),
        blue_x: opt_num(v, "blueX"),
        blue_y: opt_num(v, "blueY"),
        white_point_x: opt_num(v, "whitePointX"),
        white_point_y: opt_num(v, "whitePointY"),
        max_luminance: opt_num(v, "maxLuminance"),
        min_luminance: opt_num(v, "minLuminance"),
    }
}

fn push_num(obj: &mut Vec<(String, Amf0Value)>, key: &str, val: Option<f64>) {
    if let Some(n) = val {
        obj.push((key.to_string(), Amf0Value::Number(n)));
    }
}

fn color_config_to_amf0(cc: &ColorConfig) -> Amf0Value {
    let mut obj = Vec::new();
    push_num(&mut obj, "bitDepth", cc.bit_depth);
    push_num(&mut obj, "colorPrimaries", cc.color_primaries);
    push_num(
        &mut obj,
        "transferCharacteristics",
        cc.transfer_characteristics,
    );
    push_num(&mut obj, "matrixCoefficients", cc.matrix_coefficients);
    Amf0Value::Object(obj)
}

fn hdr_cll_to_amf0(cll: &HdrCll) -> Amf0Value {
    let mut obj = Vec::new();
    push_num(&mut obj, "maxFall", cll.max_fall);
    push_num(&mut obj, "maxCLL", cll.max_cll);
    Amf0Value::Object(obj)
}

fn hdr_mdcv_to_amf0(mdcv: &HdrMdcv) -> Amf0Value {
    let mut obj = Vec::new();
    push_num(&mut obj, "redX", mdcv.red_x);
    push_num(&mut obj, "redY", mdcv.red_y);
    push_num(&mut obj, "greenX", mdcv.green_x);
    push_num(&mut obj, "greenY", mdcv.green_y);
    push_num(&mut obj, "blueX", mdcv.blue_x);
    push_num(&mut obj, "blueY", mdcv.blue_y);
    push_num(&mut obj, "whitePointX", mdcv.white_point_x);
    push_num(&mut obj, "whitePointY", mdcv.white_point_y);
    push_num(&mut obj, "maxLuminance", mdcv.max_luminance);
    push_num(&mut obj, "minLuminance", mdcv.min_luminance);
    Amf0Value::Object(obj)
}

// ---------------------------------------------------------------------------
// onMetaData — Enhanced RTMP v2 §"Enhancing onMetaData"
// ---------------------------------------------------------------------------
//
// FLV metadata is carried in a SCRIPTDATA segment whose ScriptTagBody
// encapsulates the method name `onMetaData` and a single argument of type
// ECMA array. The array holds metadata properties describing the stream;
// the spec's "Typical properties found in the onMetaData argument object"
// table enumerates the well-known names. Availability varies by encoder, so
// every property is optional.
//
// Two properties carry a codec identifier that MAY be a FourCC encoded as a
// number: `audiocodecid` / `videocodecid`. The spec states the FourCC value
// is big-endian relative to the underlying ASCII character sequence
// (e.g. "Opus" == 0x4F707573 == 1332770163.0, "av01" == 0x61763031 ==
// 1635135537.0). A small legacy CodecID (a single-byte FLV codec id from
// the legacy AudioTagHeader / VideoTagHeader tables) is NOT a FourCC; the
// `*_fourcc()` accessors only reconstruct the four ASCII bytes when the
// value is in the FourCC range and every byte is printable ASCII.

/// Strongly-typed view of the `onMetaData` argument object (Enhanced RTMP v2
/// §"Enhancing onMetaData").
///
/// Lifts the spec's "Typical properties found in the onMetaData argument
/// object" table into named fields. Every field is `Option` because the
/// availability of each property "may vary depending on the software used to
/// create the FLV" — `None` means the property was absent. Properties not in
/// the table are preserved verbatim in [`OnMetaData::extra`] so a
/// round-trip is lossless.
///
/// Decode with [`OnMetaData::from_amf0`] (pass the AMF `value` half of the
/// `["onMetaData", value]` pair) and re-encode with [`OnMetaData::to_amf0`],
/// which produces an [`Amf0Value::EcmaArray`] as the spec requires.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct OnMetaData {
    /// Audio codec ID. A legacy single-byte FLV CodecID, or a FourCC encoded
    /// as a number (see [`OnMetaData::audio_fourcc`]).
    pub audiocodecid: Option<f64>,
    /// Audio bitrate, in kilobits per second.
    pub audiodatarate: Option<f64>,
    /// Delay introduced by the audio codec, in seconds.
    pub audiodelay: Option<f64>,
    /// Frequency at which the audio stream is replayed (Hz).
    pub audiosamplerate: Option<f64>,
    /// Number of bits used to represent each audio sample.
    pub audiosamplesize: Option<f64>,
    /// The last video frame is a key frame (seekable to end).
    pub can_seek_to_end: Option<bool>,
    /// Creation date and time.
    pub creationdate: Option<String>,
    /// Total duration of the file, in seconds.
    pub duration: Option<f64>,
    /// Total size of the file, in bytes.
    pub filesize: Option<f64>,
    /// Number of frames per second.
    pub framerate: Option<f64>,
    /// Height of the video, in pixels.
    pub height: Option<f64>,
    /// Indicates stereo audio.
    pub stereo: Option<bool>,
    /// Video codec ID. A legacy single-byte FLV CodecID, or a FourCC encoded
    /// as a number (see [`OnMetaData::video_fourcc`]).
    pub videocodecid: Option<f64>,
    /// Video bitrate, in kilobits per second.
    pub videodatarate: Option<f64>,
    /// Width of the video, in pixels.
    pub width: Option<f64>,
    /// Per-track metadata for additional audio tracks beyond the default
    /// (`trackId` 0). Keyed by `trackId` (1, 2, 3, …); each value is an
    /// object of track-level attributes. Preserved verbatim because the
    /// spec's field list for each track object is open-ended.
    pub audio_track_id_info_map: Option<Amf0Value>,
    /// Per-track metadata for additional video tracks beyond the default.
    /// Mirrors [`OnMetaData::audio_track_id_info_map`].
    pub video_track_id_info_map: Option<Amf0Value>,
    /// Any property not in the spec's typical-properties table, preserved in
    /// wire order so a decode/encode round-trip is lossless.
    pub extra: Vec<(String, Amf0Value)>,
}

impl OnMetaData {
    /// Decode the `onMetaData` argument from an already-decoded [`Amf0Value`].
    ///
    /// The spec mandates the argument be an ECMA array, but commodity peers
    /// also emit a plain anonymous Object, so both are accepted. Any other
    /// AMF type is rejected with [`Error::Other`].
    pub fn from_amf0(value: &Amf0Value) -> Result<OnMetaData> {
        let pairs: &[(String, Amf0Value)] = match value {
            Amf0Value::EcmaArray(p) | Amf0Value::Object(p) => p.as_slice(),
            _ => {
                return Err(Error::Other(
                    "onMetaData: argument must be an ECMA array or Object".into(),
                ))
            }
        };
        let mut m = OnMetaData::default();
        for (k, v) in pairs {
            match k.as_str() {
                "audiocodecid" => m.audiocodecid = v.as_f64(),
                "audiodatarate" => m.audiodatarate = v.as_f64(),
                "audiodelay" => m.audiodelay = v.as_f64(),
                "audiosamplerate" => m.audiosamplerate = v.as_f64(),
                "audiosamplesize" => m.audiosamplesize = v.as_f64(),
                "canSeekToEnd" => m.can_seek_to_end = v.as_bool(),
                "creationdate" => m.creationdate = v.as_str().map(str::to_owned),
                "duration" => m.duration = v.as_f64(),
                "filesize" => m.filesize = v.as_f64(),
                "framerate" => m.framerate = v.as_f64(),
                "height" => m.height = v.as_f64(),
                "stereo" => m.stereo = v.as_bool(),
                "videocodecid" => m.videocodecid = v.as_f64(),
                "videodatarate" => m.videodatarate = v.as_f64(),
                "width" => m.width = v.as_f64(),
                "audioTrackIdInfoMap" => m.audio_track_id_info_map = Some(v.clone()),
                "videoTrackIdInfoMap" => m.video_track_id_info_map = Some(v.clone()),
                _ => m.extra.push((k.clone(), v.clone())),
            }
        }
        Ok(m)
    }

    /// Re-encode to the AMF `value` half of the `["onMetaData", value]` pair
    /// as an [`Amf0Value::EcmaArray`] (the spec-mandated argument type).
    ///
    /// Known fields are emitted first in the spec table's order, then the
    /// two track-info maps, then any [`OnMetaData::extra`] properties in
    /// their preserved order.
    pub fn to_amf0(&self) -> Amf0Value {
        let mut obj: Vec<(String, Amf0Value)> = Vec::new();
        push_num(&mut obj, "audiocodecid", self.audiocodecid);
        push_num(&mut obj, "audiodatarate", self.audiodatarate);
        push_num(&mut obj, "audiodelay", self.audiodelay);
        push_num(&mut obj, "audiosamplerate", self.audiosamplerate);
        push_num(&mut obj, "audiosamplesize", self.audiosamplesize);
        push_bool(&mut obj, "canSeekToEnd", self.can_seek_to_end);
        if let Some(s) = &self.creationdate {
            obj.push(("creationdate".into(), Amf0Value::String(s.clone())));
        }
        push_num(&mut obj, "duration", self.duration);
        push_num(&mut obj, "filesize", self.filesize);
        push_num(&mut obj, "framerate", self.framerate);
        push_num(&mut obj, "height", self.height);
        push_bool(&mut obj, "stereo", self.stereo);
        push_num(&mut obj, "videocodecid", self.videocodecid);
        push_num(&mut obj, "videodatarate", self.videodatarate);
        push_num(&mut obj, "width", self.width);
        if let Some(v) = &self.audio_track_id_info_map {
            obj.push(("audioTrackIdInfoMap".into(), v.clone()));
        }
        if let Some(v) = &self.video_track_id_info_map {
            obj.push(("videoTrackIdInfoMap".into(), v.clone()));
        }
        obj.extend(self.extra.iter().cloned());
        Amf0Value::EcmaArray(obj)
    }

    /// Reconstruct the four-character codec FourCC from [`audiocodecid`]
    /// when it carries a FourCC encoded as a number (per the spec's
    /// big-endian note, e.g. "Opus" == 0x4F707573). Returns `None` for an
    /// absent value or a legacy single-byte CodecID.
    ///
    /// [`audiocodecid`]: OnMetaData::audiocodecid
    pub fn audio_fourcc(&self) -> Option<[u8; 4]> {
        self.audiocodecid.and_then(num_to_fourcc)
    }

    /// Reconstruct the FourCC from [`videocodecid`]. See
    /// [`OnMetaData::audio_fourcc`].
    ///
    /// [`videocodecid`]: OnMetaData::videocodecid
    pub fn video_fourcc(&self) -> Option<[u8; 4]> {
        self.videocodecid.and_then(num_to_fourcc)
    }
}

fn push_bool(obj: &mut Vec<(String, Amf0Value)>, key: &str, val: Option<bool>) {
    if let Some(b) = val {
        obj.push((key.to_string(), Amf0Value::Boolean(b)));
    }
}

/// Decode a codec-id number into a FourCC per the Enhanced RTMP v2 note that
/// a FourCC value is big-endian relative to the underlying ASCII character
/// sequence. Only values that are exactly representable as a `u32` *and*
/// whose four bytes are all printable ASCII (0x20..=0x7e) are treated as a
/// FourCC — this rejects the small legacy single-byte CodecIDs that share
/// the `*codecid` field.
fn num_to_fourcc(n: f64) -> Option<[u8; 4]> {
    if !n.is_finite() || n < 0.0 || n > u32::MAX as f64 || n.fract() != 0.0 {
        return None;
    }
    let bytes = (n as u32).to_be_bytes();
    if bytes.iter().all(|&b| (0x20..=0x7e).contains(&b)) {
        Some(bytes)
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// Multitrack — Enhanced RTMP v2 §"ExVideoTagBody" / §"ExAudioTagBody"
// ---------------------------------------------------------------------------
//
// When VideoPacketType == Multitrack (= 6) or AudioPacketType == Multitrack
// (= 5), the body holds one or more tracks rather than a single track's
// payload. The per-packet body has the layout (audio mirrors video):
//
//   multitrackType   = UB[4] as AvMultitrackType    // high nibble of next byte
//   realPacketType   = UB[4] as VideoPacketType     // low nibble (the *real*
//                                                   // PacketType the tracks
//                                                   // carry; MUST NOT be
//                                                   // Multitrack)
//   if (multitrackType != ManyTracksManyCodecs) {
//     sharedFourCc = FOURCC                         // codec shared by all tracks
//   }
//   while (more) {
//     if (multitrackType == ManyTracksManyCodecs) {
//       trackFourCc = FOURCC                        // per-track codec
//     }
//     trackId      = UI8
//     if (multitrackType != OneTrack) {
//       sizeOfTrack = UI24                          // bytes of the body that follows
//     }
//     body         = UI8[sizeOfTrack | rest-of-message]
//   }
//
// OneTrack mode carries exactly one track and no size field; the body runs
// to the end of the message. ManyTracks shares a single FourCC across all
// tracks. ManyTracksManyCodecs carries a per-track FourCC.

/// AvMultitrackType discriminator (UI8 in the spec's `enum AvMultitrackType`,
/// stored on the wire as the high nibble of the byte immediately after the
/// Multitrack PacketType nibble). See enhanced-rtmp-v2.pdf §"ExVideoTagBody" /
/// §"ExAudioTagBody".
pub const AV_MULTITRACK_TYPE_ONE_TRACK: u8 = 0;
/// All tracks share the same codec (`sharedFourCc` read once before the
/// track loop, `sizeOfTrack` UI24 present on every track).
pub const AV_MULTITRACK_TYPE_MANY_TRACKS: u8 = 1;
/// Each track carries its own codec (`trackFourCc` read inside the loop for
/// every track, no shared FourCC in the header).
pub const AV_MULTITRACK_TYPE_MANY_TRACKS_MANY_CODECS: u8 = 2;

/// Decoded `Multitrack` body of an Enhanced RTMP v2 video or audio message
/// (enhanced-rtmp-v2.pdf §"ExVideoTagBody" / §"ExAudioTagBody"). The decoded
/// view sits in [`VideoTag::multitrack`] / [`AudioTag::multitrack`]; when
/// present, the tag's [`VideoTag::ex_packet_type`] /
/// [`AudioTag::ex_packet_type`] holds the *real* (inner) PacketType the
/// tracks carry (e.g. `CodedFrames`, `SequenceStart`), and the tag's
/// [`VideoTag::fourcc`] / [`AudioTag::audio_fourcc`] holds the shared FourCC
/// when [`multitrack_type`][Multitrack::multitrack_type] is `OneTrack` or
/// `ManyTracks`. For `ManyTracksManyCodecs` the outer FourCC is `None`
/// (each track carries its own).
///
/// The [`VideoTag::body`] / [`AudioTag::body`] field is unused for
/// multitrack tags — track payloads live inside
/// [`MultitrackTrack::body`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Multitrack {
    /// `AvMultitrackType` discriminator (one of `AV_MULTITRACK_TYPE_*`).
    /// Reserved values (3..=15) round-trip verbatim — the parser does not
    /// reject them, so a forwarding ingest preserves unknown future modes.
    pub multitrack_type: u8,
    /// Decoded per-track entries in stream order. Always at least 1 entry
    /// after a successful parse.
    pub tracks: Vec<MultitrackTrack>,
}

/// One track inside a [`Multitrack`] body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MultitrackTrack {
    /// Per-track codec FourCC. `Some(..)` only when the surrounding
    /// [`Multitrack::multitrack_type`] is `ManyTracksManyCodecs` — the
    /// `OneTrack` / `ManyTracks` modes carry a shared FourCC on the outer
    /// tag (see [`VideoTag::fourcc`] / [`AudioTag::audio_fourcc`]) and this
    /// field is `None`. Set to `Some(..)` on build to opt into the
    /// many-codecs layout for this track.
    pub fourcc: Option<[u8; 4]>,
    /// `trackId = UI8`. Per spec, trackId 0 is the default track described
    /// by the top-level onMetaData; additional tracks use positive ids
    /// (1, 2, 3, …). Values are identifiers only and do not imply ordering.
    pub track_id: u8,
    /// Codec payload for this track (the shape the real PacketType + FourCC
    /// would produce as a single-track Enhanced-RTMP body). Empty for
    /// SequenceEnd tracks per spec.
    pub body: Vec<u8>,
}

impl Multitrack {
    /// Parse the multitrack track-list bytes (everything in
    /// [`VideoTag::body`] / [`AudioTag::body`] after [`parse_video`] /
    /// [`parse_audio`] stripped the per-tag header) given the outer
    /// `multitrack_type`. Returns `Err(Error::Other)` on truncation or on
    /// a track whose `sizeOfTrack` UI24 overruns the buffer.
    ///
    /// `OneTrack` mode produces exactly one track whose body runs to the
    /// end of the buffer. `ManyTracks` and `ManyTracksManyCodecs` modes
    /// loop while bytes remain, consuming a UI24 `sizeOfTrack` per track.
    pub fn parse(body: &[u8], multitrack_type: u8) -> Result<Multitrack> {
        let many_codecs = multitrack_type == AV_MULTITRACK_TYPE_MANY_TRACKS_MANY_CODECS;
        let one_track = multitrack_type == AV_MULTITRACK_TYPE_ONE_TRACK;
        let mut pos = 0usize;
        let mut tracks = Vec::new();
        loop {
            if pos >= body.len() {
                if tracks.is_empty() {
                    return Err(Error::Other(
                        "Multitrack: empty track list (need at least one track)".into(),
                    ));
                }
                break;
            }
            let track_fourcc = if many_codecs {
                if pos + 4 > body.len() {
                    return Err(Error::Other(
                        "Multitrack: truncated reading per-track FourCC".into(),
                    ));
                }
                let mut fcc = [0u8; 4];
                fcc.copy_from_slice(&body[pos..pos + 4]);
                pos += 4;
                Some(fcc)
            } else {
                None
            };
            if pos >= body.len() {
                return Err(Error::Other("Multitrack: truncated reading trackId".into()));
            }
            let track_id = body[pos];
            pos += 1;
            let track_body = if one_track {
                // OneTrack: no size field, body runs to end of buffer.
                let rest = body[pos..].to_vec();
                pos = body.len();
                rest
            } else {
                if pos + 3 > body.len() {
                    return Err(Error::Other(
                        "Multitrack: truncated reading sizeOfTrack UI24".into(),
                    ));
                }
                let size = ((body[pos] as usize) << 16)
                    | ((body[pos + 1] as usize) << 8)
                    | (body[pos + 2] as usize);
                pos += 3;
                if pos + size > body.len() {
                    return Err(Error::Other(format!(
                        "Multitrack: sizeOfTrack={size} overruns remaining {} bytes",
                        body.len() - pos
                    )));
                }
                let slice = body[pos..pos + size].to_vec();
                pos += size;
                slice
            };
            tracks.push(MultitrackTrack {
                fourcc: track_fourcc,
                track_id,
                body: track_body,
            });
            if one_track {
                break;
            }
        }
        Ok(Multitrack {
            multitrack_type,
            tracks,
        })
    }

    /// Serialise to the byte layout `parse` consumes. Output goes into the
    /// tag's [`VideoTag::body`] / [`AudioTag::body`] slot when building an
    /// outgoing multitrack message.
    ///
    /// For `OneTrack` mode only the first track's `track_id` + `body` are
    /// emitted (the second-and-beyond tracks are silently ignored — the
    /// caller is responsible for using `ManyTracks` if it has more than
    /// one). For `ManyTracksManyCodecs` each track's `fourcc` MUST be
    /// `Some(..)`; a `None` is encoded as four zero bytes to keep the
    /// output decodable but the caller should treat that as a bug.
    pub fn encode(&self) -> Vec<u8> {
        let many_codecs = self.multitrack_type == AV_MULTITRACK_TYPE_MANY_TRACKS_MANY_CODECS;
        let one_track = self.multitrack_type == AV_MULTITRACK_TYPE_ONE_TRACK;
        let mut out = Vec::new();
        for (i, track) in self.tracks.iter().enumerate() {
            if one_track && i > 0 {
                break;
            }
            if many_codecs {
                let fcc = track.fourcc.unwrap_or([0; 4]);
                out.extend_from_slice(&fcc);
            }
            out.push(track.track_id);
            if !one_track {
                let size = track.body.len() & 0x00FF_FFFF;
                out.extend_from_slice(&[(size >> 16) as u8, (size >> 8) as u8, size as u8]);
            }
            out.extend_from_slice(&track.body);
        }
        out
    }
}

/// Validation plan shared by [`VideoTag::multitrack_from_tags`] /
/// [`AudioTag::multitrack_from_tags`].
struct MultitrackPlan {
    inner_packet_type: u8,
    shared_fourcc: Option<[u8; 4]>,
    per_track_fourcc: bool,
}

/// Validate a per-track tag list against the §"ExVideoTagHeader" /
/// §"ExAudioTagHeader" multitrack invariants (one inner PacketType per
/// message, shared-vs-per-track FourCC by mode, `OneTrack` = exactly one
/// track) and derive the outer-tag fields. Generic over the audio/video
/// tag type via accessors so both pipelines share one rule set.
#[allow(clippy::too_many_arguments)]
fn plan_multitrack<T>(
    multitrack_type: u8,
    tracks: &[(u8, &T)],
    fourcc: impl Fn(&T) -> Option<[u8; 4]>,
    ex_packet_type: impl Fn(&T) -> Option<u8>,
    is_multitrack: impl Fn(&T) -> bool,
    has_mod_ex: impl Fn(&T) -> bool,
    pt_multitrack: u8,
    pt_mod_ex: u8,
    kind: &str,
) -> Result<MultitrackPlan> {
    if tracks.is_empty() {
        return Err(Error::Other(format!(
            "multitrack_from_tags ({kind}): need at least one track"
        )));
    }
    let one_track = multitrack_type == AV_MULTITRACK_TYPE_ONE_TRACK;
    let many_codecs = multitrack_type == AV_MULTITRACK_TYPE_MANY_TRACKS_MANY_CODECS;
    if one_track && tracks.len() != 1 {
        return Err(Error::Other(format!(
            "multitrack_from_tags ({kind}): OneTrack mode carries exactly one track, got {}",
            tracks.len()
        )));
    }
    let inner_pt = ex_packet_type(tracks[0].1).ok_or_else(|| {
        Error::Other(format!(
            "multitrack_from_tags ({kind}): track {} is not an Enhanced-RTMP tag (no PacketType)",
            tracks[0].0
        ))
    })?;
    if inner_pt == pt_multitrack || inner_pt == pt_mod_ex {
        return Err(Error::Other(format!(
            "multitrack_from_tags ({kind}): inner PacketType {inner_pt} is reserved (Multitrack / ModEx cannot nest)"
        )));
    }
    for (id, tag) in tracks {
        if is_multitrack(tag) {
            return Err(Error::Other(format!(
                "multitrack_from_tags ({kind}): track {id} is itself a Multitrack tag"
            )));
        }
        if has_mod_ex(tag) {
            return Err(Error::Other(format!(
                "multitrack_from_tags ({kind}): track {id} carries a ModEx prelude (message-level ModEx belongs on the outer tag)"
            )));
        }
        if ex_packet_type(tag) != Some(inner_pt) {
            return Err(Error::Other(format!(
                "multitrack_from_tags ({kind}): track {id} PacketType differs (the wire carries one inner PacketType per message)"
            )));
        }
        if fourcc(tag).is_none() {
            return Err(Error::Other(format!(
                "multitrack_from_tags ({kind}): track {id} is not an Enhanced-RTMP tag (no FourCC)"
            )));
        }
    }
    let shared_fourcc = if many_codecs {
        None
    } else {
        let fcc = fourcc(tracks[0].1);
        if let Some((id, _)) = tracks.iter().find(|(_, t)| fourcc(t) != fcc) {
            return Err(Error::Other(format!(
                "multitrack_from_tags ({kind}): track {id} FourCC differs — OneTrack / ManyTracks share one codec; use ManyTracksManyCodecs"
            )));
        }
        fcc
    };
    Ok(MultitrackPlan {
        inner_packet_type: inner_pt,
        shared_fourcc,
        per_track_fourcc: many_codecs,
    })
}

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
    /// Enhanced RTMP v2 ModEx prelude chain
    /// (`enhanced-rtmp-v2.pdf` §"ExVideoTagHeader"). Empty for
    /// legacy tags and for Enhanced tags that carry no modifier.
    /// Each entry was a `PacketType.ModEx` step before the real
    /// [`ex_packet_type`][VideoTag::ex_packet_type] was decoded;
    /// the chain is re-emitted verbatim ahead of the real packet
    /// type on build. The only subtype defined today is
    /// `TimestampOffsetNano` (high-precision sub-millisecond
    /// presentation offset).
    pub mod_ex: Vec<ModEx>,
    /// Enhanced RTMP v2 `Multitrack` body (per-track FourCC + trackId +
    /// sizeOfVideoTrack chain — see [`Multitrack`]). `Some(..)` only when
    /// the wire PacketType nibble was `Multitrack = 6`; in that case
    /// [`ex_packet_type`][VideoTag::ex_packet_type] holds the *real* inner
    /// PacketType (e.g. `CodedFrames`, `SequenceStart`),
    /// [`fourcc`][VideoTag::fourcc] holds the shared codec FourCC when the
    /// multitrack mode is `OneTrack` / `ManyTracks` (and `None` for
    /// `ManyTracksManyCodecs`), and the tag's [`body`][VideoTag::body] is
    /// empty (track payloads sit in each [`MultitrackTrack::body`]).
    pub multitrack: Option<Multitrack>,
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

    /// True when this tag is the FourCC-mode
    /// `VideoPacketType.MPEG2TSSequenceStart` (= 5) — the
    /// MPEG-2 TS carriage sequence-start variant
    /// (`enhanced-rtmp-v2.pdf` §"ExVideoTagBody"). Per spec
    /// `PacketTypeSequenceStart` and `PacketTypeMPEG2TSSequenceStart`
    /// are mutually exclusive: this one signals that the codec
    /// bitstream is carried in MPEG-2 TS format. The only FourCc the
    /// spec defines a body for so far is `av01`, whose
    /// [`body`][VideoTag::body] is an `AV1VideoDescriptor`. Lift the
    /// descriptor bytes via [`VideoTag::mpeg2ts_video_descriptor`].
    pub fn is_ex_mpeg2ts_sequence_start(&self) -> bool {
        self.fourcc.is_some() && self.ex_packet_type == Some(EX_PACKET_TYPE_MPEG2TS_SEQUENCE_START)
    }

    /// Borrow the MPEG-2 TS sequence-start descriptor body when this
    /// tag is a [`MPEG2TSSequenceStart`][Self::is_ex_mpeg2ts_sequence_start]
    /// for `av01` (the body is an `AV1VideoDescriptor` per
    /// `enhanced-rtmp-v2.pdf` §"ExVideoTagBody"). Returns `None` for
    /// any other tag — including an MPEG2TSSequenceStart for a FourCc
    /// the spec has not yet assigned a descriptor body, so a caller
    /// only acts on a descriptor it can interpret.
    pub fn mpeg2ts_video_descriptor(&self) -> Option<&[u8]> {
        if self.is_ex_mpeg2ts_sequence_start() && self.fourcc == Some(FOURCC_AV1) {
            Some(&self.body)
        } else {
            None
        }
    }

    /// Build a FourCC-mode `VideoPacketType.MPEG2TSSequenceStart` tag
    /// (`enhanced-rtmp-v2.pdf` §"ExVideoTagBody") carrying the given
    /// descriptor `body` for `av01` (an `AV1VideoDescriptor`). The
    /// FrameType is stamped [`VIDEO_FRAME_KEYFRAME`] (a sequence start
    /// begins a decodable run); no SI24 CTS is emitted (only
    /// `CodedFrames` for NALU FourCCs carries one). Round-trips through
    /// [`build_video`] / [`parse_video`] back to the same body and
    /// [`is_ex_mpeg2ts_sequence_start`][Self::is_ex_mpeg2ts_sequence_start].
    pub fn mpeg2ts_sequence_start_tag(fourcc: [u8; 4], body: Vec<u8>) -> VideoTag {
        VideoTag {
            frame_type: VIDEO_FRAME_KEYFRAME,
            codec_id: 0,
            avc_packet_type: None,
            composition_time: 0,
            body,
            ex_packet_type: Some(EX_PACKET_TYPE_MPEG2TS_SEQUENCE_START),
            fourcc: Some(fourcc),
            mod_ex: Vec::new(),
            multitrack: None,
        }
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

    /// True when this tag is the FourCC-mode
    /// `VideoPacketType.SequenceEnd` (= 2) — signals the end of the
    /// coded video sequence (`enhanced-rtmp-v2.pdf` §"ExVideoTagBody":
    /// "signals end of sequence"). The body is empty.
    pub fn is_ex_sequence_end(&self) -> bool {
        self.fourcc.is_some() && self.ex_packet_type == Some(EX_PACKET_TYPE_SEQUENCE_END)
    }

    /// Build a FourCC-mode `VideoPacketType.SequenceEnd` tag for the
    /// given codec FourCC (`enhanced-rtmp-v2.pdf` §"ExVideoTagBody").
    /// The body is empty and the FrameType is stamped
    /// [`VIDEO_FRAME_INTER`] (a sequence-end packet carries no
    /// decodable picture). Round-trips through [`build_video`] /
    /// [`parse_video`] back to the same
    /// [`is_ex_sequence_end`][Self::is_ex_sequence_end].
    pub fn sequence_end_tag(fourcc: [u8; 4]) -> VideoTag {
        VideoTag {
            frame_type: VIDEO_FRAME_INTER,
            codec_id: 0,
            avc_packet_type: None,
            composition_time: 0,
            body: Vec::new(),
            ex_packet_type: Some(EX_PACKET_TYPE_SEQUENCE_END),
            fourcc: Some(fourcc),
            mod_ex: Vec::new(),
            multitrack: None,
        }
    }

    /// True when this tag is a `VideoFrameType.Command` frame: the
    /// FrameType nibble is [`VIDEO_FRAME_COMMAND`] (= 5) and, in
    /// Enhanced mode, the PacketType is *not* `Metadata` (which also
    /// uses FrameType 5 = `Info` but carries an AMF body rather than a
    /// command byte). Such a tag carries no coded video — only a single
    /// [`VideoCommand`][Self::video_command] byte.
    ///
    /// (`video_file_format_spec_v10_1.pdf` §E.4.3.1 "VideoTagBody" /
    /// `enhanced-rtmp-v2.pdf` §"ExVideoTagHeader".)
    pub fn is_command(&self) -> bool {
        self.frame_type == VIDEO_FRAME_COMMAND
            && self.ex_packet_type != Some(EX_PACKET_TYPE_METADATA)
    }

    /// Decode the `videoCommand = UI8` carried by a
    /// [`VideoFrameType.Command`][Self::is_command] tag.
    ///
    /// Both the legacy (`video_file_format_spec_v10_1.pdf` §E.4.3.1)
    /// and Enhanced-RTMP (`enhanced-rtmp-v2.pdf` §"ExVideoTagHeader")
    /// framings place the command in the first body byte once the
    /// header (legacy `frame_type|codec_id`, or Enhanced
    /// header + FourCC) has been consumed. Returns:
    ///
    /// * `None` when this is not a command tag (per [`Self::is_command`]),
    ///   or the body is empty (truncated wire form).
    /// * `Some(cmd)` — one of [`VIDEO_COMMAND_START_SEEK`] /
    ///   [`VIDEO_COMMAND_END_SEEK`], or a reserved value passed through
    ///   verbatim ("if a value in the bitstream is not understood, the
    ///   logic must fail gracefully" — we surface the raw byte rather
    ///   than reject it, so callers ignore unknown commands).
    pub fn video_command(&self) -> Option<u8> {
        if !self.is_command() {
            return None;
        }
        self.body.first().copied()
    }

    /// Build a legacy `VideoFrameType.Command` (= info/command) tag
    /// carrying a single `videoCommand` byte
    /// (`video_file_format_spec_v10_1.pdf` §E.4.3.1). The legacy
    /// command byte's seek meaning is codec-independent, but the
    /// spec's `CodecID` nibble is still present in the header; pass the
    /// stream's video codec id (e.g. [`VIDEO_CODEC_AVC`]).
    ///
    /// The produced tag has `frame_type = VIDEO_FRAME_COMMAND`, the
    /// given `codec_id`, no FourCC / Enhanced framing, and
    /// `body = [command]` so [`build_video`] emits the
    /// `frame_type|codec_id` byte followed by the single command byte.
    pub fn command_tag(codec_id: u8, command: u8) -> VideoTag {
        VideoTag {
            frame_type: VIDEO_FRAME_COMMAND,
            codec_id,
            avc_packet_type: None,
            composition_time: 0,
            body: vec![command],
            ex_packet_type: None,
            fourcc: None,
            mod_ex: Vec::new(),
            multitrack: None,
        }
    }

    /// Build an Enhanced-RTMP `VideoFrameType.Command` tag carrying a
    /// single `videoCommand` byte for the given codec FourCC
    /// (`enhanced-rtmp-v2.pdf` §"ExVideoTagHeader": the
    /// `videoPacketType != Metadata && videoFrameType == Command`
    /// branch — the command byte follows the FourCC and the body has no
    /// further payload).
    ///
    /// The `ex_packet_type` is stamped `CodedFrames` so the FrameType
    /// nibble (`Command`) is the sole command signal; the SI24 CTS is
    /// *not* emitted for a command tag (see [`build_video`]). The
    /// produced tag round-trips through [`build_video`] /
    /// [`parse_video`] back to the same [`VideoTag::video_command`].
    pub fn command_tag_ex(fourcc: [u8; 4], command: u8) -> VideoTag {
        VideoTag {
            frame_type: VIDEO_FRAME_COMMAND,
            codec_id: 0,
            avc_packet_type: None,
            composition_time: 0,
            body: vec![command],
            ex_packet_type: Some(EX_PACKET_TYPE_CODED_FRAMES),
            fourcc: Some(fourcc),
            mod_ex: Vec::new(),
            multitrack: None,
        }
    }

    /// Lift the `"colorInfo"` HDR metadata out of a
    /// `VideoPacketType.Metadata` tag into the strongly-typed [`ColorInfo`]
    /// view (Enhanced RTMP §"Metadata Frame").
    ///
    /// The metadata [`body`][VideoTag::body] is an AMF-encoded sequence of
    /// `[name, value]` pairs; this scans for the `"colorInfo"` name and
    /// decodes the following value. Returns:
    ///
    /// * `Ok(None)` when this is not a metadata tag, or no `"colorInfo"`
    ///   pair is present (the spec leaves room for other future names).
    /// * `Ok(Some(ColorInfo))` for a decoded `colorInfo` value, including
    ///   the reset signal (`Undefined`/`{}` → [`ColorInfo::is_reset`]).
    /// * `Err(..)` when the AMF body is malformed or the `colorInfo` value
    ///   is the wrong AMF type.
    pub fn color_info(&self) -> Result<Option<ColorInfo>> {
        if !self.is_ex_metadata() {
            return Ok(None);
        }
        let values = amf::decode_all(&self.body)?;
        // The body is a flat `name, value, name, value, …` stream. Find the
        // "colorInfo" name string and decode the value that follows it.
        let mut i = 0;
        while i + 1 < values.len() {
            if values[i].as_str() == Some("colorInfo") {
                return ColorInfo::from_amf0(&values[i + 1]).map(Some);
            }
            i += 2;
        }
        Ok(None)
    }

    /// Build a `VideoPacketType.Metadata` tag carrying a single
    /// `["colorInfo", value]` pair for the given codec FourCC (Enhanced RTMP
    /// §"Metadata Frame"). The `FrameType` flags are ignored by spec for a
    /// metadata packet, so this stamps the conventional `Info` (5) value.
    ///
    /// The inverse of [`VideoTag::color_info`]: the produced tag's
    /// [`body`][VideoTag::body] is `encode("colorInfo") ++ encode(value)`,
    /// where a [reset][ColorInfo::is_reset] `ColorInfo` encodes the value as
    /// `Undefined`.
    pub fn color_info_tag(fourcc: [u8; 4], color_info: &ColorInfo) -> VideoTag {
        let mut body = Vec::new();
        amf::encode(&mut body, &Amf0Value::String("colorInfo".into()));
        amf::encode(&mut body, &color_info.to_amf0());
        VideoTag {
            frame_type: VIDEO_FRAME_INFO,
            codec_id: 0,
            avc_packet_type: None,
            composition_time: 0,
            body,
            ex_packet_type: Some(EX_PACKET_TYPE_METADATA),
            fourcc: Some(fourcc),
            mod_ex: Vec::new(),
            multitrack: None,
        }
    }

    /// Sum of the `TimestampOffsetNano` ModEx entries on this tag, in
    /// nanoseconds. Per `enhanced-rtmp-v2.pdf` the offset is added to
    /// the current media message's presentation time without altering
    /// the core RTMP millisecond timestamp. Returns `0` when no such
    /// entry is present.
    pub fn timestamp_offset_nano(&self) -> u32 {
        self.mod_ex
            .iter()
            .filter_map(ModEx::timestamp_offset_nano)
            .fold(0u32, |acc, n| acc.saturating_add(n))
    }

    /// True when this tag is an Enhanced-RTMP v2 video `Multitrack`
    /// message (the wire PacketType nibble was `Multitrack = 6` and
    /// [`Self::multitrack`] decoded the per-track body).
    pub fn is_multitrack(&self) -> bool {
        self.multitrack.is_some()
    }

    /// Build an Enhanced-RTMP v2 video `Multitrack` tag with the given
    /// FrameType, real inner PacketType, shared FourCC (when the multitrack
    /// mode is `OneTrack` / `ManyTracks`; pass `None` for
    /// `ManyTracksManyCodecs`), and per-track body. The returned tag has
    /// `ex_packet_type = real_packet_type`, `fourcc = shared_fourcc`,
    /// `multitrack = Some(mt)`, and `body` empty. ModEx prelude is empty.
    pub fn multitrack_tag(
        frame_type: u8,
        real_packet_type: u8,
        shared_fourcc: Option<[u8; 4]>,
        mt: Multitrack,
    ) -> VideoTag {
        VideoTag {
            frame_type,
            codec_id: 0,
            avc_packet_type: None,
            composition_time: 0,
            body: Vec::new(),
            ex_packet_type: Some(real_packet_type),
            fourcc: shared_fourcc,
            mod_ex: Vec::new(),
            multitrack: Some(mt),
        }
    }

    /// Demultiplex an Enhanced-RTMP v2 video `Multitrack` tag into
    /// standalone per-track [`VideoTag`]s (enhanced-rtmp-v2.pdf
    /// §"Multitrack Streaming via Enhanced RTMP" / §"ExVideoTagBody").
    ///
    /// Each returned `(trackId, tag)` pair is the single-track tag the
    /// same frame would have produced had it been published alone: the
    /// per-track FourCC (the shared one for `OneTrack` / `ManyTracks`,
    /// the track's own for `ManyTracksManyCodecs`), the message's real
    /// inner PacketType, the outer FrameType, and the track body decoded
    /// through the ordinary [`parse_video`] rules — in particular the
    /// per-track SI24 `compositionTimeOffset` that `CodedFrames` carries
    /// for the NALU FourCCs (`hvc1` / `avc1` / `vvc1`) is lifted into
    /// [`VideoTag::composition_time`], exactly as the spec's
    /// `ExVideoTagBody` loop reads it once per track.
    ///
    /// The outer tag's ModEx prelude is *not* copied onto the per-track
    /// tags: per §"ExVideoTagHeader" a ModEx entry (e.g.
    /// `TimestampOffsetNano`) modifies the whole message, so it stays a
    /// property of the outer tag ([`VideoTag::timestamp_offset_nano`]).
    ///
    /// Errors: the tag is not multitrack, a `ManyTracksManyCodecs` track
    /// is missing its FourCC, or a track body fails the per-track parse
    /// (e.g. a truncated SI24 CTS).
    pub fn demux_tracks(&self) -> Result<Vec<(u8, VideoTag)>> {
        let mt = self.multitrack.as_ref().ok_or_else(|| {
            Error::Other("VideoTag::demux_tracks: tag is not a Multitrack message".into())
        })?;
        let inner_pt = self.ex_packet_type.ok_or_else(|| {
            Error::Other("VideoTag::demux_tracks: multitrack tag lacks inner PacketType".into())
        })?;
        let mut out = Vec::with_capacity(mt.tracks.len());
        for track in &mt.tracks {
            let fcc = track.fourcc.or(self.fourcc).ok_or_else(|| {
                Error::Other(format!(
                    "VideoTag::demux_tracks: track {} has no FourCC (neither per-track nor shared)",
                    track.track_id
                ))
            })?;
            // Synthesize the single-track wire payload and reuse
            // `parse_video` so the per-track body follows the exact
            // §"ExVideoTagBody" shape (CTS, Metadata, Command, …) with
            // one source of truth.
            let mut payload = Vec::with_capacity(5 + track.body.len());
            payload.push(VIDEO_IS_EX_HEADER | ((self.frame_type & 0x07) << 4) | (inner_pt & 0x0F));
            payload.extend_from_slice(&fcc);
            payload.extend_from_slice(&track.body);
            let tag = parse_video(&payload).map_err(|e| {
                Error::Other(format!(
                    "VideoTag::demux_tracks: track {} body failed to parse: {e}",
                    track.track_id
                ))
            })?;
            out.push((track.track_id, tag));
        }
        Ok(out)
    }

    /// Multiplex standalone single-track Enhanced-RTMP [`VideoTag`]s
    /// into one v2 `Multitrack` tag — the inverse of
    /// [`VideoTag::demux_tracks`].
    ///
    /// `multitrack_type` is one of the `AV_MULTITRACK_TYPE_*` modes.
    /// Every input tag must be an Enhanced-RTMP tag (`fourcc` +
    /// `ex_packet_type` set), non-multitrack, with an empty ModEx
    /// prelude (message-level ModEx belongs on the returned outer tag —
    /// set [`VideoTag::mod_ex`] afterwards); all tags must share the
    /// same inner PacketType and FrameType (the wire carries one of
    /// each per message, per §"ExVideoTagHeader"), and the inner
    /// PacketType must not be `Multitrack` or `ModEx`. `OneTrack`
    /// requires exactly one track; `OneTrack` / `ManyTracks` require a
    /// single shared FourCC across all tracks, while
    /// `ManyTracksManyCodecs` stamps each track's own FourCC into the
    /// track list.
    pub fn multitrack_from_tags(
        multitrack_type: u8,
        tracks: &[(u8, &VideoTag)],
    ) -> Result<VideoTag> {
        let plan = plan_multitrack(
            multitrack_type,
            tracks,
            |t| t.fourcc,
            |t| t.ex_packet_type,
            |t| t.multitrack.is_some(),
            |t| !t.mod_ex.is_empty(),
            EX_PACKET_TYPE_MULTITRACK,
            EX_PACKET_TYPE_MOD_EX,
            "video",
        )?;
        let frame_type = tracks[0].1.frame_type;
        if let Some((id, _)) = tracks.iter().find(|(_, t)| t.frame_type != frame_type) {
            return Err(Error::Other(format!(
                "multitrack_from_tags: track {id} FrameType differs (the outer header carries one FrameType per message)"
            )));
        }
        let mut mt_tracks = Vec::with_capacity(tracks.len());
        for (track_id, tag) in tracks {
            // `build_video` emits `[head(1)][FourCC(4)][CTS?][body]` for a
            // non-multitrack Enhanced tag with no ModEx; stripping the
            // 5-byte prefix leaves exactly the §"ExVideoTagBody" per-track
            // body (including the SI24 CTS where the FourCC × PacketType
            // pair carries one).
            let wire = build_video(tag);
            mt_tracks.push(MultitrackTrack {
                fourcc: if plan.per_track_fourcc {
                    tag.fourcc
                } else {
                    None
                },
                track_id: *track_id,
                body: wire[5..].to_vec(),
            });
        }
        Ok(VideoTag::multitrack_tag(
            frame_type,
            plan.inner_packet_type,
            plan.shared_fourcc,
            Multitrack {
                multitrack_type,
                tracks: mt_tracks,
            },
        ))
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
        // --- Enhanced RTMP v1/v2 framing ---
        //
        //   byte 0      = IsExHeader(1) | FrameType(3) | PacketType(4)
        //   [ModEx prelude chain — present only when PacketType == ModEx]
        //   byte ..=+3  = FourCC (4 ASCII bytes)
        //   byte ..     = body, with shape depending on FourCC × PacketType
        //
        // Per spec, when PacketType == Metadata the FrameType
        // flags above the nibble are required to be ignored;
        // we still preserve the raw bits in `frame_type` so
        // callers that diff fixtures can see them.
        let frame_type = (b0 >> 4) & 0b0111;
        let mut packet_type = b0 & 0x0F;
        let mut pos = 1;

        // ModEx prelude (enhanced-rtmp-v2.pdf §"ExVideoTagHeader"):
        // while the freshly-read PacketType nibble is ModEx, consume
        // a size-prefixed modExData entry + the trailing
        // modExType/packetType nibble byte, looping until a non-ModEx
        // PacketType terminates the chain. The chain sits between the
        // header byte and the FourCC.
        let mut mod_ex = Vec::new();
        if packet_type == EX_PACKET_TYPE_MOD_EX {
            let (chain, real_pt, next) =
                parse_mod_ex_chain(payload, pos, EX_PACKET_TYPE_MOD_EX, "video")?;
            mod_ex = chain;
            packet_type = real_pt;
            pos = next;
        }

        // Multitrack prelude (enhanced-rtmp-v2.pdf §"ExVideoTagHeader"):
        // a Multitrack PacketType pulls in a `multitrackType (UB[4]) |
        // realPacketType (UB[4])` byte and, when the multitrack mode is
        // not ManyTracksManyCodecs, a shared FourCC. The body (the
        // per-track list) is decoded later via `Multitrack::parse`.
        let mut multitrack_type: Option<u8> = None;
        if packet_type == EX_PACKET_TYPE_MULTITRACK {
            if pos >= payload.len() {
                return Err(Error::Other(
                    "Enhanced RTMP video Multitrack: truncated reading multitrackType nibble"
                        .into(),
                ));
            }
            let nibble = payload[pos];
            pos += 1;
            let mt_type = (nibble >> 4) & 0x0F;
            let inner_pt = nibble & 0x0F;
            // Spec: "This fetch MUST not result in a VideoPacketType.Multitrack"
            if inner_pt == EX_PACKET_TYPE_MULTITRACK {
                return Err(Error::Other(
                    "Enhanced RTMP video Multitrack: inner PacketType MUST NOT be Multitrack"
                        .into(),
                ));
            }
            multitrack_type = Some(mt_type);
            packet_type = inner_pt;
        }

        // For Multitrack ManyTracksManyCodecs there is no shared FourCC
        // before the per-track loop; for OneTrack / ManyTracks the shared
        // FourCC sits here (per spec). For non-Multitrack tags the FourCC
        // always sits here.
        let need_shared_fourcc = match multitrack_type {
            Some(t) => t != AV_MULTITRACK_TYPE_MANY_TRACKS_MANY_CODECS,
            None => true,
        };
        let fcc_opt = if need_shared_fourcc {
            if pos + 4 > payload.len() {
                return Err(Error::Other(
                    "Enhanced RTMP video tag: need 4 bytes for FourCC after header/ModEx".into(),
                ));
            }
            let mut fcc = [0u8; 4];
            fcc.copy_from_slice(&payload[pos..pos + 4]);
            pos += 4;
            Some(fcc)
        } else {
            None
        };
        // Keep a non-Option fcc for the non-Multitrack branches below
        // (preserves the pre-change shape of the rest of the function).
        let fcc = fcc_opt.unwrap_or([0; 4]);

        // Multitrack tags: SI24 CompositionTime lives inside each
        // per-track body (a track is itself an Enhanced-RTMP video
        // body), so the outer parser only consumes the track list.
        if let Some(mt_type) = multitrack_type {
            let mt = Multitrack::parse(&payload[pos..], mt_type)?;
            return Ok(VideoTag {
                frame_type,
                codec_id: 0,
                avc_packet_type: None,
                composition_time: 0,
                body: Vec::new(),
                ex_packet_type: Some(packet_type),
                fourcc: fcc_opt,
                mod_ex,
                multitrack: Some(mt),
            });
        }

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
        // A `VideoFrameType.Command` tag carries no coded video — the
        // single `videoCommand = UI8` byte follows the FourCC and the
        // spec sets `processVideoBody = false`, so the CodedFrames body
        // path (including the SI24 CTS read) is never reached. Skip the
        // CTS so the command byte stays at `body[0]`
        // (enhanced-rtmp-v2.pdf §"ExVideoTagHeader": the
        // `videoPacketType != Metadata && videoFrameType == Command`
        // branch precedes the ExVideoTagBody loop).
        let is_command =
            frame_type == VIDEO_FRAME_COMMAND && packet_type != EX_PACKET_TYPE_METADATA;
        let needs_cts = !is_command
            && packet_type == EX_PACKET_TYPE_CODED_FRAMES
            && (fcc == FOURCC_HEVC || fcc == FOURCC_AVC || fcc == FOURCC_VVC);
        let (cts, body_start) = if needs_cts {
            if pos + 3 > payload.len() {
                return Err(Error::Other(
                    "Enhanced RTMP / HEVC CodedFrames: need 3 bytes for SI24 CTS".into(),
                ));
            }
            let raw = ((payload[pos] as i32) << 16)
                | ((payload[pos + 1] as i32) << 8)
                | (payload[pos + 2] as i32);
            (sign_extend_si24(raw), pos + 3)
        } else {
            (0, pos)
        };

        Ok(VideoTag {
            frame_type,
            codec_id: 0, // reserved in extended mode; legacy nibble unused.
            avc_packet_type: None,
            composition_time: cts,
            body: payload[body_start..].to_vec(),
            ex_packet_type: Some(packet_type),
            fourcc: Some(fcc),
            mod_ex,
            multitrack: None,
        })
    } else {
        // --- Legacy pre-2023 framing ---
        let frame_type = b0 >> 4;
        let codec_id = b0 & 0x0F;
        if frame_type == VIDEO_FRAME_COMMAND {
            // `video_file_format_spec_v10_1.pdf` §E.4.3.1: when
            // FrameType == 5 the VideoTagBody is a single `UI8`
            // command (StartSeek / EndSeek) for *every* CodecID — the
            // AVC packet-type + SI24 CTS prefix is absent even when
            // CodecID == 7. Keep the command byte in `body[0]` so
            // [`VideoTag::video_command`] lifts it.
            Ok(VideoTag {
                frame_type,
                codec_id,
                avc_packet_type: None,
                composition_time: 0,
                body: payload[1..].to_vec(),
                ex_packet_type: None,
                fourcc: None,
                mod_ex: Vec::new(),
                multitrack: None,
            })
        } else if codec_id == VIDEO_CODEC_AVC {
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
                mod_ex: Vec::new(),
                multitrack: None,
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
                mod_ex: Vec::new(),
                multitrack: None,
            })
        }
    }
}

/// Build an RTMP video-tag payload.
///
/// Legacy mode (`tag.fourcc.is_none()` and `tag.multitrack.is_none()`):
/// writes the 1-byte frame/codec header + optional AVC packet type +
/// 3-byte composition time, then `body`.
///
/// Enhanced RTMP mode (`tag.fourcc = Some([..])` *or*
/// `tag.multitrack = Some(..)` for ManyTracksManyCodecs): writes the
/// `IsExHeader | frame_type | packet_type` byte, optionally a
/// `multitrackType | realPacketType` byte for Multitrack tags, the
/// 4-byte FourCC (omitted for Multitrack ManyTracksManyCodecs), the
/// SI24 CTS *only* when FourCC ∈ {HEVC, AVC, VVC} and
/// PacketType == CodedFrames on a non-Multitrack tag, then `body`
/// (or the encoded track list for Multitrack tags).
pub fn build_video(tag: &VideoTag) -> Vec<u8> {
    if tag.fourcc.is_some() || tag.multitrack.is_some() {
        let real_packet_type = tag.ex_packet_type.unwrap_or(EX_PACKET_TYPE_CODED_FRAMES);
        let multitrack_outer_pt = if tag.multitrack.is_some() {
            Some(EX_PACKET_TYPE_MULTITRACK)
        } else {
            None
        };
        // The packet type that sits in the byte *after* the ModEx chain
        // (or the header byte itself when no ModEx is present): Multitrack
        // for a multitrack tag, the real packet type otherwise.
        let post_mod_ex_pt = multitrack_outer_pt.unwrap_or(real_packet_type);
        // When a ModEx prelude is present the header byte's PacketType
        // nibble is `ModEx`; the next packet type is carried by the
        // terminating nibble of the chain
        // (enhanced-rtmp-v2.pdf §"ExVideoTagHeader"). Otherwise the
        // header nibble is `post_mod_ex_pt` directly.
        let header_pt = if tag.mod_ex.is_empty() {
            post_mod_ex_pt
        } else {
            EX_PACKET_TYPE_MOD_EX
        };
        // Per Enhanced RTMP §"Defining Additional Video Codecs"
        // FrameType is UB[3] (i.e. lives in bits 4..=6 — bit 7
        // is IsExHeader). Mask to 3 bits before packing.
        let head = VIDEO_IS_EX_HEADER | ((tag.frame_type & 0x07) << 4) | (header_pt & 0x0F);
        let mut out = Vec::with_capacity(tag.body.len() + 8);
        out.push(head);
        build_mod_ex_chain(&mut out, &tag.mod_ex, EX_PACKET_TYPE_MOD_EX, post_mod_ex_pt);
        if let Some(mt) = &tag.multitrack {
            // Multitrack nibble byte: `multitrackType (UB[4]) |
            // realPacketType (UB[4])`.
            out.push(((mt.multitrack_type & 0x0F) << 4) | (real_packet_type & 0x0F));
            // Shared FourCC sits here unless the mode is ManyTracksManyCodecs.
            if mt.multitrack_type != AV_MULTITRACK_TYPE_MANY_TRACKS_MANY_CODECS {
                let fcc = tag.fourcc.unwrap_or([0; 4]);
                out.extend_from_slice(&fcc);
            }
            out.extend_from_slice(&mt.encode());
            return out;
        }
        // Non-multitrack: FourCC always sits here. `tag.fourcc` is Some
        // by the outer `if` (the multitrack branch above already returned).
        let fcc = tag
            .fourcc
            .expect("Enhanced-RTMP non-Multitrack tag requires fourcc");
        out.extend_from_slice(&fcc);
        // Mirrors the parse-side `needs_cts` rule: HEVC / AVC /
        // VVC + CodedFrames emit the SI24 composition-time;
        // everything else (CodedFramesX, SequenceStart,
        // SequenceEnd, Metadata, and the non-NALU FourCCs)
        // omits it per Enhanced RTMP v1/v2 §"ExVideoTagBody".
        // A Command frame (FrameType == VIDEO_FRAME_COMMAND, non-Metadata
        // PacketType) carries only the `videoCommand` UI8 in the body and
        // no SI24 CTS — mirror the parse-side `is_command` guard so a
        // `command_tag_ex` (which stamps `CodedFrames` as the PacketType)
        // does not spuriously emit three CTS bytes ahead of the command.
        let is_command =
            tag.frame_type == VIDEO_FRAME_COMMAND && real_packet_type != EX_PACKET_TYPE_METADATA;
        let cts_on_wire = !is_command
            && real_packet_type == EX_PACKET_TYPE_CODED_FRAMES
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
        // Legacy `VideoFrameType.Command` (FrameType == 5): per
        // `video_file_format_spec_v10_1.pdf` §E.4.3.1 the VideoTagBody is
        // a single `UI8` command regardless of CodecID — the AVC
        // packet-type + SI24 CTS prefix is *not* present. Skip it so the
        // command byte follows the header byte directly.
        if tag.codec_id == VIDEO_CODEC_AVC && tag.frame_type != VIDEO_FRAME_COMMAND {
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
/// `UB[4]` bits are interpreted as an AudioPacketType". We zero
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
    /// Enhanced RTMP v2 ModEx prelude chain
    /// (`enhanced-rtmp-v2.pdf` §"ExAudioTagHeader"). Empty for
    /// legacy tags and for Enhanced tags that carry no modifier.
    /// Each entry was an `AudioPacketType.ModEx` step before the
    /// real [`ex_packet_type`][AudioTag::ex_packet_type] was
    /// decoded; the chain is re-emitted verbatim ahead of the real
    /// packet type on build. The only subtype defined today is
    /// `TimestampOffsetNano`.
    pub mod_ex: Vec<ModEx>,
    /// Enhanced RTMP v2 `Multitrack` body (per-track FourCC + trackId +
    /// sizeOfAudioTrack chain — see [`Multitrack`]). `Some(..)` only when
    /// the wire AudioPacketType nibble was `Multitrack = 5`; in that case
    /// [`ex_packet_type`][AudioTag::ex_packet_type] holds the *real* inner
    /// AudioPacketType (e.g. `CodedFrames`, `SequenceStart`),
    /// [`audio_fourcc`][AudioTag::audio_fourcc] holds the shared codec
    /// FourCC when the multitrack mode is `OneTrack` / `ManyTracks` (and
    /// `None` for `ManyTracksManyCodecs`), and the tag's
    /// [`body`][AudioTag::body] is empty (track payloads sit in each
    /// [`MultitrackTrack::body`]).
    pub multitrack: Option<Multitrack>,
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

    /// True when this tag is the FourCC-mode
    /// `AudioPacketType.SequenceEnd` (= 2) — signals the end of the
    /// audio sequence for the current track. Per
    /// `enhanced-rtmp-v2.pdf` §"ExAudioTagHeader" this has "no less
    /// than the same meaning as a silence message" (see
    /// [`AudioMessage::Silence`]); it exists so the end of an audio
    /// sequence can be signalled per-track. The body is empty.
    pub fn is_ex_sequence_end(&self) -> bool {
        self.audio_fourcc.is_some() && self.ex_packet_type == Some(AUDIO_PACKET_TYPE_SEQUENCE_END)
    }

    /// Build a FourCC-mode `AudioPacketType.SequenceEnd` tag for the
    /// given codec FourCC (`enhanced-rtmp-v2.pdf` §"ExAudioTagHeader").
    /// The body is empty. Round-trips through [`build_audio`] /
    /// [`parse_audio`] back to the same
    /// [`is_ex_sequence_end`][Self::is_ex_sequence_end].
    pub fn sequence_end_tag(fourcc: [u8; 4]) -> AudioTag {
        AudioTag {
            sound_format: AUDIO_FORMAT_EX_HEADER,
            sound_rate: 0,
            sound_size_16bit: false,
            stereo: false,
            aac_packet_type: None,
            ex_packet_type: Some(AUDIO_PACKET_TYPE_SEQUENCE_END),
            audio_fourcc: Some(fourcc),
            body: Vec::new(),
            mod_ex: Vec::new(),
            multitrack: None,
        }
    }

    /// Sum of the `TimestampOffsetNano` ModEx entries on this tag, in
    /// nanoseconds (added to the message presentation time without
    /// altering the RTMP millisecond timestamp). `0` when absent.
    pub fn timestamp_offset_nano(&self) -> u32 {
        self.mod_ex
            .iter()
            .filter_map(ModEx::timestamp_offset_nano)
            .fold(0u32, |acc, n| acc.saturating_add(n))
    }

    /// True when this tag is an Enhanced-RTMP v2
    /// `AudioPacketType.MultichannelConfig` message (per
    /// enhanced-rtmp-v2.pdf §"ExAudioTagBody"). The body holds the
    /// `audioChannelOrder + channelCount + (mapping | flags)` layout;
    /// callers lift it via [`AudioTag::multichannel_config`].
    pub fn is_multichannel_config(&self) -> bool {
        self.audio_fourcc.is_some()
            && self.ex_packet_type == Some(AUDIO_PACKET_TYPE_MULTICHANNEL_CONFIG)
    }

    /// Decode the `MultichannelConfig` body of this tag. Returns
    /// `Ok(None)` when the tag is not a MultichannelConfig message.
    /// Errors flow through from [`MultichannelConfig::parse`] on
    /// truncated bodies.
    pub fn multichannel_config(&self) -> Result<Option<MultichannelConfig>> {
        if self.is_multichannel_config() {
            Ok(Some(MultichannelConfig::parse(&self.body)?))
        } else {
            Ok(None)
        }
    }

    /// Build an Enhanced-RTMP v2 `MultichannelConfig` audio tag with
    /// the given codec FourCC and decoded body. The returned tag has
    /// `ex_packet_type = MultichannelConfig`, `audio_fourcc = fourcc`,
    /// and `body` set to `cfg.encode()`. ModEx prelude is empty.
    pub fn multichannel_config_tag(fourcc: [u8; 4], cfg: &MultichannelConfig) -> AudioTag {
        AudioTag {
            sound_format: AUDIO_FORMAT_EX_HEADER,
            sound_rate: 0,
            sound_size_16bit: false,
            stereo: false,
            aac_packet_type: None,
            ex_packet_type: Some(AUDIO_PACKET_TYPE_MULTICHANNEL_CONFIG),
            audio_fourcc: Some(fourcc),
            body: cfg.encode(),
            mod_ex: Vec::new(),
            multitrack: None,
        }
    }

    /// True when this tag is an Enhanced-RTMP v2 audio `Multitrack`
    /// message (the wire AudioPacketType nibble was `Multitrack = 5`
    /// and [`Self::multitrack`] decoded the per-track body).
    pub fn is_multitrack(&self) -> bool {
        self.multitrack.is_some()
    }

    /// Build an Enhanced-RTMP v2 audio `Multitrack` tag with the given
    /// real inner AudioPacketType, shared FourCC (`None` for
    /// `ManyTracksManyCodecs`), and per-track body. The returned tag has
    /// `ex_packet_type = real_packet_type`, `audio_fourcc = shared_fourcc`,
    /// `multitrack = Some(mt)`, and `body` empty. ModEx prelude is empty.
    pub fn multitrack_tag(
        real_packet_type: u8,
        shared_fourcc: Option<[u8; 4]>,
        mt: Multitrack,
    ) -> AudioTag {
        AudioTag {
            sound_format: AUDIO_FORMAT_EX_HEADER,
            sound_rate: 0,
            sound_size_16bit: false,
            stereo: false,
            aac_packet_type: None,
            ex_packet_type: Some(real_packet_type),
            audio_fourcc: shared_fourcc,
            body: Vec::new(),
            mod_ex: Vec::new(),
            multitrack: Some(mt),
        }
    }

    /// Demultiplex an Enhanced-RTMP v2 audio `Multitrack` tag into
    /// standalone per-track [`AudioTag`]s (enhanced-rtmp-v2.pdf
    /// §"Multitrack Streaming via Enhanced RTMP" / §"ExAudioTagBody").
    ///
    /// Mirror of [`VideoTag::demux_tracks`]: each `(trackId, tag)` pair
    /// is the single-track tag the same frame would have produced alone
    /// (per-track or shared FourCC, the message's real inner
    /// AudioPacketType, body decoded through the ordinary
    /// [`parse_audio`] rules). The outer tag's ModEx prelude is not
    /// copied — it modifies the whole message and stays a property of
    /// the outer tag.
    pub fn demux_tracks(&self) -> Result<Vec<(u8, AudioTag)>> {
        let mt = self.multitrack.as_ref().ok_or_else(|| {
            Error::Other("AudioTag::demux_tracks: tag is not a Multitrack message".into())
        })?;
        let inner_pt = self.ex_packet_type.ok_or_else(|| {
            Error::Other("AudioTag::demux_tracks: multitrack tag lacks inner PacketType".into())
        })?;
        let mut out = Vec::with_capacity(mt.tracks.len());
        for track in &mt.tracks {
            let fcc = track.fourcc.or(self.audio_fourcc).ok_or_else(|| {
                Error::Other(format!(
                    "AudioTag::demux_tracks: track {} has no FourCC (neither per-track nor shared)",
                    track.track_id
                ))
            })?;
            let mut payload = Vec::with_capacity(5 + track.body.len());
            payload.push((AUDIO_FORMAT_EX_HEADER << 4) | (inner_pt & 0x0F));
            payload.extend_from_slice(&fcc);
            payload.extend_from_slice(&track.body);
            let tag = parse_audio(&payload).map_err(|e| {
                Error::Other(format!(
                    "AudioTag::demux_tracks: track {} body failed to parse: {e}",
                    track.track_id
                ))
            })?;
            out.push((track.track_id, tag));
        }
        Ok(out)
    }

    /// Multiplex standalone single-track Enhanced-RTMP [`AudioTag`]s
    /// into one v2 `Multitrack` tag — the inverse of
    /// [`AudioTag::demux_tracks`] and the mirror of
    /// [`VideoTag::multitrack_from_tags`] (same invariants: uniform
    /// inner AudioPacketType, shared FourCC for `OneTrack` /
    /// `ManyTracks` vs per-track FourCC for `ManyTracksManyCodecs`,
    /// exactly one track in `OneTrack` mode, no nested Multitrack /
    /// ModEx).
    pub fn multitrack_from_tags(
        multitrack_type: u8,
        tracks: &[(u8, &AudioTag)],
    ) -> Result<AudioTag> {
        let plan = plan_multitrack(
            multitrack_type,
            tracks,
            |t| t.audio_fourcc,
            |t| t.ex_packet_type,
            |t| t.multitrack.is_some(),
            |t| !t.mod_ex.is_empty(),
            AUDIO_PACKET_TYPE_MULTITRACK,
            AUDIO_PACKET_TYPE_MOD_EX,
            "audio",
        )?;
        let mut mt_tracks = Vec::with_capacity(tracks.len());
        for (track_id, tag) in tracks {
            // `build_audio` emits `[head(1)][FourCC(4)][body]` for a
            // non-multitrack Enhanced tag with no ModEx; stripping the
            // 5-byte prefix leaves the §"ExAudioTagBody" per-track body.
            let wire = build_audio(tag);
            mt_tracks.push(MultitrackTrack {
                fourcc: if plan.per_track_fourcc {
                    tag.audio_fourcc
                } else {
                    None
                },
                track_id: *track_id,
                body: wire[5..].to_vec(),
            });
        }
        Ok(AudioTag::multitrack_tag(
            plan.inner_packet_type,
            plan.shared_fourcc,
            Multitrack {
                multitrack_type,
                tracks: mt_tracks,
            },
        ))
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
/// The `ModEx` AudioPacketType prelude (a chain of
/// `modExDataSize + modExData + modExType/packetType` entries before
/// the real packet type) is now decoded into [`AudioTag::mod_ex`].
/// The `MultichannelConfig` AudioPacketType is also recognised — the
/// body bytes (`audioChannelOrder + channelCount + flags|mapping`)
/// sit in [`AudioTag::body`] verbatim and lift to the strongly-typed
/// [`MultichannelConfig`] view through
/// [`AudioTag::multichannel_config`]. The `Multitrack` AudioPacketType
/// is also recognised — the `multitrackType (UB[4]) | realPacketType
/// (UB[4])` byte plus the optional shared FourCC are consumed inline
/// here, and the per-track list (`(trackFourCc if ManyTracksManyCodecs)
/// | trackId(UI8) | (sizeOfAudioTrack(UI24) if not OneTrack) | body`)
/// is decoded into [`AudioTag::multitrack`].
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
        //   [ModEx prelude chain — present only when packetType == ModEx]
        //   byte ..=+3 = AudioFourCc (4 ASCII bytes)
        //   byte ..    = body, per (FourCc, PacketType) per
        //                §"ExAudioTagBody"
        //
        // Per spec the legacy bit-field SoundRate/SoundSize/
        // SoundType are NOT interpreted in this mode — zero them
        // on the parsed struct so a downstream consumer that
        // (incorrectly) keys off them gets a clearly-zero answer
        // instead of an arbitrary alias of the AudioPacketType
        // nibble.
        let mut packet_type = b0 & 0x0F;
        let mut pos = 1;

        // ModEx prelude (enhanced-rtmp-v2.pdf §"ExAudioTagHeader"):
        // identical loop to the video path — consume size-prefixed
        // modExData + the trailing modExType/packetType nibble while
        // the PacketType nibble is ModEx. The chain sits between the
        // header byte and the FourCC.
        let mut mod_ex = Vec::new();
        if packet_type == AUDIO_PACKET_TYPE_MOD_EX {
            let (chain, real_pt, next) =
                parse_mod_ex_chain(payload, pos, AUDIO_PACKET_TYPE_MOD_EX, "audio")?;
            mod_ex = chain;
            packet_type = real_pt;
            pos = next;
        }

        // Multitrack prelude (enhanced-rtmp-v2.pdf §"ExAudioTagHeader"):
        // a Multitrack AudioPacketType pulls in a `multitrackType
        // (UB[4]) | realPacketType (UB[4])` byte and, when the
        // multitrack mode is not ManyTracksManyCodecs, a shared FourCC.
        // The body (per-track list) is decoded later via
        // `Multitrack::parse`.
        let mut multitrack_type: Option<u8> = None;
        if packet_type == AUDIO_PACKET_TYPE_MULTITRACK {
            if pos >= payload.len() {
                return Err(Error::Other(
                    "Enhanced RTMP audio Multitrack: truncated reading multitrackType nibble"
                        .into(),
                ));
            }
            let nibble = payload[pos];
            pos += 1;
            let mt_type = (nibble >> 4) & 0x0F;
            let inner_pt = nibble & 0x0F;
            // Spec: "This fetch MUST not result in a AudioPacketType.Multitrack"
            if inner_pt == AUDIO_PACKET_TYPE_MULTITRACK {
                return Err(Error::Other(
                    "Enhanced RTMP audio Multitrack: inner PacketType MUST NOT be Multitrack"
                        .into(),
                ));
            }
            multitrack_type = Some(mt_type);
            packet_type = inner_pt;
        }

        let need_shared_fourcc = match multitrack_type {
            Some(t) => t != AV_MULTITRACK_TYPE_MANY_TRACKS_MANY_CODECS,
            None => true,
        };
        let fcc_opt = if need_shared_fourcc {
            if pos + 4 > payload.len() {
                return Err(Error::Other(
                    "Enhanced RTMP audio tag: need 4 bytes for FourCC after header/ModEx".into(),
                ));
            }
            let mut fcc = [0u8; 4];
            fcc.copy_from_slice(&payload[pos..pos + 4]);
            pos += 4;
            Some(fcc)
        } else {
            None
        };

        if let Some(mt_type) = multitrack_type {
            let mt = Multitrack::parse(&payload[pos..], mt_type)?;
            return Ok(AudioTag {
                sound_format,
                sound_rate: 0,
                sound_size_16bit: false,
                stereo: false,
                aac_packet_type: None,
                ex_packet_type: Some(packet_type),
                audio_fourcc: fcc_opt,
                body: Vec::new(),
                mod_ex,
                multitrack: Some(mt),
            });
        }

        let fcc = fcc_opt.expect("non-Multitrack audio tag requires shared FourCC slot");
        Ok(AudioTag {
            sound_format,
            sound_rate: 0,
            sound_size_16bit: false,
            stereo: false,
            aac_packet_type: None,
            ex_packet_type: Some(packet_type),
            audio_fourcc: Some(fcc),
            body: payload[pos..].to_vec(),
            mod_ex,
            multitrack: None,
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
                mod_ex: Vec::new(),
                multitrack: None,
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
                mod_ex: Vec::new(),
                multitrack: None,
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
    if tag.audio_fourcc.is_some() || tag.multitrack.is_some() {
        let real_packet_type = tag.ex_packet_type.unwrap_or(AUDIO_PACKET_TYPE_CODED_FRAMES);
        let multitrack_outer_pt = if tag.multitrack.is_some() {
            Some(AUDIO_PACKET_TYPE_MULTITRACK)
        } else {
            None
        };
        let post_mod_ex_pt = multitrack_outer_pt.unwrap_or(real_packet_type);
        // When a ModEx prelude is present the header byte's
        // AudioPacketType nibble is `ModEx`; the next packet type is
        // carried by the terminating nibble of the chain
        // (enhanced-rtmp-v2.pdf §"ExAudioTagHeader"). For a multitrack
        // tag the next packet type is `Multitrack`, not the real inner.
        let header_pt = if tag.mod_ex.is_empty() {
            post_mod_ex_pt
        } else {
            AUDIO_PACKET_TYPE_MOD_EX
        };
        let head = (AUDIO_FORMAT_EX_HEADER << 4) | (header_pt & 0x0F);
        let mut out = Vec::with_capacity(tag.body.len() + 5);
        out.push(head);
        build_mod_ex_chain(
            &mut out,
            &tag.mod_ex,
            AUDIO_PACKET_TYPE_MOD_EX,
            post_mod_ex_pt,
        );
        if let Some(mt) = &tag.multitrack {
            // Multitrack nibble: `multitrackType (UB[4]) | realPacketType
            // (UB[4])`.
            out.push(((mt.multitrack_type & 0x0F) << 4) | (real_packet_type & 0x0F));
            if mt.multitrack_type != AV_MULTITRACK_TYPE_MANY_TRACKS_MANY_CODECS {
                let fcc = tag.audio_fourcc.unwrap_or([0; 4]);
                out.extend_from_slice(&fcc);
            }
            out.extend_from_slice(&mt.encode());
            return out;
        }
        let fcc = tag
            .audio_fourcc
            .expect("Enhanced-RTMP non-Multitrack audio tag requires audio_fourcc");
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

/// An Enhanced-RTMP v2 audio *message*: either a normal coded /
/// signalling [`AudioTag`], or the special **silence** message.
///
/// `enhanced-rtmp-v2.pdf` §"ExAudioTagHeader" documents a previously
/// undocumented *audio silence* message: "This silence message is
/// identified when an audio message contains a zero-length payload,
/// or more precisely, an empty audio message without an
/// AudioTagHeader, indicating a period of silence." The spec further
/// notes "`AudioPacketType.SequenceEnd` is to have no less than the
/// same meaning as a silence message".
///
/// Because a silence message has **no bytes at all** on the wire (not
/// even a SoundFormat header), it cannot be represented as an
/// [`AudioTag`] (which always begins with at least one header byte).
/// [`parse_audio`] therefore still rejects an empty slice; callers
/// that want to recognise silence parse the raw audio-message payload
/// through [`parse_audio_message`], which lifts the zero-length case
/// to [`AudioMessage::Silence`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AudioMessage {
    /// The audio message carried a real tag (legacy framing or the
    /// Enhanced-RTMP `ExHeader` framing). Decoded via [`parse_audio`].
    Tag(AudioTag),
    /// The audio message was zero-length — a spec-defined *silence*
    /// signal. Per `enhanced-rtmp-v2.pdf` the receiver SHOULD play out
    /// any buffered audio, flush the decoder, and switch to wall-clock
    /// timing for A/V sync during the silence period. The action is
    /// otherwise system-dependent.
    Silence,
}

impl AudioMessage {
    /// True for the [`AudioMessage::Silence`] variant.
    pub fn is_silence(&self) -> bool {
        matches!(self, AudioMessage::Silence)
    }

    /// Borrow the inner [`AudioTag`] when this is a
    /// [`AudioMessage::Tag`]; `None` for silence.
    pub fn as_tag(&self) -> Option<&AudioTag> {
        match self {
            AudioMessage::Tag(t) => Some(t),
            AudioMessage::Silence => None,
        }
    }
}

/// Classify a raw RTMP audio-message payload as the spec-defined
/// *silence* signal: a zero-length payload
/// (`enhanced-rtmp-v2.pdf` §"ExAudioTagHeader"). An empty audio
/// message carries no AudioTagHeader and indicates a period of
/// silence.
pub fn is_silence_payload(payload: &[u8]) -> bool {
    payload.is_empty()
}

/// Build the wire bytes for an audio *silence* message — an empty
/// payload (`enhanced-rtmp-v2.pdf` §"ExAudioTagHeader"). The returned
/// `Vec` is always empty; the helper exists so call sites read
/// symmetrically with [`build_audio`] and document intent at the
/// emission point.
pub fn build_silence_audio() -> Vec<u8> {
    Vec::new()
}

/// Parse a raw RTMP audio-message payload into an [`AudioMessage`].
///
/// A zero-length payload lifts to [`AudioMessage::Silence`]
/// (`enhanced-rtmp-v2.pdf` §"ExAudioTagHeader"); any non-empty payload
/// is decoded through [`parse_audio`] and wrapped in
/// [`AudioMessage::Tag`]. Errors flow through from [`parse_audio`] on
/// a malformed non-empty payload.
pub fn parse_audio_message(payload: &[u8]) -> Result<AudioMessage> {
    if is_silence_payload(payload) {
        Ok(AudioMessage::Silence)
    } else {
        Ok(AudioMessage::Tag(parse_audio(payload)?))
    }
}

/// Serialise an [`AudioMessage`] back to its raw audio-message
/// payload — [`build_silence_audio`] for [`AudioMessage::Silence`]
/// (an empty payload) and [`build_audio`] for [`AudioMessage::Tag`].
/// The exact inverse of [`parse_audio_message`].
pub fn build_audio_message(msg: &AudioMessage) -> Vec<u8> {
    match msg {
        AudioMessage::Silence => build_silence_audio(),
        AudioMessage::Tag(tag) => build_audio(tag),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn video_tag_avc_nalu_roundtrip() {
        let tag = VideoTag {
            mod_ex: Vec::new(),
            frame_type: VIDEO_FRAME_KEYFRAME,
            codec_id: VIDEO_CODEC_AVC,
            avc_packet_type: Some(AVC_PACKET_TYPE_NALU),
            composition_time: 42,
            body: b"\x00\x00\x00\x05hello".to_vec(),
            ex_packet_type: None,
            fourcc: None,

            multitrack: None,
        };
        let payload = build_video(&tag);
        assert_eq!(payload[0], 0x17); // keyframe + AVC
        let back = parse_video(&payload).unwrap();
        assert_eq!(back, tag);
    }

    #[test]
    fn video_tag_negative_cts_sign_extends() {
        let tag = VideoTag {
            mod_ex: Vec::new(),
            frame_type: VIDEO_FRAME_INTER,
            codec_id: VIDEO_CODEC_AVC,
            avc_packet_type: Some(AVC_PACKET_TYPE_NALU),
            composition_time: -5,
            body: vec![0x01],
            ex_packet_type: None,
            fourcc: None,

            multitrack: None,
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
            mod_ex: Vec::new(),
            frame_type: VIDEO_FRAME_KEYFRAME,
            codec_id: 0,
            avc_packet_type: None,
            composition_time: 0,
            body: b"\x01dummy-hvcc".to_vec(),
            ex_packet_type: Some(EX_PACKET_TYPE_SEQUENCE_START),
            fourcc: Some(FOURCC_HEVC),

            multitrack: None,
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
            mod_ex: Vec::new(),
            frame_type: VIDEO_FRAME_INTER,
            codec_id: 0,
            avc_packet_type: None,
            composition_time: -33,
            body: b"\x00\x00\x00\x04NALU".to_vec(),
            ex_packet_type: Some(EX_PACKET_TYPE_CODED_FRAMES),
            fourcc: Some(FOURCC_HEVC),

            multitrack: None,
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
            mod_ex: Vec::new(),
            frame_type: VIDEO_FRAME_INTER,
            codec_id: 0,
            avc_packet_type: None,
            composition_time: 0,
            body: b"\x00\x00\x00\x04NALU".to_vec(),
            ex_packet_type: Some(EX_PACKET_TYPE_CODED_FRAMES_X),
            fourcc: Some(FOURCC_HEVC),

            multitrack: None,
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
            mod_ex: Vec::new(),
            frame_type: VIDEO_FRAME_KEYFRAME,
            codec_id: 0,
            avc_packet_type: None,
            composition_time: 0,
            body: b"\x81\x05\x0c\x00".to_vec(),
            ex_packet_type: Some(EX_PACKET_TYPE_SEQUENCE_START),
            fourcc: Some(FOURCC_AV1),

            multitrack: None,
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
            mod_ex: Vec::new(),
            frame_type: VIDEO_FRAME_KEYFRAME,
            codec_id: 0,
            avc_packet_type: None,
            composition_time: 0,
            body: b"\x0a\x0b\x0cobu-stub".to_vec(),
            ex_packet_type: Some(EX_PACKET_TYPE_CODED_FRAMES),
            fourcc: Some(FOURCC_AV1),

            multitrack: None,
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
            mod_ex: Vec::new(),
            frame_type: VIDEO_FRAME_KEYFRAME,
            codec_id: 0,
            avc_packet_type: None,
            composition_time: 0,
            body: b"vp9-frame-bytes".to_vec(),
            ex_packet_type: Some(EX_PACKET_TYPE_CODED_FRAMES),
            fourcc: Some(FOURCC_VP9),

            multitrack: None,
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
            mod_ex: Vec::new(),
            frame_type: VIDEO_FRAME_KEYFRAME,
            codec_id: 0,
            avc_packet_type: None,
            composition_time: 0,
            body: vec![],
            ex_packet_type: Some(EX_PACKET_TYPE_SEQUENCE_END),
            fourcc: Some(FOURCC_HEVC),

            multitrack: None,
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
            mod_ex: Vec::new(),
            frame_type: VIDEO_FRAME_INFO, // would be "ignored" per spec
            codec_id: 0,
            avc_packet_type: None,
            composition_time: 0,
            body: b"amf-stub".to_vec(),
            ex_packet_type: Some(EX_PACKET_TYPE_METADATA),
            fourcc: Some(FOURCC_HEVC),

            multitrack: None,
        };
        let payload = build_video(&tag);
        // IsExHeader=1 | FrameType=5 | PacketType=4 = 0xD4.
        assert_eq!(payload[0], 0xD4);
        let back = parse_video(&payload).unwrap();
        assert_eq!(back, tag);
        assert!(back.is_ex_metadata());
    }

    #[test]
    fn color_info_round_trips_full_hdr10() {
        // Realistic HDR10 colorInfo: 10-bit, BT.2020 primaries (9),
        // PQ transfer (16), BT.2020 NCL matrix (9), with hdrCll + hdrMdcv.
        let ci = ColorInfo {
            color_config: Some(ColorConfig {
                bit_depth: Some(10.0),
                color_primaries: Some(9.0),
                transfer_characteristics: Some(16.0),
                matrix_coefficients: Some(9.0),
            }),
            hdr_cll: Some(HdrCll {
                max_fall: Some(400.0),
                max_cll: Some(1000.0),
            }),
            hdr_mdcv: Some(HdrMdcv {
                red_x: Some(0.708),
                red_y: Some(0.292),
                green_x: Some(0.170),
                green_y: Some(0.797),
                blue_x: Some(0.131),
                blue_y: Some(0.046),
                white_point_x: Some(0.3127),
                white_point_y: Some(0.3290),
                max_luminance: Some(1000.0),
                min_luminance: Some(0.0001),
            }),
        };
        let tag = VideoTag::color_info_tag(FOURCC_HEVC, &ci);
        assert!(tag.is_ex_metadata());
        let payload = build_video(&tag);
        let back = parse_video(&payload).unwrap();
        assert_eq!(back, tag);
        let decoded = back.color_info().unwrap().unwrap();
        assert_eq!(decoded, ci);
        assert!(!decoded.is_reset());
    }

    #[test]
    fn color_info_partial_only_color_config_round_trips() {
        // Only colorConfig present — hdrCll/hdrMdcv absent must stay absent.
        let ci = ColorInfo {
            color_config: Some(ColorConfig {
                bit_depth: Some(8.0),
                color_primaries: Some(1.0),
                transfer_characteristics: Some(1.0),
                matrix_coefficients: Some(1.0),
            }),
            hdr_cll: None,
            hdr_mdcv: None,
        };
        let tag = VideoTag::color_info_tag(FOURCC_AV1, &ci);
        let decoded = VideoTag::color_info(&parse_video(&build_video(&tag)).unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(decoded, ci);
        assert!(decoded.hdr_cll.is_none());
        assert!(decoded.hdr_mdcv.is_none());
    }

    #[test]
    fn color_info_reset_encodes_undefined() {
        // The spec's RECOMMENDED reset signal: colorInfo = Undefined.
        let ci = ColorInfo::default();
        assert!(ci.is_reset());
        let tag = VideoTag::color_info_tag(FOURCC_HEVC, &ci);
        let back = parse_video(&build_video(&tag)).unwrap();
        let decoded = back.color_info().unwrap().unwrap();
        assert!(decoded.is_reset());
        assert_eq!(decoded, ColorInfo::default());
        // Body must be the "colorInfo" string followed by an AMF Undefined
        // marker (0x06), not an Object.
        let values = crate::amf::decode_all(&tag.body).unwrap();
        assert_eq!(values[0].as_str(), Some("colorInfo"));
        assert_eq!(values[1], Amf0Value::Undefined);
    }

    #[test]
    fn color_info_empty_object_is_present_but_empty() {
        // An empty `{}` colorInfo (alternative reset form) decodes to a
        // present-but-all-None ColorInfo, distinct from a missing pair.
        let mut body = Vec::new();
        crate::amf::encode(&mut body, &Amf0Value::String("colorInfo".into()));
        crate::amf::encode(&mut body, &Amf0Value::Object(Vec::new()));
        let tag = VideoTag {
            frame_type: VIDEO_FRAME_INFO,
            codec_id: 0,
            avc_packet_type: None,
            composition_time: 0,
            body,
            ex_packet_type: Some(EX_PACKET_TYPE_METADATA),
            fourcc: Some(FOURCC_HEVC),
            mod_ex: Vec::new(),
            multitrack: None,
        };
        let decoded = tag.color_info().unwrap().unwrap();
        // Empty object → all sub-objects absent → reset.
        assert!(decoded.is_reset());
    }

    #[test]
    fn color_info_none_for_non_metadata_tag() {
        let tag = VideoTag {
            frame_type: VIDEO_FRAME_KEYFRAME,
            codec_id: VIDEO_CODEC_AVC,
            avc_packet_type: Some(AVC_PACKET_TYPE_NALU),
            composition_time: 0,
            body: vec![0, 0, 0, 1, 0x65],
            ex_packet_type: None,
            fourcc: None,
            mod_ex: Vec::new(),
            multitrack: None,
        };
        assert_eq!(tag.color_info().unwrap(), None);
    }

    #[test]
    fn color_info_none_when_pair_name_is_not_colorinfo() {
        // A metadata tag carrying some other (future) name yields None,
        // not an error.
        let mut body = Vec::new();
        crate::amf::encode(&mut body, &Amf0Value::String("somethingElse".into()));
        crate::amf::encode(&mut body, &Amf0Value::Number(1.0));
        let tag = VideoTag::color_info_tag(FOURCC_HEVC, &ColorInfo::default());
        let tag = VideoTag { body, ..tag };
        assert_eq!(tag.color_info().unwrap(), None);
    }

    #[test]
    fn color_info_rejects_wrong_amf_type() {
        // colorInfo value of a scalar AMF type is a malformed metadata body.
        let mut body = Vec::new();
        crate::amf::encode(&mut body, &Amf0Value::String("colorInfo".into()));
        crate::amf::encode(&mut body, &Amf0Value::Number(42.0));
        let tag = VideoTag::color_info_tag(FOURCC_HEVC, &ColorInfo::default());
        let tag = VideoTag { body, ..tag };
        assert!(tag.color_info().is_err());
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
                mod_ex: Vec::new(),
                frame_type: ft,
                codec_id: VIDEO_CODEC_AVC,
                avc_packet_type: Some(AVC_PACKET_TYPE_NALU),
                composition_time: 0,
                body: vec![0x00],
                ex_packet_type: None,
                fourcc: None,

                multitrack: None,
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
            mod_ex: Vec::new(),
            frame_type: VIDEO_FRAME_KEYFRAME,
            codec_id: 0,
            avc_packet_type: None,
            composition_time: 0,
            body: vec![
                0x01, 0x00, 0x08, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            ],
            ex_packet_type: Some(EX_PACKET_TYPE_SEQUENCE_START),
            fourcc: Some(FOURCC_VP8),

            multitrack: None,
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
            mod_ex: Vec::new(),
            frame_type: VIDEO_FRAME_INTER,
            codec_id: 0,
            avc_packet_type: None,
            composition_time: 0,
            body: b"vp8-frame-bytes".to_vec(),
            ex_packet_type: Some(EX_PACKET_TYPE_CODED_FRAMES),
            fourcc: Some(FOURCC_VP8),

            multitrack: None,
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
            mod_ex: Vec::new(),
            frame_type: VIDEO_FRAME_KEYFRAME,
            codec_id: 0,
            avc_packet_type: None,
            composition_time: 0,
            body: b"\x01\x42\xc0\x1edummy-avcc".to_vec(),
            ex_packet_type: Some(EX_PACKET_TYPE_SEQUENCE_START),
            fourcc: Some(FOURCC_AVC),

            multitrack: None,
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
            mod_ex: Vec::new(),
            frame_type: VIDEO_FRAME_INTER,
            codec_id: 0,
            avc_packet_type: None,
            composition_time: -100,
            body: b"\x00\x00\x00\x05nalu1".to_vec(),
            ex_packet_type: Some(EX_PACKET_TYPE_CODED_FRAMES),
            fourcc: Some(FOURCC_AVC),

            multitrack: None,
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
            mod_ex: Vec::new(),
            frame_type: VIDEO_FRAME_INTER,
            codec_id: 0,
            avc_packet_type: None,
            composition_time: 0,
            body: b"\x00\x00\x00\x05nalu2".to_vec(),
            ex_packet_type: Some(EX_PACKET_TYPE_CODED_FRAMES_X),
            fourcc: Some(FOURCC_AVC),

            multitrack: None,
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
            mod_ex: Vec::new(),
            frame_type: VIDEO_FRAME_KEYFRAME,
            codec_id: 0,
            avc_packet_type: None,
            composition_time: 0,
            body: b"\xff\xfcdummy-vvcc".to_vec(),
            ex_packet_type: Some(EX_PACKET_TYPE_SEQUENCE_START),
            fourcc: Some(FOURCC_VVC),

            multitrack: None,
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
            mod_ex: Vec::new(),
            frame_type: VIDEO_FRAME_KEYFRAME,
            codec_id: 0,
            avc_packet_type: None,
            composition_time: 17,
            body: b"\x00\x00\x00\x06h266ku".to_vec(),
            ex_packet_type: Some(EX_PACKET_TYPE_CODED_FRAMES),
            fourcc: Some(FOURCC_VVC),

            multitrack: None,
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
            mod_ex: Vec::new(),
            frame_type: VIDEO_FRAME_INTER,
            codec_id: 0,
            avc_packet_type: None,
            composition_time: 0,
            body: b"\x00\x00\x00\x03vvc".to_vec(),
            ex_packet_type: Some(EX_PACKET_TYPE_CODED_FRAMES_X),
            fourcc: Some(FOURCC_VVC),

            multitrack: None,
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
                mod_ex: Vec::new(),
                frame_type: VIDEO_FRAME_KEYFRAME,
                codec_id: 0,
                avc_packet_type: None,
                composition_time: 0,
                body: vec![0xDE, 0xAD, 0xBE, 0xEF],
                ex_packet_type: Some(EX_PACKET_TYPE_SEQUENCE_END),
                fourcc: Some(fcc),

                multitrack: None,
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
            mod_ex: Vec::new(),
            sound_format: AUDIO_FORMAT_AAC,
            sound_rate: 3,
            sound_size_16bit: true,
            stereo: true,
            aac_packet_type: Some(AAC_PACKET_TYPE_SEQUENCE_HEADER),
            body: vec![0x12, 0x10], // LC-AAC 44.1k stereo AudioSpecificConfig
            ex_packet_type: None,
            audio_fourcc: None,

            multitrack: None,
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
            mod_ex: Vec::new(),
            sound_format: AUDIO_FORMAT_EX_HEADER,
            sound_rate: 0,
            sound_size_16bit: false,
            stereo: false,
            aac_packet_type: None,
            ex_packet_type: Some(AUDIO_PACKET_TYPE_SEQUENCE_START),
            audio_fourcc: Some(FOURCC_OPUS),
            body: b"OpusHead\x01\x02".to_vec(),

            multitrack: None,
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
            mod_ex: Vec::new(),
            sound_format: AUDIO_FORMAT_EX_HEADER,
            sound_rate: 0,
            sound_size_16bit: false,
            stereo: false,
            aac_packet_type: None,
            ex_packet_type: Some(AUDIO_PACKET_TYPE_CODED_FRAMES),
            audio_fourcc: Some(FOURCC_OPUS),
            body: b"opus-frame-bytes".to_vec(),

            multitrack: None,
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
            mod_ex: Vec::new(),
            sound_format: AUDIO_FORMAT_EX_HEADER,
            sound_rate: 0,
            sound_size_16bit: false,
            stereo: false,
            aac_packet_type: None,
            ex_packet_type: Some(AUDIO_PACKET_TYPE_SEQUENCE_START),
            audio_fourcc: Some(FOURCC_FLAC),
            body: b"fLaC\x80\x00\x00\x22streaminfo".to_vec(),

            multitrack: None,
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
            mod_ex: Vec::new(),
            sound_format: AUDIO_FORMAT_EX_HEADER,
            sound_rate: 0,
            sound_size_16bit: false,
            stereo: false,
            aac_packet_type: None,
            ex_packet_type: Some(AUDIO_PACKET_TYPE_CODED_FRAMES),
            audio_fourcc: Some(FOURCC_AC3),
            body: vec![0x0B, 0x77, 0x12, 0x34, 0x56, 0x78], // AC-3 sync + stub

            multitrack: None,
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
            mod_ex: Vec::new(),
            sound_format: AUDIO_FORMAT_EX_HEADER,
            sound_rate: 0,
            sound_size_16bit: false,
            stereo: false,
            aac_packet_type: None,
            ex_packet_type: Some(AUDIO_PACKET_TYPE_CODED_FRAMES),
            audio_fourcc: Some(FOURCC_EAC3),
            body: vec![0x0B, 0x77, 0xAB, 0xCD],

            multitrack: None,
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
            mod_ex: Vec::new(),
            sound_format: AUDIO_FORMAT_EX_HEADER,
            sound_rate: 0,
            sound_size_16bit: false,
            stereo: false,
            aac_packet_type: None,
            ex_packet_type: Some(AUDIO_PACKET_TYPE_CODED_FRAMES),
            audio_fourcc: Some(FOURCC_MP3),
            body: vec![0xFF, 0xFB, 0x90, 0x00], // MP3 sync header stub

            multitrack: None,
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
            mod_ex: Vec::new(),
            sound_format: AUDIO_FORMAT_EX_HEADER,
            sound_rate: 0,
            sound_size_16bit: false,
            stereo: false,
            aac_packet_type: None,
            ex_packet_type: Some(AUDIO_PACKET_TYPE_SEQUENCE_START),
            audio_fourcc: Some(FOURCC_AAC),
            body: vec![0x12, 0x10], // LC-AAC 44.1k stereo ASC

            multitrack: None,
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
            mod_ex: Vec::new(),
            sound_format: AUDIO_FORMAT_EX_HEADER,
            sound_rate: 0,
            sound_size_16bit: false,
            stereo: false,
            aac_packet_type: None,
            ex_packet_type: Some(AUDIO_PACKET_TYPE_SEQUENCE_END),
            audio_fourcc: Some(FOURCC_OPUS),
            body: vec![],

            multitrack: None,
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

    // ------- Enhanced RTMP v2 ModEx prelude (Veovera 2026) -------

    #[test]
    fn ex_video_mod_ex_timestamp_offset_nano_roundtrip() {
        // A single TimestampOffsetNano ModEx entry preceding a VVC
        // CodedFrames packet. Header byte low nibble = ModEx(7);
        // chain carries the real CodedFrames(1) packet type in its
        // terminating nibble; SI24 CTS then follows the FourCC.
        let nano = 999_999u32; // spec max sub-millisecond offset.
        let tag = VideoTag {
            frame_type: VIDEO_FRAME_INTER,
            codec_id: 0,
            avc_packet_type: None,
            composition_time: 7,
            body: b"\x00\x00\x00\x05nalu!".to_vec(),
            ex_packet_type: Some(EX_PACKET_TYPE_CODED_FRAMES),
            fourcc: Some(FOURCC_VVC),
            mod_ex: vec![ModEx::timestamp_offset_nano_entry(nano)],

            multitrack: None,
        };
        let payload = build_video(&tag);
        // byte 0 = IsExHeader|FrameType(2)|ModEx(7) = 0b1010_0111 = 0xA7.
        assert_eq!(payload[0], 0xA7);
        // modExDataSize = UI8 + 1 → data is 3 bytes, so UI8 = 2.
        assert_eq!(payload[1], 2);
        // modExData = bytesToUI24(999_999) = 0x0F_423F.
        assert_eq!(&payload[2..5], &[0x0F, 0x42, 0x3F]);
        // nibble byte: modExType(0, high) | packetType CodedFrames(1, low).
        assert_eq!(payload[5], 0x01);
        // FourCC then SI24 CTS then body.
        assert_eq!(&payload[6..10], b"vvc1");
        assert_eq!(&payload[10..13], &[0x00, 0x00, 0x07]);
        assert_eq!(&payload[13..], b"\x00\x00\x00\x05nalu!");

        let back = parse_video(&payload).unwrap();
        assert_eq!(back, tag);
        assert_eq!(back.timestamp_offset_nano(), nano);
        assert_eq!(back.mod_ex[0].timestamp_offset_nano(), Some(nano));
    }

    #[test]
    fn ex_video_mod_ex_chain_multiple_entries_roundtrip() {
        // Two chained ModEx entries before an AV1 SequenceStart.
        // The first entry's terminating nibble is ModEx again; the
        // second's is the real SequenceStart(0).
        let tag = VideoTag {
            frame_type: VIDEO_FRAME_KEYFRAME,
            codec_id: 0,
            avc_packet_type: None,
            composition_time: 0,
            body: b"av1cfg".to_vec(),
            ex_packet_type: Some(EX_PACKET_TYPE_SEQUENCE_START),
            fourcc: Some(FOURCC_AV1),
            mod_ex: vec![
                ModEx::timestamp_offset_nano_entry(500_000),
                ModEx {
                    mod_ex_type: 3, // a future/unknown subtype: preserved verbatim
                    data: vec![0xAA, 0xBB],
                },
            ],

            multitrack: None,
        };
        let payload = build_video(&tag);
        // First entry: size byte (2 → 3-byte data), data, nibble
        // (ModExType 0 | ModEx 7) = 0x07.
        assert_eq!(payload[1], 2);
        assert_eq!(&payload[2..5], &[0x07, 0xA1, 0x20]); // bytesToUI24(500_000)
        assert_eq!(payload[5], 0x07);
        // Second entry: size byte (1 → 2-byte data), data, nibble
        // (ModExType 3 | SequenceStart 0) = 0x30.
        assert_eq!(payload[6], 1);
        assert_eq!(&payload[7..9], &[0xAA, 0xBB]);
        assert_eq!(payload[9], 0x30);
        assert_eq!(&payload[10..14], b"av01");
        assert_eq!(&payload[14..], b"av1cfg");

        let back = parse_video(&payload).unwrap();
        assert_eq!(back, tag);
        // Only the TimestampOffsetNano entry contributes to the sum.
        assert_eq!(back.timestamp_offset_nano(), 500_000);
    }

    #[test]
    fn ex_video_mod_ex_ui16_size_escape_roundtrip() {
        // modExData longer than 255 bytes uses the UI16 escape:
        // the 8-bit size byte is 0xFF (== 256 sentinel) followed by
        // a UI16 of (len - 1).
        let big = vec![0x5A; 300];
        let tag = VideoTag {
            frame_type: VIDEO_FRAME_INTER,
            codec_id: 0,
            avc_packet_type: None,
            composition_time: 0,
            body: b"hevc-frame".to_vec(),
            ex_packet_type: Some(EX_PACKET_TYPE_CODED_FRAMES_X),
            fourcc: Some(FOURCC_HEVC),
            mod_ex: vec![ModEx {
                mod_ex_type: MOD_EX_TYPE_TIMESTAMP_OFFSET_NANO,
                data: big.clone(),
            }],

            multitrack: None,
        };
        let payload = build_video(&tag);
        // size: 0xFF sentinel + UI16(len-1 = 299 = 0x012B).
        assert_eq!(payload[1], 0xFF);
        assert_eq!(&payload[2..4], &[0x01, 0x2B]);
        assert_eq!(&payload[4..4 + 300], &big[..]);
        // nibble after data: ModExType 0 | CodedFramesX(3).
        assert_eq!(payload[4 + 300], 0x03);

        let back = parse_video(&payload).unwrap();
        assert_eq!(back, tag);
        assert_eq!(back.mod_ex[0].data.len(), 300);
    }

    #[test]
    fn ex_audio_mod_ex_timestamp_offset_nano_roundtrip() {
        // ModEx prelude on an Opus CodedFrames audio tag.
        let nano = 250_000u32;
        let tag = AudioTag {
            sound_format: AUDIO_FORMAT_EX_HEADER,
            sound_rate: 0,
            sound_size_16bit: false,
            stereo: false,
            aac_packet_type: None,
            ex_packet_type: Some(AUDIO_PACKET_TYPE_CODED_FRAMES),
            audio_fourcc: Some(FOURCC_OPUS),
            body: b"opus-pkt".to_vec(),
            mod_ex: vec![ModEx::timestamp_offset_nano_entry(nano)],

            multitrack: None,
        };
        let payload = build_audio(&tag);
        // byte 0 = ExHeader(9) << 4 | ModEx(7) = 0x97.
        assert_eq!(payload[0], 0x97);
        assert_eq!(payload[1], 2); // 3-byte data → UI8 = 2.
        assert_eq!(&payload[2..5], &[0x03, 0xD0, 0x90]); // bytesToUI24(250_000)
                                                         // nibble: ModExType 0 | CodedFrames(1).
        assert_eq!(payload[5], 0x01);
        assert_eq!(&payload[6..10], b"Opus");
        assert_eq!(&payload[10..], b"opus-pkt");

        let back = parse_audio(&payload).unwrap();
        assert_eq!(back, tag);
        assert_eq!(back.timestamp_offset_nano(), nano);
    }

    #[test]
    fn mod_ex_accessor_rejects_wrong_type_and_short_data() {
        // timestamp_offset_nano() only resolves for the
        // TimestampOffsetNano subtype with >= 3 data bytes.
        let wrong_type = ModEx {
            mod_ex_type: 1,
            data: vec![0, 0, 0],
        };
        assert_eq!(wrong_type.timestamp_offset_nano(), None);
        let too_short = ModEx {
            mod_ex_type: MOD_EX_TYPE_TIMESTAMP_OFFSET_NANO,
            data: vec![0x00, 0x01],
        };
        assert_eq!(too_short.timestamp_offset_nano(), None);
    }

    #[test]
    fn ex_video_mod_ex_truncated_chain_fails_controlled() {
        // Header announces ModEx but the chain is cut short — the
        // parser must surface a controlled error, not panic / index
        // out of bounds.
        // byte0 = IsExHeader|FrameType1|ModEx7 = 0x97 then a size
        // byte claiming 3 data bytes but no data following.
        let truncated = [0x97u8, 0x02];
        assert!(parse_video(&truncated).is_err());
        // Size + data present but missing the modExType/packetType nibble.
        let no_nibble = [0x97u8, 0x02, 0x00, 0x00, 0x00];
        assert!(parse_video(&no_nibble).is_err());
        // Chain terminates with a real packet type but no FourCC.
        let no_fourcc = [0x97u8, 0x02, 0x00, 0x00, 0x00, 0x01];
        assert!(parse_video(&no_fourcc).is_err());
    }

    #[test]
    fn ex_audio_mod_ex_truncated_chain_fails_controlled() {
        let truncated = [0x97u8, 0x02];
        assert!(parse_audio(&truncated).is_err());
        let no_fourcc = [0x97u8, 0x02, 0x00, 0x00, 0x00, 0x01];
        assert!(parse_audio(&no_fourcc).is_err());
    }

    #[test]
    fn ex_video_without_mod_ex_emits_no_prelude() {
        // Empty mod_ex must produce byte-identical output to the
        // pre-ModEx encoding (no spurious prelude bytes).
        let tag = VideoTag {
            frame_type: VIDEO_FRAME_KEYFRAME,
            codec_id: 0,
            avc_packet_type: None,
            composition_time: 0,
            body: b"\x01cfg".to_vec(),
            ex_packet_type: Some(EX_PACKET_TYPE_SEQUENCE_START),
            fourcc: Some(FOURCC_HEVC),
            mod_ex: Vec::new(),

            multitrack: None,
        };
        let payload = build_video(&tag);
        // Header low nibble is the real packet type, not ModEx.
        assert_eq!(payload[0] & 0x0F, EX_PACKET_TYPE_SEQUENCE_START);
        assert_eq!(&payload[1..5], b"hvc1");
        assert_eq!(&payload[5..], b"\x01cfg");
        assert_eq!(parse_video(&payload).unwrap(), tag);
    }

    // ------- Enhanced RTMP v2 MultichannelConfig (Veovera 2026) -------

    #[test]
    fn multichannel_config_unspecified_roundtrip() {
        // 2-byte body: order=Unspecified(0), channelCount=2.
        let cfg = MultichannelConfig {
            order: MultichannelConfigOrder::Unspecified,
            channel_count: 2,
            extra: Vec::new(),
        };
        let bytes = cfg.encode();
        assert_eq!(bytes, [0x00, 0x02]);
        let back = MultichannelConfig::parse(&bytes).unwrap();
        assert_eq!(back, cfg);
    }

    #[test]
    fn multichannel_config_native_5_1_layout() {
        // 5.1 surround = FL + FR + FC + LFE1 + BL + BR
        // = 0x01 | 0x02 | 0x04 | 0x08 | 0x10 | 0x20 = 0x3F.
        let mask = audio_channel_mask::FRONT_LEFT
            | audio_channel_mask::FRONT_RIGHT
            | audio_channel_mask::FRONT_CENTER
            | audio_channel_mask::LOW_FREQUENCY1
            | audio_channel_mask::BACK_LEFT
            | audio_channel_mask::BACK_RIGHT;
        assert_eq!(mask, 0x0000_003F);
        let cfg = MultichannelConfig {
            order: MultichannelConfigOrder::Native { flags: mask },
            channel_count: 6,
            extra: Vec::new(),
        };
        let bytes = cfg.encode();
        // order(1) | channelCount(6) | UI32-BE mask
        assert_eq!(bytes, [0x01, 0x06, 0x00, 0x00, 0x00, 0x3F]);
        let back = MultichannelConfig::parse(&bytes).unwrap();
        assert_eq!(back, cfg);
        if let MultichannelConfigOrder::Native { flags } = back.order {
            assert_eq!(flags & audio_channel_mask::LOW_FREQUENCY1, 0x08);
            assert_eq!(flags & audio_channel_mask::TOP_CENTER, 0); // not present
        } else {
            panic!("expected Native order");
        }
    }

    #[test]
    fn multichannel_config_custom_mapping_roundtrip() {
        // Stereo with explicit speaker map: ch0=FL, ch1=FR.
        let cfg = MultichannelConfig {
            order: MultichannelConfigOrder::Custom {
                mapping: vec![audio_channel::FRONT_LEFT, audio_channel::FRONT_RIGHT],
            },
            channel_count: 2,
            extra: Vec::new(),
        };
        let bytes = cfg.encode();
        // order(2) | channelCount(2) | mapping[2]
        assert_eq!(bytes, [0x02, 0x02, 0x00, 0x01]);
        let back = MultichannelConfig::parse(&bytes).unwrap();
        assert_eq!(back, cfg);
    }

    #[test]
    fn multichannel_config_custom_22_2_layout() {
        // 22.2 surround needs all 24 spec-defined channel positions
        // including the SMPTE ST 2036-2 extras. Exercises every
        // `audio_channel::*` constant on the wire.
        let mapping: Vec<u8> = (0..24).collect();
        let cfg = MultichannelConfig {
            order: MultichannelConfigOrder::Custom {
                mapping: mapping.clone(),
            },
            channel_count: 24,
            extra: Vec::new(),
        };
        let bytes = cfg.encode();
        assert_eq!(bytes.len(), 2 + 24);
        assert_eq!(bytes[0], AUDIO_CHANNEL_ORDER_CUSTOM);
        assert_eq!(bytes[1], 24);
        assert_eq!(&bytes[2..], mapping.as_slice());
        let back = MultichannelConfig::parse(&bytes).unwrap();
        assert_eq!(back, cfg);
    }

    #[test]
    fn multichannel_config_custom_with_unused_unknown_sentinels() {
        // The spec carves out 0xFE / 0xFF for empty / unknown channels;
        // round-trip those as well so callers can encode "skip this
        // channel" / "unknown speaker" without losing them.
        let cfg = MultichannelConfig {
            order: MultichannelConfigOrder::Custom {
                mapping: vec![
                    audio_channel::FRONT_LEFT,
                    audio_channel::FRONT_RIGHT,
                    audio_channel::UNUSED,
                    audio_channel::UNKNOWN,
                ],
            },
            channel_count: 4,
            extra: Vec::new(),
        };
        let bytes = cfg.encode();
        assert_eq!(bytes, [0x02, 0x04, 0x00, 0x01, 0xFE, 0xFF]);
        assert_eq!(MultichannelConfig::parse(&bytes).unwrap(), cfg);
    }

    #[test]
    fn multichannel_config_truncated_errors() {
        // Empty body: needs at least order + channelCount.
        assert!(MultichannelConfig::parse(&[]).is_err());
        assert!(MultichannelConfig::parse(&[0x00]).is_err());
        // Native missing the UI32 flags.
        assert!(MultichannelConfig::parse(&[0x01, 0x06, 0x00]).is_err());
        // Custom missing one mapping byte.
        assert!(MultichannelConfig::parse(&[0x02, 0x03, 0x00, 0x01]).is_err());
        // Unspecified with stray trailing bytes — caller likely
        // misframed it; refuse to silently swallow them.
        assert!(MultichannelConfig::parse(&[0x00, 0x02, 0xff]).is_err());
    }

    #[test]
    fn multichannel_config_reserved_order_preserves_extra_bytes() {
        // A reserved order value (anything outside 0..=2 for now) is
        // preserved verbatim so the surrounding tag can be forwarded
        // unchanged. The trailing bytes flow through `extra`.
        let body = vec![0x05, 0x04, 0xAA, 0xBB, 0xCC];
        let cfg = MultichannelConfig::parse(&body).unwrap();
        assert_eq!(cfg.order, MultichannelConfigOrder::Reserved(0x05));
        assert_eq!(cfg.channel_count, 4);
        assert_eq!(cfg.extra, vec![0xAA, 0xBB, 0xCC]);
        // Round-trip preserves the bytes.
        assert_eq!(cfg.encode(), body);
    }

    #[test]
    fn audio_tag_multichannel_config_full_roundtrip() {
        // End-to-end: build an Enhanced-RTMP audio tag carrying a
        // MultichannelConfig body for the Opus FourCC, drive it
        // through build_audio + parse_audio, then re-lift to the
        // strongly-typed view.
        let cfg = MultichannelConfig {
            order: MultichannelConfigOrder::Native {
                flags: audio_channel_mask::FRONT_LEFT
                    | audio_channel_mask::FRONT_RIGHT
                    | audio_channel_mask::FRONT_CENTER,
            },
            channel_count: 3,
            extra: Vec::new(),
        };
        let tag = AudioTag::multichannel_config_tag(FOURCC_OPUS, &cfg);
        assert!(tag.is_multichannel_config());
        assert_eq!(tag.audio_fourcc, Some(FOURCC_OPUS));
        assert_eq!(
            tag.ex_packet_type,
            Some(AUDIO_PACKET_TYPE_MULTICHANNEL_CONFIG)
        );
        // Wire shape: header byte (ExHeader nibble + MultichannelConfig
        // nibble) + 4-byte FourCC + 6-byte MultichannelConfig body.
        let wire = build_audio(&tag);
        assert_eq!(wire[0], (AUDIO_FORMAT_EX_HEADER << 4) | 0x04);
        assert_eq!(&wire[1..5], b"Opus");
        assert_eq!(wire.len(), 1 + 4 + 6);
        // Round-trip back.
        let back = parse_audio(&wire).unwrap();
        assert_eq!(back, tag);
        let cfg_back = back.multichannel_config().unwrap().unwrap();
        assert_eq!(cfg_back, cfg);
    }

    #[test]
    fn audio_tag_multichannel_config_accessor_returns_none_for_other_packet_types() {
        // A SequenceStart tag is not a MultichannelConfig — the helper
        // returns None rather than mis-parsing the sequence header
        // bytes as a channel layout.
        let tag = AudioTag {
            sound_format: AUDIO_FORMAT_EX_HEADER,
            sound_rate: 0,
            sound_size_16bit: false,
            stereo: false,
            aac_packet_type: None,
            ex_packet_type: Some(AUDIO_PACKET_TYPE_SEQUENCE_START),
            audio_fourcc: Some(FOURCC_OPUS),
            body: vec![b'O', b'p', b'u', b's', b'H', b'e', b'a', b'd'],
            mod_ex: Vec::new(),

            multitrack: None,
        };
        assert!(!tag.is_multichannel_config());
        assert!(tag.multichannel_config().unwrap().is_none());
    }

    #[test]
    fn multichannel_config_disjoint_from_legacy_audio() {
        // A legacy (non-Enhanced) audio tag never lifts as a
        // MultichannelConfig — the accessor returns None even if the
        // legacy body happens to start with a 0/1/2 byte the
        // MultichannelConfig parser would otherwise accept.
        let tag = AudioTag {
            sound_format: AUDIO_FORMAT_AAC,
            sound_rate: 3,
            sound_size_16bit: true,
            stereo: true,
            aac_packet_type: Some(AAC_PACKET_TYPE_RAW),
            ex_packet_type: None,
            audio_fourcc: None,
            body: vec![0x01, 0x06, 0x00, 0x00, 0x00, 0x3F],
            mod_ex: Vec::new(),

            multitrack: None,
        };
        assert!(!tag.is_multichannel_config());
        assert!(tag.multichannel_config().unwrap().is_none());
    }

    #[test]
    fn audio_channel_mask_22_2_bit_assignments() {
        // The 24 bit positions in audio_channel_mask must line up
        // 1:1 with the AudioChannel UI8 indices when bits are read
        // as `1 << channel_index`. Spec table cross-check.
        let pairs = [
            (audio_channel::FRONT_LEFT, audio_channel_mask::FRONT_LEFT),
            (audio_channel::FRONT_RIGHT, audio_channel_mask::FRONT_RIGHT),
            (
                audio_channel::FRONT_CENTER,
                audio_channel_mask::FRONT_CENTER,
            ),
            (
                audio_channel::LOW_FREQUENCY1,
                audio_channel_mask::LOW_FREQUENCY1,
            ),
            (audio_channel::BACK_LEFT, audio_channel_mask::BACK_LEFT),
            (audio_channel::BACK_RIGHT, audio_channel_mask::BACK_RIGHT),
            (
                audio_channel::FRONT_LEFT_CENTER,
                audio_channel_mask::FRONT_LEFT_CENTER,
            ),
            (
                audio_channel::FRONT_RIGHT_CENTER,
                audio_channel_mask::FRONT_RIGHT_CENTER,
            ),
            (audio_channel::BACK_CENTER, audio_channel_mask::BACK_CENTER),
            (audio_channel::SIDE_LEFT, audio_channel_mask::SIDE_LEFT),
            (audio_channel::SIDE_RIGHT, audio_channel_mask::SIDE_RIGHT),
            (audio_channel::TOP_CENTER, audio_channel_mask::TOP_CENTER),
            (
                audio_channel::TOP_FRONT_LEFT,
                audio_channel_mask::TOP_FRONT_LEFT,
            ),
            (
                audio_channel::TOP_FRONT_CENTER,
                audio_channel_mask::TOP_FRONT_CENTER,
            ),
            (
                audio_channel::TOP_FRONT_RIGHT,
                audio_channel_mask::TOP_FRONT_RIGHT,
            ),
            (
                audio_channel::TOP_BACK_LEFT,
                audio_channel_mask::TOP_BACK_LEFT,
            ),
            (
                audio_channel::TOP_BACK_CENTER,
                audio_channel_mask::TOP_BACK_CENTER,
            ),
            (
                audio_channel::TOP_BACK_RIGHT,
                audio_channel_mask::TOP_BACK_RIGHT,
            ),
            (
                audio_channel::LOW_FREQUENCY2,
                audio_channel_mask::LOW_FREQUENCY2,
            ),
            (
                audio_channel::TOP_SIDE_LEFT,
                audio_channel_mask::TOP_SIDE_LEFT,
            ),
            (
                audio_channel::TOP_SIDE_RIGHT,
                audio_channel_mask::TOP_SIDE_RIGHT,
            ),
            (
                audio_channel::BOTTOM_FRONT_CENTER,
                audio_channel_mask::BOTTOM_FRONT_CENTER,
            ),
            (
                audio_channel::BOTTOM_FRONT_LEFT,
                audio_channel_mask::BOTTOM_FRONT_LEFT,
            ),
            (
                audio_channel::BOTTOM_FRONT_RIGHT,
                audio_channel_mask::BOTTOM_FRONT_RIGHT,
            ),
        ];
        for (ch, mask) in pairs {
            assert_eq!(
                1u32 << ch as u32,
                mask,
                "channel {ch} should map to mask bit (1 << {ch})"
            );
        }
    }

    // ----------------------------------------------------------------
    // Multitrack — Enhanced RTMP v2 §"ExVideoTagBody" / §"ExAudioTagBody"
    // ----------------------------------------------------------------

    #[test]
    fn multitrack_one_track_video_roundtrip() {
        // OneTrack mode: no per-track FourCC, no UI24 size. The track
        // body runs from after the UI8 trackId to end-of-buffer.
        let mt = Multitrack {
            multitrack_type: AV_MULTITRACK_TYPE_ONE_TRACK,
            tracks: vec![MultitrackTrack {
                fourcc: None,
                track_id: 0,
                body: b"\x00\x00\x00\x05hello".to_vec(),
            }],
        };
        let tag = VideoTag::multitrack_tag(
            VIDEO_FRAME_KEYFRAME,
            EX_PACKET_TYPE_CODED_FRAMES,
            Some(FOURCC_AVC),
            mt.clone(),
        );
        assert!(tag.is_multitrack());
        let wire = build_video(&tag);
        // Header byte: IsExHeader(1) | FrameType(001) | PacketType(Multitrack=0110)
        // = 0b1001_0110 = 0x96.
        assert_eq!(wire[0], 0x96);
        // Multitrack nibble byte: (OneTrack=0 << 4) | (CodedFrames=1) = 0x01.
        assert_eq!(wire[1], 0x01);
        // Shared FourCC sits next (OneTrack uses a shared codec).
        assert_eq!(&wire[2..6], b"avc1");
        // Then trackId, then body bytes (NO UI24 size).
        assert_eq!(wire[6], 0x00);
        assert_eq!(&wire[7..], b"\x00\x00\x00\x05hello");
        let back = parse_video(&wire).unwrap();
        assert_eq!(back, tag);
        assert_eq!(back.multitrack.as_ref().unwrap(), &mt);
        assert_eq!(back.ex_packet_type, Some(EX_PACKET_TYPE_CODED_FRAMES));
        assert_eq!(back.fourcc, Some(FOURCC_AVC));
    }

    #[test]
    fn multitrack_many_tracks_video_roundtrip() {
        // ManyTracks mode: shared FourCC, per-track UI24 sizeOfTrack.
        // Two HEVC tracks of different sizes.
        let mt = Multitrack {
            multitrack_type: AV_MULTITRACK_TYPE_MANY_TRACKS,
            tracks: vec![
                MultitrackTrack {
                    fourcc: None,
                    track_id: 0,
                    body: b"hevc-track-0".to_vec(),
                },
                MultitrackTrack {
                    fourcc: None,
                    track_id: 1,
                    body: b"hevc-track-1-longer".to_vec(),
                },
            ],
        };
        let tag = VideoTag::multitrack_tag(
            VIDEO_FRAME_INTER,
            EX_PACKET_TYPE_CODED_FRAMES,
            Some(FOURCC_HEVC),
            mt.clone(),
        );
        let wire = build_video(&tag);
        // Header byte: IsExHeader(1) | FrameType(010) | PacketType(0110)
        // = 0b1010_0110 = 0xA6.
        assert_eq!(wire[0], 0xA6);
        // Multitrack nibble: (ManyTracks=1 << 4) | (CodedFrames=1) = 0x11.
        assert_eq!(wire[1], 0x11);
        assert_eq!(&wire[2..6], b"hvc1");
        // track 0: trackId(0) + UI24 size(12) + 12 body bytes
        assert_eq!(wire[6], 0x00);
        assert_eq!(&wire[7..10], &[0, 0, 12]);
        assert_eq!(&wire[10..22], b"hevc-track-0");
        // track 1: trackId(1) + UI24 size(19) + 19 body bytes
        assert_eq!(wire[22], 0x01);
        assert_eq!(&wire[23..26], &[0, 0, 19]);
        assert_eq!(&wire[26..45], b"hevc-track-1-longer");
        let back = parse_video(&wire).unwrap();
        assert_eq!(back, tag);
    }

    #[test]
    fn multitrack_many_tracks_many_codecs_video_roundtrip() {
        // ManyTracksManyCodecs: no shared FourCC, each track carries its
        // own FourCC, each track has a UI24 size.
        let mt = Multitrack {
            multitrack_type: AV_MULTITRACK_TYPE_MANY_TRACKS_MANY_CODECS,
            tracks: vec![
                MultitrackTrack {
                    fourcc: Some(FOURCC_HEVC),
                    track_id: 0,
                    body: b"hevc-data".to_vec(),
                },
                MultitrackTrack {
                    fourcc: Some(FOURCC_AV1),
                    track_id: 1,
                    body: b"av1-obu-bytes".to_vec(),
                },
            ],
        };
        // For ManyTracksManyCodecs the shared outer FourCC is None.
        let tag = VideoTag::multitrack_tag(
            VIDEO_FRAME_KEYFRAME,
            EX_PACKET_TYPE_CODED_FRAMES,
            None,
            mt.clone(),
        );
        let wire = build_video(&tag);
        // Header byte: 0x96 (same as OneTrack — IsExHeader | KF | Multitrack).
        assert_eq!(wire[0], 0x96);
        // Multitrack nibble: (MTMC=2 << 4) | (CodedFrames=1) = 0x21.
        assert_eq!(wire[1], 0x21);
        // No shared FourCC follows — track 0 starts at offset 2.
        assert_eq!(&wire[2..6], b"hvc1");
        assert_eq!(wire[6], 0x00);
        assert_eq!(&wire[7..10], &[0, 0, 9]);
        assert_eq!(&wire[10..19], b"hevc-data");
        // Track 1.
        assert_eq!(&wire[19..23], b"av01");
        assert_eq!(wire[23], 0x01);
        assert_eq!(&wire[24..27], &[0, 0, 13]);
        assert_eq!(&wire[27..40], b"av1-obu-bytes");
        let back = parse_video(&wire).unwrap();
        assert_eq!(back, tag);
        assert_eq!(back.fourcc, None);
    }

    #[test]
    fn multitrack_audio_one_track_roundtrip() {
        // OneTrack audio Multitrack carrying an Opus CodedFrames body.
        let mt = Multitrack {
            multitrack_type: AV_MULTITRACK_TYPE_ONE_TRACK,
            tracks: vec![MultitrackTrack {
                fourcc: None,
                track_id: 0,
                body: b"opus-packet-bytes".to_vec(),
            }],
        };
        let tag = AudioTag::multitrack_tag(
            AUDIO_PACKET_TYPE_CODED_FRAMES,
            Some(FOURCC_OPUS),
            mt.clone(),
        );
        assert!(tag.is_multitrack());
        let wire = build_audio(&tag);
        // Header byte: ExHeader(9) | AudioPacketType(Multitrack=5) = 0x95.
        assert_eq!(wire[0], 0x95);
        // Multitrack nibble: (OneTrack=0 << 4) | (CodedFrames=1) = 0x01.
        assert_eq!(wire[1], 0x01);
        assert_eq!(&wire[2..6], b"Opus");
        assert_eq!(wire[6], 0x00); // trackId 0
        assert_eq!(&wire[7..], b"opus-packet-bytes");
        let back = parse_audio(&wire).unwrap();
        assert_eq!(back, tag);
        assert_eq!(back.ex_packet_type, Some(AUDIO_PACKET_TYPE_CODED_FRAMES));
        assert_eq!(back.audio_fourcc, Some(FOURCC_OPUS));
    }

    #[test]
    fn multitrack_audio_many_tracks_many_codecs_roundtrip() {
        // Mixed Opus + AAC audio multitrack — ManyTracksManyCodecs.
        let mt = Multitrack {
            multitrack_type: AV_MULTITRACK_TYPE_MANY_TRACKS_MANY_CODECS,
            tracks: vec![
                MultitrackTrack {
                    fourcc: Some(FOURCC_OPUS),
                    track_id: 0,
                    body: b"opus-bytes".to_vec(),
                },
                MultitrackTrack {
                    fourcc: Some(FOURCC_AAC),
                    track_id: 7,
                    body: b"aac-raw-frame".to_vec(),
                },
            ],
        };
        let tag = AudioTag::multitrack_tag(AUDIO_PACKET_TYPE_CODED_FRAMES, None, mt.clone());
        let wire = build_audio(&tag);
        assert_eq!(wire[0], 0x95);
        assert_eq!(wire[1], 0x21);
        // Track 0
        assert_eq!(&wire[2..6], b"Opus");
        assert_eq!(wire[6], 0x00);
        assert_eq!(&wire[7..10], &[0, 0, 10]);
        assert_eq!(&wire[10..20], b"opus-bytes");
        // Track 1
        assert_eq!(&wire[20..24], b"mp4a");
        assert_eq!(wire[24], 0x07);
        assert_eq!(&wire[25..28], &[0, 0, 13]);
        assert_eq!(&wire[28..41], b"aac-raw-frame");
        let back = parse_audio(&wire).unwrap();
        assert_eq!(back, tag);
        assert_eq!(back.audio_fourcc, None);
    }

    #[test]
    fn multitrack_video_inner_packet_type_must_not_be_multitrack() {
        // Spec: "This fetch MUST not result in a VideoPacketType.Multitrack"
        // Header 0x96 (Ex/KF/Multitrack), then nibble byte 0x06 (OneTrack |
        // inner=Multitrack=6). The parser must reject without recursing.
        let wire = [0x96u8, 0x06, b'a', b'v', b'c', b'1', 0x00];
        let err = parse_video(&wire).unwrap_err();
        let msg = format!("{err:?}");
        assert!(msg.contains("MUST NOT"), "got: {msg}");
    }

    #[test]
    fn multitrack_audio_inner_packet_type_must_not_be_multitrack() {
        // Same constraint for audio (AudioPacketType.Multitrack = 5).
        let wire = [0x95u8, 0x05, b'O', b'p', b'u', b's', 0x00];
        let err = parse_audio(&wire).unwrap_err();
        let msg = format!("{err:?}");
        assert!(msg.contains("MUST NOT"), "got: {msg}");
    }

    #[test]
    fn multitrack_video_truncated_size_overruns_error() {
        // ManyTracks: trackId(0) + UI24 size=100 + only 5 bytes follow.
        // The parser must surface a clean error, not panic.
        // Layout: header(0x96) + mt-nibble(0x11) + shared FourCC(avc1) +
        // track0 trackId(0) + size UI24 = 100 + only 5 body bytes.
        let mut wire = vec![0x96u8, 0x11];
        wire.extend_from_slice(b"avc1");
        wire.push(0x00); // trackId
        wire.extend_from_slice(&[0x00, 0x00, 100]); // size = 100
        wire.extend_from_slice(b"short"); // only 5 bytes
        let err = parse_video(&wire).unwrap_err();
        let msg = format!("{err:?}");
        assert!(
            msg.contains("overruns"),
            "expected size-overrun error, got: {msg}"
        );
    }

    #[test]
    fn multitrack_truncation_paths_audio_video() {
        // Truncated reading the multitrack nibble byte (header byte
        // says Multitrack but no payload follows).
        assert!(parse_video(&[0x96]).is_err());
        assert!(parse_audio(&[0x95]).is_err());
        // Multitrack nibble present, but missing shared FourCC (OneTrack +
        // CodedFrames inner — needs 4 bytes for the shared FourCC).
        assert!(parse_video(&[0x96, 0x01]).is_err());
        assert!(parse_audio(&[0x95, 0x01]).is_err());
        // Multitrack with shared FourCC but no trackId.
        // For OneTrack with no trackId after FourCC the loop reports
        // empty-track-list.
        assert!(parse_video(&[0x96, 0x01, b'a', b'v', b'c', b'1']).is_err());
        // ManyTracksManyCodecs needs no shared FourCC but a per-track
        // FourCC; a buffer with only the multitrack nibble fails the
        // empty-list path.
        assert!(parse_video(&[0x96, 0x21]).is_err());
    }

    #[test]
    fn multitrack_video_roundtrips_through_mod_ex_prelude() {
        // The ModEx prelude and the Multitrack prelude compose: the
        // header byte's PacketType nibble is ModEx; the chain's
        // terminating nibble is Multitrack; the nibble byte that
        // follows the FourCC-less position carries `multitrackType |
        // realPacketType`. This test confirms the parse / build path
        // round-trips that compound prelude.
        let mt = Multitrack {
            multitrack_type: AV_MULTITRACK_TYPE_ONE_TRACK,
            tracks: vec![MultitrackTrack {
                fourcc: None,
                track_id: 0,
                body: b"hevc-nalus".to_vec(),
            }],
        };
        let mut tag = VideoTag::multitrack_tag(
            VIDEO_FRAME_KEYFRAME,
            EX_PACKET_TYPE_CODED_FRAMES,
            Some(FOURCC_HEVC),
            mt.clone(),
        );
        tag.mod_ex = vec![ModEx::timestamp_offset_nano_entry(123_456)];
        let wire = build_video(&tag);
        let back = parse_video(&wire).unwrap();
        assert_eq!(back, tag);
        assert_eq!(back.timestamp_offset_nano(), 123_456);
        assert!(back.is_multitrack());
    }

    #[test]
    fn multitrack_helpers_round_trip_through_body_encode_parse() {
        // Direct Multitrack::encode + Multitrack::parse symmetry, with
        // a reserved multitrack_type (4) preserved verbatim (it's not
        // OneTrack so a UI24 size IS emitted; tracks are still decodable).
        let mt = Multitrack {
            multitrack_type: 4,
            tracks: vec![
                MultitrackTrack {
                    fourcc: None,
                    track_id: 2,
                    body: vec![0xDE, 0xAD],
                },
                MultitrackTrack {
                    fourcc: None,
                    track_id: 3,
                    body: vec![0xBE, 0xEF, 0xCA, 0xFE],
                },
            ],
        };
        let bytes = mt.encode();
        let back = Multitrack::parse(&bytes, 4).unwrap();
        assert_eq!(back, mt);
    }

    // --- MPEG2TSSequenceStart video (enhanced-rtmp-v2 §"ExVideoTagBody") ---

    #[test]
    fn mpeg2ts_sequence_start_av1_round_trips() {
        let descriptor = vec![0x80, 0x04, 0x81, 0x0D, 0x00, 0x00];
        let tag = VideoTag::mpeg2ts_sequence_start_tag(FOURCC_AV1, descriptor.clone());
        assert!(tag.is_ex_mpeg2ts_sequence_start());
        assert!(!tag.is_ex_sequence_header()); // mutually exclusive
        assert_eq!(tag.mpeg2ts_video_descriptor(), Some(&descriptor[..]));

        let wire = build_video(&tag);
        // Header byte: IsExHeader(0x80) | FrameType keyframe(1<<4) |
        // MPEG2TSSequenceStart(5).
        assert_eq!(
            wire[0],
            0x80 | (1 << 4) | EX_PACKET_TYPE_MPEG2TS_SEQUENCE_START
        );
        assert_eq!(&wire[1..5], &FOURCC_AV1); // no CTS for this packet type
        assert_eq!(&wire[5..], &descriptor[..]);

        let back = parse_video(&wire).unwrap();
        assert_eq!(back, tag);
        assert!(back.is_ex_mpeg2ts_sequence_start());
        assert_eq!(back.mpeg2ts_video_descriptor(), Some(&descriptor[..]));
    }

    #[test]
    fn mpeg2ts_descriptor_is_none_for_non_mpeg2ts_tags() {
        // A regular SequenceStart is not an MPEG2TS sequence start.
        let seq = VideoTag {
            frame_type: VIDEO_FRAME_KEYFRAME,
            codec_id: 0,
            avc_packet_type: None,
            composition_time: 0,
            body: vec![0x01, 0x02],
            ex_packet_type: Some(EX_PACKET_TYPE_SEQUENCE_START),
            fourcc: Some(FOURCC_AV1),
            mod_ex: Vec::new(),
            multitrack: None,
        };
        assert!(!seq.is_ex_mpeg2ts_sequence_start());
        assert_eq!(seq.mpeg2ts_video_descriptor(), None);

        // An MPEG2TSSequenceStart for a FourCc with no spec-defined
        // descriptor body yields None from the descriptor accessor.
        let hvc = VideoTag::mpeg2ts_sequence_start_tag(FOURCC_HEVC, vec![0xAA]);
        assert!(hvc.is_ex_mpeg2ts_sequence_start());
        assert_eq!(hvc.mpeg2ts_video_descriptor(), None);
    }

    // --- SequenceEnd typed surface (enhanced-rtmp-v2) ---

    #[test]
    fn video_sequence_end_tag_round_trips() {
        let tag = VideoTag::sequence_end_tag(FOURCC_HEVC);
        assert!(tag.is_ex_sequence_end());
        assert!(!tag.is_ex_sequence_header());
        assert!(tag.body.is_empty());
        let wire = build_video(&tag);
        assert_eq!(wire[0], 0x80 | (2 << 4) | EX_PACKET_TYPE_SEQUENCE_END);
        assert_eq!(&wire[1..5], &FOURCC_HEVC);
        assert_eq!(wire.len(), 5); // header + FourCC, empty body, no CTS
        let back = parse_video(&wire).unwrap();
        assert_eq!(back, tag);
        assert!(back.is_ex_sequence_end());
    }

    #[test]
    fn audio_sequence_end_tag_round_trips() {
        let tag = AudioTag::sequence_end_tag(FOURCC_OPUS);
        assert!(tag.is_ex_sequence_end());
        assert!(!tag.is_ex_sequence_header());
        assert!(tag.body.is_empty());
        let wire = build_audio(&tag);
        assert_eq!(
            wire[0],
            (AUDIO_FORMAT_EX_HEADER << 4) | AUDIO_PACKET_TYPE_SEQUENCE_END
        );
        assert_eq!(&wire[1..5], &FOURCC_OPUS);
        assert_eq!(wire.len(), 5);
        let back = parse_audio(&wire).unwrap();
        assert_eq!(back, tag);
        assert!(back.is_ex_sequence_end());
    }

    // --- Audio silence message (enhanced-rtmp-v2 §"ExAudioTagHeader") ---

    #[test]
    fn empty_audio_payload_is_silence_message() {
        // "an empty audio message without an AudioTagHeader, indicating
        // a period of silence." A zero-length payload lifts to Silence.
        assert!(is_silence_payload(&[]));
        let msg = parse_audio_message(&[]).unwrap();
        assert_eq!(msg, AudioMessage::Silence);
        assert!(msg.is_silence());
        assert!(msg.as_tag().is_none());
    }

    #[test]
    fn silence_message_builds_to_empty_payload() {
        assert!(build_silence_audio().is_empty());
        assert_eq!(
            build_audio_message(&AudioMessage::Silence),
            Vec::<u8>::new()
        );
    }

    #[test]
    fn silence_message_round_trips() {
        let wire = build_audio_message(&AudioMessage::Silence);
        let back = parse_audio_message(&wire).unwrap();
        assert_eq!(back, AudioMessage::Silence);
    }

    #[test]
    fn non_empty_audio_payload_is_a_tag_not_silence() {
        // A legacy AAC raw frame is a real tag, never silence.
        let tag = AudioTag {
            sound_format: AUDIO_FORMAT_AAC,
            sound_rate: 3,
            sound_size_16bit: true,
            stereo: true,
            aac_packet_type: Some(AAC_PACKET_TYPE_RAW),
            ex_packet_type: None,
            audio_fourcc: None,
            body: vec![0xAB, 0xCD],
            mod_ex: Vec::new(),
            multitrack: None,
        };
        let wire = build_audio(&tag);
        assert!(!is_silence_payload(&wire));
        let msg = parse_audio_message(&wire).unwrap();
        assert!(!msg.is_silence());
        assert_eq!(msg, AudioMessage::Tag(tag));
    }

    #[test]
    fn audio_message_round_trips_a_tag() {
        // Enhanced-RTMP Opus CodedFrames tag wrapped as a message.
        let tag = AudioTag {
            sound_format: AUDIO_FORMAT_EX_HEADER,
            sound_rate: 0,
            sound_size_16bit: false,
            stereo: false,
            aac_packet_type: None,
            ex_packet_type: Some(AUDIO_PACKET_TYPE_CODED_FRAMES),
            audio_fourcc: Some(FOURCC_OPUS),
            body: vec![0x01, 0x02, 0x03],
            mod_ex: Vec::new(),
            multitrack: None,
        };
        let wire = build_audio_message(&AudioMessage::Tag(tag.clone()));
        let back = parse_audio_message(&wire).unwrap();
        assert_eq!(back, AudioMessage::Tag(tag));
    }
}
