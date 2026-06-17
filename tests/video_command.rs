//! `VideoFrameType.Command` (StartSeek / EndSeek) round-trips, for
//! both legacy FLV framing (`video_file_format_spec_v10_1.pdf`
//! §E.4.3.1 "VideoTagBody") and Enhanced-RTMP FourCC framing
//! (`enhanced-rtmp-v2.pdf` §"ExVideoTagHeader" — the
//! `videoPacketType != Metadata && videoFrameType == Command` branch).

use oxideav_rtmp::flv::{
    build_video, parse_video, VideoTag, EX_PACKET_TYPE_CODED_FRAMES, VIDEO_CODEC_AVC,
    VIDEO_COMMAND_END_SEEK, VIDEO_COMMAND_START_SEEK, VIDEO_FRAME_COMMAND,
};

const FOURCC_HEVC: [u8; 4] = *b"hvc1";
const FOURCC_AV1: [u8; 4] = *b"av01";

#[test]
fn legacy_command_tag_round_trips() {
    for cmd in [VIDEO_COMMAND_START_SEEK, VIDEO_COMMAND_END_SEEK] {
        let tag = VideoTag::command_tag(VIDEO_CODEC_AVC, cmd);
        assert!(tag.is_command());
        assert_eq!(tag.video_command(), Some(cmd));

        let bytes = build_video(&tag);
        // Header byte (FrameType 5 | CodecID 7) + single command byte:
        // NO AVC packet-type / SI24 CTS prefix for a command frame.
        assert_eq!(
            bytes,
            vec![(VIDEO_FRAME_COMMAND << 4) | VIDEO_CODEC_AVC, cmd]
        );

        let back = parse_video(&bytes).unwrap();
        assert_eq!(back, tag);
        assert_eq!(back.video_command(), Some(cmd));
        assert_eq!(back.frame_type, VIDEO_FRAME_COMMAND);
        assert_eq!(back.composition_time, 0);
        assert!(back.avc_packet_type.is_none());
    }
}

#[test]
fn legacy_command_non_avc_codec() {
    // The command byte is codec-independent; a Screen-codec command
    // tag has the same single-byte body.
    let tag = VideoTag::command_tag(3 /* Screen */, VIDEO_COMMAND_START_SEEK);
    let bytes = build_video(&tag);
    assert_eq!(
        bytes,
        vec![(VIDEO_FRAME_COMMAND << 4) | 3, VIDEO_COMMAND_START_SEEK]
    );
    let back = parse_video(&bytes).unwrap();
    assert_eq!(back.video_command(), Some(VIDEO_COMMAND_START_SEEK));
}

#[test]
fn enhanced_command_tag_round_trips() {
    for fcc in [FOURCC_HEVC, FOURCC_AV1] {
        for cmd in [VIDEO_COMMAND_START_SEEK, VIDEO_COMMAND_END_SEEK] {
            let tag = VideoTag::command_tag_ex(fcc, cmd);
            assert!(tag.is_command());
            assert_eq!(tag.video_command(), Some(cmd));

            let bytes = build_video(&tag);
            // IsExHeader(1) | FrameType(Command=5) | PacketType(CodedFrames=1)
            // = 0b1_101_0001 = 0xD1, then the 4-byte FourCC, then the
            // single command byte — crucially NO SI24 CTS even for the
            // NALU FourCC `hvc1` (Command short-circuits processVideoBody).
            assert_eq!(bytes[0], 0xD1);
            assert_eq!(&bytes[1..5], &fcc);
            assert_eq!(bytes.len(), 6);
            assert_eq!(bytes[5], cmd);

            let back = parse_video(&bytes).unwrap();
            assert_eq!(back, tag);
            assert_eq!(back.video_command(), Some(cmd));
            assert_eq!(back.fourcc, Some(fcc));
            assert_eq!(back.ex_packet_type, Some(EX_PACKET_TYPE_CODED_FRAMES));
            // Command frame carries no coded video / CTS.
            assert_eq!(back.composition_time, 0);
        }
    }
}

#[test]
fn non_command_frames_are_not_commands() {
    // A normal HEVC CodedFrames keyframe must NOT be misclassified as a
    // command, and its SI24 CTS must still survive the round-trip.
    let tag = VideoTag {
        frame_type: 1, // KeyFrame
        codec_id: 0,
        avc_packet_type: None,
        composition_time: 42,
        body: vec![0xde, 0xad, 0xbe, 0xef],
        ex_packet_type: Some(EX_PACKET_TYPE_CODED_FRAMES),
        fourcc: Some(FOURCC_HEVC),
        mod_ex: Vec::new(),
        multitrack: None,
    };
    assert!(!tag.is_command());
    assert_eq!(tag.video_command(), None);
    let back = parse_video(&build_video(&tag)).unwrap();
    assert_eq!(back, tag);
    assert_eq!(back.composition_time, 42);
}

#[test]
fn unknown_command_value_passes_through() {
    // "If a value in the bitstream is not understood, the logic must
    // fail gracefully" — a reserved command byte is surfaced verbatim,
    // not rejected.
    let tag = VideoTag::command_tag_ex(FOURCC_HEVC, 0x7f);
    let back = parse_video(&build_video(&tag)).unwrap();
    assert_eq!(back.video_command(), Some(0x7f));
}

#[test]
fn empty_command_body_is_none() {
    // A truncated command tag (header but no command byte) yields
    // `None` rather than panicking.
    let bytes = vec![(VIDEO_FRAME_COMMAND << 4) | VIDEO_CODEC_AVC];
    let tag = parse_video(&bytes).unwrap();
    assert!(tag.is_command());
    assert_eq!(tag.video_command(), None);
}
