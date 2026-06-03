//! Integration tests for Enhanced RTMP video framing.
//!
//! Covers both the v1 (Veovera 2023) FourCC set — HEVC / AV1 / VP9 —
//! and the v2 (Veovera 2026) additions — VP8 / FourCC-mode AVC /
//! VVC. These exercise the parse → `video_to_packet` → CodecId path
//! so a downstream consumer that wires `RtmpSession` into the
//! registry sees the right `CodecParameters` + `Packet` flags for
//! every publisher we know how to handle (OBS 30+ in Enhanced-RTMP
//! mode, YouTube / Twitch / Facebook ingest endpoints that support
//! the FourCC variant).

use oxideav_rtmp::flv::{
    build_video, parse_video, VideoTag, EX_PACKET_TYPE_CODED_FRAMES, EX_PACKET_TYPE_CODED_FRAMES_X,
    EX_PACKET_TYPE_METADATA, EX_PACKET_TYPE_SEQUENCE_START, FOURCC_AV1, FOURCC_AVC, FOURCC_HEVC,
    FOURCC_VP8, FOURCC_VP9, FOURCC_VVC, VIDEO_FRAME_INTER, VIDEO_FRAME_KEYFRAME,
};
use oxideav_rtmp::{video_codec_id_for_tag, video_fourcc_codec_id, video_to_packet, RTMP_MS_TO_NS};

/// The exact byte sequence Enhanced RTMP §"Defining Additional
/// Video Codecs" Table 4 specifies for a HEVC keyframe with a CTS
/// offset of zero: `IsExHeader(1) | FrameType(1) | PacketType(1)` =
/// `0x91`, then the `hvc1` FourCC, then SI24(0), then the
/// length-prefixed NALU bytes. Reading this back yields a packet
/// with `pts == dts` and `keyframe = true`.
#[test]
fn hevc_keyframe_wire_bytes_round_trip_to_packet() {
    let payload: Vec<u8> = vec![
        0x91, // IsExHeader=1 | FrameType=1 (key) | PacketType=1 (CodedFrames)
        b'h', b'v', b'c', b'1', // FourCC
        0x00, 0x00, 0x00, // SI24 CompositionTime = 0
        0x00, 0x00, 0x00, 0x05, b'h', b'e', b'l', b'l', b'o', // NALU
    ];
    let tag = parse_video(&payload).expect("parse");
    assert_eq!(tag.fourcc, Some(FOURCC_HEVC));
    assert_eq!(tag.ex_packet_type, Some(EX_PACKET_TYPE_CODED_FRAMES));
    assert_eq!(tag.composition_time, 0);
    assert_eq!(tag.body, b"\x00\x00\x00\x05hello".to_vec());

    let pkt = video_to_packet(1000, &tag);
    // RTMP_TIME_BASE is 1/1_000_000_000 — wire ms times RTMP_MS_TO_NS.
    assert_eq!(pkt.dts, Some(1000 * RTMP_MS_TO_NS));
    assert_eq!(pkt.pts, Some(1000 * RTMP_MS_TO_NS));
    assert!(pkt.flags.keyframe);
    assert!(!pkt.flags.header);
    assert_eq!(video_codec_id_for_tag(&tag).as_str(), "hevc");
}

/// HEVC CTS encoding: a publisher emitting B-frames sends SI24
/// composition-time offsets that the receiver must add to DTS to
/// get PTS. Sign-extension must work for the negative half of the
/// 24-bit range. Test value here exercises the `0xFFFFD0` →
/// -48 path.
#[test]
fn hevc_negative_cts_wire_value_recovers_signed_offset() {
    let payload: Vec<u8> = vec![
        0xA1, // IsExHeader=1 | FrameType=2 (inter) | PacketType=1 (CodedFrames)
        b'h', b'v', b'c', b'1', 0xFF, 0xFF, 0xD0, // SI24(-48), two's complement
        0xDE, 0xAD, 0xBE, 0xEF, // body
    ];
    let tag = parse_video(&payload).expect("parse");
    assert_eq!(tag.composition_time, -48);

    let pkt = video_to_packet(500, &tag);
    assert_eq!(pkt.dts, Some(500 * RTMP_MS_TO_NS));
    assert_eq!(pkt.pts, Some(452 * RTMP_MS_TO_NS));
    assert!(!pkt.flags.keyframe);
}

