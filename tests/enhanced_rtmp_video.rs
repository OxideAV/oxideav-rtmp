//! Integration tests for Enhanced RTMP v1 (Veovera 2023) video framing.
//!
//! These exercise the parse → `video_to_packet` → CodecId path so a
//! downstream consumer that wires `RtmpSession` into the registry
//! sees the right `CodecParameters` + `Packet` flags for HEVC / AV1
//! / VP9 publishers (e.g. OBS 30+ in Enhanced-RTMP mode, YouTube /
//! Twitch ingest endpoints that support the FourCC variant).

use oxideav_rtmp::flv::{
    build_video, parse_video, VideoTag, EX_PACKET_TYPE_CODED_FRAMES, EX_PACKET_TYPE_METADATA,
    EX_PACKET_TYPE_SEQUENCE_START, FOURCC_AV1, FOURCC_HEVC, FOURCC_VP9, VIDEO_FRAME_INTER,
    VIDEO_FRAME_KEYFRAME,
};
use oxideav_rtmp::{video_codec_id_for_tag, video_to_packet};

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
    assert_eq!(pkt.dts, Some(1000));
    assert_eq!(pkt.pts, Some(1000));
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
    assert_eq!(pkt.dts, Some(500));
    assert_eq!(pkt.pts, Some(452));
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
            frame_type: ft,
            codec_id: 0,
            avc_packet_type: None,
            composition_time: cts,
            body,
            ex_packet_type: Some(pt),
            fourcc: Some(fcc),
        };
        let bytes1 = build_video(&tag1);
        let tag2 = parse_video(&bytes1).unwrap_or_else(|e| panic!("{label}: parse: {e:?}"));
        let bytes2 = build_video(&tag2);
        assert_eq!(bytes1, bytes2, "{label}: build is not idempotent");
        assert_eq!(tag1, tag2, "{label}: parse(build(t)) != t");
    }
}
