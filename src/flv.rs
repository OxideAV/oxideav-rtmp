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
/// back through [`MultichannelConfig::encode`] / [`AudioTag::with_multichannel_config`].
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
    /// Trailing bytes preserved verbatim when [`order`] is
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
    /// True when this tag carries an Enhanced-RTMP
    /// `PacketTypeMetadata` body (HDR `colorInfo` and the like).
    /// Per Enhanced RTMP v1 the `FrameType` flags above the
    /// PacketType nibble are required to be ignored when this is
    /// set, so callers that classify keyframe vs interframe must
    /// short-circuit on this predicate first.
    pub fn is_ex_metadata(&self) -> bool {
        self.fourcc.is_some() && self.ex_packet_type == Some(EX_PACKET_TYPE_METADATA)
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
        let needs_cts = packet_type == EX_PACKET_TYPE_CODED_FRAMES
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
        let cts_on_wire = real_packet_type == EX_PACKET_TYPE_CODED_FRAMES
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
}