/// AV1 and VP9 never carry SI24 on the wire (Enhanced RTMP v1
/// Table 4: `CompositionTime Offset is implied to equal zero`).
/// Reading these bytes back must not consume three phantom CTS
/// bytes from the body — assert the entire post-FourCC payload is
/// the body.
#[test]
fn av1_coded_frames_body_starts_immediately_after_fourcc() {
    let payload: Vec<u8> = vec![0x91, b'a', b'v', b'0', b'1', 0x0a, 0x0b, 0x0c, 0x0d, 0x0e];
    let tag = parse_video(&payload).expect("parse");
    assert_eq!(tag.fourcc, Some(FOURCC_AV1));
    assert_eq!(tag.composition_time, 0);
    // 5 body bytes — none consumed by a phantom CTS.
    assert_eq!(tag.body, vec![0x0a, 0x0b, 0x0c, 0x0d, 0x0e]);
    assert_eq!(video_codec_id_for_tag(&tag).as_str(), "av1");
}

/// VP9 SequenceStart carries the `VPCodecConfigurationRecord` (a
/// 12-ish byte tuple — profile / level / bitDepth / chromaSubsampling
/// / primaries / etc., defined alongside the WebM matroska codec
/// private). We don't parse the record here — just surface it as
/// the packet's body with the `header` flag set so a downstream
/// VP9 decoder picks it up via `extradata`.
#[test]
fn vp9_sequence_start_emits_header_flagged_packet_with_body_extradata() {
    let mut payload: Vec<u8> = vec![0x90, b'v', b'p', b'0', b'9'];
    // 12-byte stub `VPCodecConfigurationRecord`.
    payload.extend_from_slice(&[
        0x01, 0x00, 0x08, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    ]);
    let tag = parse_video(&payload).expect("parse");
    assert!(tag.is_ex_sequence_header());
    assert_eq!(tag.body.len(), 12);

    let pkt = video_to_packet(0, &tag);
    assert!(pkt.flags.header);
    assert!(pkt.flags.keyframe);
    assert_eq!(pkt.data.len(), 12);
}

