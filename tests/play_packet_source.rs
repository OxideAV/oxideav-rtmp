//! End-to-end check of the pull-mode `PacketSource` adapter
//! (`rtmp-play://` scheme).
//!
//! 1. Bind a real [`RtmpServer`] on an ephemeral port and serve a
//!    small synthetic stream through a [`PlaySession`].
//! 2. Open `rtmp-play://127.0.0.1:N/vod/clip` through a real
//!    `SourceRegistry` and verify:
//!     * the resulting `SourceOutput::Packets` carries audio + video
//!       stream descriptors with the right codec ids + extradata
//!       (AVCDecoderConfigurationRecord / AudioSpecificConfig);
//!     * `metadata()` surfaces the flattened `onMetaData` scalars;
//!     * `next_packet()` produces the expected packets in order on
//!       the nanosecond timeline (`ms * RTMP_MS_TO_NS`);
//!     * the iterator terminates with [`oxideav_core::Error::Eof`]
//!       once the server ends playback with `StreamEOF`.
//!
//! This exercises the dial-style opener, the shared probing loop
//! running over an [`RtmpPlayer`], and the PlayerPacket → Packet
//! conversion — proving pushed and pulled streams are
//! indistinguishable downstream.

use std::thread;

use oxideav_core::{Error as CoreError, SourceOutput, SourceRegistry};
use oxideav_rtmp::flv::{
    AudioTag, VideoTag, AAC_PACKET_TYPE_RAW, AAC_PACKET_TYPE_SEQUENCE_HEADER, AUDIO_FORMAT_AAC,
    AVC_PACKET_TYPE_NALU, AVC_PACKET_TYPE_SEQUENCE_HEADER, VIDEO_CODEC_AVC, VIDEO_FRAME_INTER,
    VIDEO_FRAME_KEYFRAME,
};
use oxideav_rtmp::{
    register, Amf0Value, RtmpServer, SessionRequest, AUDIO_STREAM_INDEX, RTMP_MS_TO_NS,
    RTMP_TIME_BASE, VIDEO_STREAM_INDEX,
};

#[test]
fn registry_open_rtmp_play_url_pulls_remote_stream() {
    let server = RtmpServer::bind("127.0.0.1:0").expect("bind");
    let addr = server.local_addr().expect("local_addr");

    let server_thread = thread::spawn(move || {
        let req = match server.accept_any().expect("accept_any") {
            SessionRequest::Play(req) => req,
            SessionRequest::Publish(_) => panic!("expected play"),
        };
        assert_eq!(req.app, "vod");
        assert_eq!(req.stream_name, "clip");
        let mut session = req.accept().expect("accept");

        let meta = Amf0Value::EcmaArray(vec![
            ("width".into(), Amf0Value::Number(1280.0)),
            ("height".into(), Amf0Value::Number(720.0)),
        ]);
        session.send_metadata(&meta).expect("metadata");
        session
            .send_video(
                0,
                &VideoTag {
                    mod_ex: Vec::new(),
                    frame_type: VIDEO_FRAME_KEYFRAME,
                    codec_id: VIDEO_CODEC_AVC,
                    avc_packet_type: Some(AVC_PACKET_TYPE_SEQUENCE_HEADER),
                    composition_time: 0,
                    body: vec![0x01, 0x42, 0x80, 0x1E, 0x00],
                    ex_packet_type: None,
                    fourcc: None,
                    multitrack: None,
                },
            )
            .expect("video sh");
        session
            .send_audio(
                0,
                &AudioTag {
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
                },
            )
            .expect("audio sh");
        session
            .send_video(
                40,
                &VideoTag {
                    mod_ex: Vec::new(),
                    frame_type: VIDEO_FRAME_INTER,
                    codec_id: VIDEO_CODEC_AVC,
                    avc_packet_type: Some(AVC_PACKET_TYPE_NALU),
                    composition_time: 0,
                    body: vec![0x41, 0x9A, 0x00],
                    ex_packet_type: None,
                    fourcc: None,
                    multitrack: None,
                },
            )
            .expect("video 1");
        session
            .send_audio(
                23,
                &AudioTag {
                    mod_ex: Vec::new(),
                    sound_format: AUDIO_FORMAT_AAC,
                    sound_rate: 3,
                    sound_size_16bit: true,
                    stereo: true,
                    aac_packet_type: Some(AAC_PACKET_TYPE_RAW),
                    body: vec![0xDE, 0xAD],
                    ex_packet_type: None,
                    audio_fourcc: None,
                    multitrack: None,
                },
            )
            .expect("audio 1");
        session.close().expect("close");
    });

    let mut reg = SourceRegistry::new();
    register(&mut reg);
    let url = format!("rtmp-play://{addr}/vod/clip");
    let mut src = match reg.open(&url).expect("registry open") {
        SourceOutput::Packets(p) => p,
        other => panic!("expected Packets output, got {}", kind_of(&other)),
    };

    // Stream descriptors: audio (index 0, aac, ASC extradata) +
    // video (index 1, h264, avcC extradata), both on the ns clock.
    let streams = src.streams().to_vec();
    assert_eq!(streams.len(), 2);
    let audio = &streams[0];
    assert_eq!(audio.index, AUDIO_STREAM_INDEX);
    assert_eq!(audio.time_base, RTMP_TIME_BASE);
    assert_eq!(audio.params.codec_id.as_str(), "aac");
    assert_eq!(audio.params.extradata, vec![0x12, 0x10]);
    let video = &streams[1];
    assert_eq!(video.index, VIDEO_STREAM_INDEX);
    assert_eq!(video.params.codec_id.as_str(), "h264");
    assert_eq!(video.params.extradata, vec![0x01, 0x42, 0x80, 0x1E, 0x00]);

    // Flattened onMetaData scalars.
    let meta = src.metadata().to_vec();
    assert!(meta.contains(&("width".to_string(), "1280".to_string())));
    assert!(meta.contains(&("height".to_string(), "720".to_string())));

    // Packets replay in arrival order with ns timestamps.
    let p1 = src.next_packet().expect("packet 1"); // video seq header
    assert_eq!(p1.stream_index, VIDEO_STREAM_INDEX);
    assert_eq!(p1.pts, Some(0));
    let p2 = src.next_packet().expect("packet 2"); // audio seq header
    assert_eq!(p2.stream_index, AUDIO_STREAM_INDEX);
    let p3 = src.next_packet().expect("packet 3"); // video inter @40ms
    assert_eq!(p3.stream_index, VIDEO_STREAM_INDEX);
    assert_eq!(p3.dts, Some(40 * RTMP_MS_TO_NS));
    let p4 = src.next_packet().expect("packet 4"); // audio @23ms
    assert_eq!(p4.stream_index, AUDIO_STREAM_INDEX);
    assert_eq!(p4.pts, Some(23 * RTMP_MS_TO_NS));

    // Clean end: StreamEOF → Eof.
    match src.next_packet() {
        Err(CoreError::Eof) => {}
        other => panic!("expected Eof, got {other:?}"),
    }

    server_thread.join().expect("server thread");
}

fn kind_of(out: &SourceOutput) -> &'static str {
    match out {
        SourceOutput::Bytes(_) => "Bytes",
        SourceOutput::Packets(_) => "Packets",
        SourceOutput::Frames(_) => "Frames",
        SourceOutput::MultiTitle(_) => "MultiTitle",
        _ => "Unknown",
    }
}