/// Round-trip through both directions: build → parse → build. Catches
/// any asymmetry between encoder and decoder branches (e.g. CTS
/// emitted for AV1 by mistake, or extra bytes being absorbed by the
/// parser).
#[test]
fn build_then_parse_then_build_is_idempotent_for_all_fourccs() {
    struct Case {
        label: &'static str,
        fcc: [u8; 4],
        ft: u8,
        pt: u8,
        cts: i32,
        body: Vec<u8>,
    }
    let cases = [
        Case {
            label: "hevc-start",
            fcc: FOURCC_HEVC,
            ft: VIDEO_FRAME_KEYFRAME,
            pt: EX_PACKET_TYPE_SEQUENCE_START,
            cts: 0,
            body: b"hvcc".to_vec(),
        },
        Case {
            label: "hevc-coded",
            fcc: FOURCC_HEVC,
            ft: VIDEO_FRAME_INTER,
            pt: EX_PACKET_TYPE_CODED_FRAMES,
            cts: -7,
            body: b"nalu".to_vec(),
        },
        Case {
            label: "hevc-meta",
            fcc: FOURCC_HEVC,
            ft: VIDEO_FRAME_INTER,
            pt: EX_PACKET_TYPE_METADATA,
            cts: 0,
            body: b"amf-pair".to_vec(),
        },
        Case {
            label: "av1-start",
            fcc: FOURCC_AV1,
            ft: VIDEO_FRAME_KEYFRAME,
            pt: EX_PACKET_TYPE_SEQUENCE_START,
            cts: 0,
            body: b"av1c".to_vec(),
        },
        Case {
            label: "av1-coded",
            fcc: FOURCC_AV1,
            ft: VIDEO_FRAME_KEYFRAME,
            pt: EX_PACKET_TYPE_CODED_FRAMES,
            cts: 0,
            body: b"obus".to_vec(),
        },
        Case {
            label: "vp9-coded",
            fcc: FOURCC_VP9,
            ft: VIDEO_FRAME_KEYFRAME,
            pt: EX_PACKET_TYPE_CODED_FRAMES,
            cts: 0,
            body: b"frame".to_vec(),
        },
        // ----- Enhanced RTMP v2 (Veovera 2026) additions -----
        Case {
            label: "vp8-start",
            fcc: FOURCC_VP8,
            ft: VIDEO_FRAME_KEYFRAME,
            pt: EX_PACKET_TYPE_SEQUENCE_START,
            cts: 0,
            body: b"vp8c".to_vec(),
        },
        Case {
            label: "vp8-coded",
            fcc: FOURCC_VP8,
            ft: VIDEO_FRAME_KEYFRAME,
            pt: EX_PACKET_TYPE_CODED_FRAMES,
            cts: 0,
            body: b"vp8frame".to_vec(),
        },
        Case {
            label: "avc1-start",
            fcc: FOURCC_AVC,
            ft: VIDEO_FRAME_KEYFRAME,
            pt: EX_PACKET_TYPE_SEQUENCE_START,
            cts: 0,
            body: b"avcc".to_vec(),
        },
        Case {
            label: "avc1-coded-with-cts",
            fcc: FOURCC_AVC,
            ft: VIDEO_FRAME_INTER,
            pt: EX_PACKET_TYPE_CODED_FRAMES,
            cts: -42,
            body: b"\x00\x00\x00\x05nalu1".to_vec(),
        },
        Case {
            label: "avc1-coded-x",
            fcc: FOURCC_AVC,
            ft: VIDEO_FRAME_INTER,
            pt: EX_PACKET_TYPE_CODED_FRAMES_X,
            cts: 0,
            body: b"\x00\x00\x00\x05nalu2".to_vec(),
        },
        Case {
            label: "vvc1-start",
            fcc: FOURCC_VVC,
            ft: VIDEO_FRAME_KEYFRAME,
            pt: EX_PACKET_TYPE_SEQUENCE_START,
            cts: 0,
            body: b"vvcc".to_vec(),
        },
        Case {
            label: "vvc1-coded-with-cts",
            fcc: FOURCC_VVC,
            ft: VIDEO_FRAME_INTER,
            pt: EX_PACKET_TYPE_CODED_FRAMES,
            cts: 23,
            body: b"\x00\x00\x00\x06h266ku".to_vec(),
        },
        Case {
            label: "vvc1-coded-x",
            fcc: FOURCC_VVC,
            ft: VIDEO_FRAME_KEYFRAME,
            pt: EX_PACKET_TYPE_CODED_FRAMES_X,
            cts: 0,
            body: b"\x00\x00\x00\x03vvc".to_vec(),
        },
    ];
    for Case {
        label,
        fcc,
        ft,
        pt,
        cts,
        body,
    } in cases
    {
        let tag1 = VideoTag {
            mod_ex: Vec::new(),
            frame_type: ft,
            codec_id: 0,
            avc_packet_type: None,
            composition_time: cts,
            body,
            ex_packet_type: Some(pt),
            fourcc: Some(fcc),

            multitrack: None,
        };
        let bytes1 = build_video(&tag1);
        let tag2 = parse_video(&bytes1).unwrap_or_else(|e| panic!("{label}: parse: {e:?}"));
        let bytes2 = build_video(&tag2);
        assert_eq!(bytes1, bytes2, "{label}: build is not idempotent");
        assert_eq!(tag1, tag2, "{label}: parse(build(t)) != t");
    }
}

// ------- Enhanced RTMP v2 (Veovera 2026) FourCC additions -------

/// The exact byte sequence enhanced-rtmp-v2.pdf §"Enhanced Video"
/// gives for a FourCC-mode AVC keyframe with a CTS offset:
/// `IsExHeader(1) | FrameType(1) | PacketType(1)` = `0x91`, then
/// `avc1`, then SI24(CTS), then length-prefixed NALUs. Parse must
/// recover the same `composition_time` as the legacy AVC path
/// since the pseudocode rows are identical (`compositionTimeOffset
/// = SI24`). Resolves to `CodecId("h264")`.
#[test]
fn avc_fourcc_keyframe_wire_bytes_round_trip_to_packet() {
    let payload: Vec<u8> = vec![
        0x91, // IsExHeader=1 | FrameType=1 (key) | PacketType=1 (CodedFrames)
        b'a', b'v', b'c', b'1', // FourCC
        0x00, 0x00, 0x10, // SI24(16)
        0x00, 0x00, 0x00, 0x05, b'h', b'e', b'l', b'l', b'o', // NALU
    ];
    let tag = parse_video(&payload).expect("parse");
    assert_eq!(tag.fourcc, Some(FOURCC_AVC));
    assert_eq!(tag.ex_packet_type, Some(EX_PACKET_TYPE_CODED_FRAMES));
    assert_eq!(tag.composition_time, 16);
    assert_eq!(tag.body, b"\x00\x00\x00\x05hello".to_vec());

    let pkt = video_to_packet(2000, &tag);
    assert_eq!(pkt.dts, Some(2000 * RTMP_MS_TO_NS));
    assert_eq!(pkt.pts, Some(2016 * RTMP_MS_TO_NS));
    assert!(pkt.flags.keyframe);
    assert!(!pkt.flags.header);
    assert_eq!(video_codec_id_for_tag(&tag).as_str(), "h264");
}

/// VVC SequenceStart wire bytes: `IsExHeader=1 | FrameType=1 |
/// PacketType=0` (`0x90`), then `vvc1`, then the
/// `VVCDecoderConfigurationRecord`. The packet must be flagged
/// `header` so a downstream H.266 decoder can pick the record up
/// as extradata, and the codec id resolves to `"vvc"`.
#[test]
fn vvc_sequence_start_emits_header_flagged_packet_with_extradata() {
    let mut payload: Vec<u8> = vec![0x90, b'v', b'v', b'c', b'1'];
    payload.extend_from_slice(&[0xff, 0xfc, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08]); // 10-byte stub VVCDecoderConfigurationRecord
    let tag = parse_video(&payload).expect("parse");
    assert!(tag.is_ex_sequence_header());
    assert_eq!(tag.body.len(), 10);

    let pkt = video_to_packet(0, &tag);
    assert!(pkt.flags.header);
    assert!(pkt.flags.keyframe);
    assert_eq!(pkt.data.len(), 10);
    assert_eq!(video_codec_id_for_tag(&tag).as_str(), "vvc");
}

/// VVC inter-frame with negative SI24 CTS reproduces the same
/// sign-extension path the HEVC test exercises. enhanced-rtmp-v2.pdf
/// §"ExVideoTagBody" mandates `compositionTimeOffset = SI24` for the
/// VVC + CodedFrames row.
#[test]
fn vvc_negative_cts_wire_value_recovers_signed_offset() {
    let payload: Vec<u8> = vec![
        0xA1, // IsExHeader=1 | FrameType=2 (inter) | PacketType=1 (CodedFrames)
        b'v', b'v', b'c', b'1', // FourCC
        0xFF, 0xFF, 0xD0, // SI24(-48)
        0xCA, 0xFE, 0xBA, 0xBE, // body
    ];
    let tag = parse_video(&payload).expect("parse");
    assert_eq!(tag.composition_time, -48);
    let pkt = video_to_packet(500, &tag);
    assert_eq!(pkt.dts, Some(500 * RTMP_MS_TO_NS));
    assert_eq!(pkt.pts, Some(452 * RTMP_MS_TO_NS));
    assert!(!pkt.flags.keyframe);
}

/// VP8 CodedFrames have no SI24 on the wire — analogous to AV1 / VP9.
/// Reading must not consume three phantom CTS bytes from the body.
#[test]
fn vp8_coded_frames_body_starts_immediately_after_fourcc() {
    let payload: Vec<u8> = vec![
        0x91, b'v', b'p', b'0', b'8', 0xDE, 0xAD, 0xBE, 0xEF, 0x10, 0x20,
    ];
    let tag = parse_video(&payload).expect("parse");
    assert_eq!(tag.fourcc, Some(FOURCC_VP8));
    assert_eq!(tag.composition_time, 0);
    assert_eq!(tag.body, vec![0xDE, 0xAD, 0xBE, 0xEF, 0x10, 0x20]);
    assert_eq!(video_codec_id_for_tag(&tag).as_str(), "vp8");
}

/// AVC-FourCC CodedFramesX optimisation: the SI24 is omitted (CTS
/// implied zero) — three bytes saved vs the CodedFrames form.
#[test]
fn avc_fourcc_coded_frames_x_omits_cts_on_wire() {
    let with_cts: Vec<u8> = vec![
        0xA1, b'a', b'v', b'c', b'1', 0x00, 0x00, 0x00, b'A', b'B', b'C', b'D',
    ];
    let without_cts: Vec<u8> = vec![0xA3, b'a', b'v', b'c', b'1', b'A', b'B', b'C', b'D'];
    let tag1 = parse_video(&with_cts).expect("parse codedframes");
    let tag2 = parse_video(&without_cts).expect("parse codedframesx");
    assert_eq!(tag1.composition_time, 0);
    assert_eq!(tag2.composition_time, 0);
    assert_eq!(tag1.body, tag2.body);
    // CodedFramesX shaves exactly three bytes off the wire.
    assert_eq!(with_cts.len() - without_cts.len(), 3);
}

/// FourCC → CodecId mapping covers every Enhanced-RTMP video FourCC
/// (v1 set + v2 additions). Anything else falls back to `"unknown"`.
#[test]
fn video_fourcc_codec_id_maps_v1_and_v2_set_and_falls_back() {
    assert_eq!(video_fourcc_codec_id(FOURCC_AV1).as_str(), "av1");
    assert_eq!(video_fourcc_codec_id(FOURCC_VP9).as_str(), "vp9");
    assert_eq!(video_fourcc_codec_id(FOURCC_HEVC).as_str(), "hevc");
    assert_eq!(video_fourcc_codec_id(FOURCC_VP8).as_str(), "vp8");
    assert_eq!(video_fourcc_codec_id(FOURCC_AVC).as_str(), "h264");
    assert_eq!(video_fourcc_codec_id(FOURCC_VVC).as_str(), "vvc");
    assert_eq!(video_fourcc_codec_id(*b"zzzz").as_str(), "unknown");
}

/// A ModEx-prefixed wire payload (Enhanced RTMP v2 §"ExVideoTagHeader")
/// must decode the prelude chain, recover the real PacketType from the
/// chain's terminating nibble, and resolve to the correct CodecId. The
/// ModEx signal is transparent at the FourCC / PacketType layer, and
/// the `TimestampOffsetNano` payload folds onto the packet's *presentation*
/// time (PTS) without altering the core RTMP decode timestamp (DTS), per
/// `enhanced-rtmp-v2.pdf` §"ExVideoTagHeader" (nanosecond offset adjusts
/// presentation time without altering the core RTMP timestamp).
/// Before ModEx support the header's low nibble of 7 would have been
/// mis-read as an unknown PacketType and the four chain bytes mistaken
/// for the FourCC.
#[test]
fn mod_ex_prefixed_hevc_coded_frames_resolves_through_adapter() {
    // byte0 = IsExHeader|FrameType=2(inter)|PacketType=7(ModEx) = 0xA7
    // ModEx entry: size UI8=2 (3-byte data), bytesToUI24(750_000)=0x0B71B0,
    //   nibble = ModExType(0)|CodedFrames(1) = 0x01
    // then FourCC hvc1, SI24 CTS=0, NALU body.
    let payload: Vec<u8> = vec![
        0xA7, 0x02, 0x0B, 0x71, 0xB0, 0x01, b'h', b'v', b'c', b'1', 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x05, b'h', b'e', b'l', b'l', b'o',
    ];
    let tag = parse_video(&payload).expect("parse mod_ex");
    assert_eq!(tag.fourcc, Some(FOURCC_HEVC));
    assert_eq!(tag.ex_packet_type, Some(EX_PACKET_TYPE_CODED_FRAMES));
    assert_eq!(tag.composition_time, 0);
    assert_eq!(tag.body, b"\x00\x00\x00\x05hello".to_vec());
    assert_eq!(tag.mod_ex.len(), 1);
    assert_eq!(tag.timestamp_offset_nano(), 750_000);

    // The adapter routes by the recovered real PacketType (same
    // CodecId + flags as a plain HEVC CodedFrames tag), and folds the
    // 750_000 ns offset onto PTS only — DTS stays on the raw ms grid.
    let pkt = video_to_packet(2000, &tag);
    assert_eq!(pkt.dts, Some(2000 * RTMP_MS_TO_NS));
    assert_eq!(pkt.pts, Some(2000 * RTMP_MS_TO_NS + 750_000));
    assert!(!pkt.flags.keyframe);
    assert_eq!(video_codec_id_for_tag(&tag).as_str(), "hevc");

    // Build round-trips the prelude verbatim.
    assert_eq!(build_video(&tag), payload);
}
