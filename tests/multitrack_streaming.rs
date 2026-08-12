//! End-to-end Multitrack streaming (enhanced-rtmp-v2.pdf
//! §"Multitrack Streaming via Enhanced RTMP") over real loopback
//! sockets, in both directions:
//!
//! * publish: `RtmpClient::send_video_multitrack` /
//!   `send_audio_multitrack` → `RtmpSession::next_packet` →
//!   `demux_tracks` recovers every per-track tag bit-exactly
//!   (including the per-track SI24 composition time the NALU FourCCs
//!   carry inside their track bodies);
//! * play: `PlaySession::send_video_multitrack` →
//!   `RtmpPlayer::next_packet` → `demux_tracks` on the subscriber
//!   side, proving a server can serve an ABR ladder / multi-codec
//!   variant set on one play stream.

use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use oxideav_rtmp::flv::{
    AudioTag, VideoTag, AUDIO_PACKET_TYPE_CODED_FRAMES, AV_MULTITRACK_TYPE_MANY_TRACKS,
    AV_MULTITRACK_TYPE_MANY_TRACKS_MANY_CODECS, EX_PACKET_TYPE_CODED_FRAMES, FOURCC_FLAC,
    FOURCC_HEVC, FOURCC_OPUS, VIDEO_FRAME_KEYFRAME,
};
use oxideav_rtmp::{
    PlayOptions, PlayerPacket, RtmpClient, RtmpPlayer, RtmpServer, SessionRequest, StreamPacket,
};

fn ex_video(fourcc: [u8; 4], cts: i32, body: &[u8]) -> VideoTag {
    VideoTag {
        frame_type: VIDEO_FRAME_KEYFRAME,
        codec_id: 0,
        avc_packet_type: None,
        composition_time: cts,
        body: body.to_vec(),
        ex_packet_type: Some(EX_PACKET_TYPE_CODED_FRAMES),
        fourcc: Some(fourcc),
        mod_ex: Vec::new(),
        multitrack: None,
    }
}

fn ex_audio(fourcc: [u8; 4], body: &[u8]) -> AudioTag {
    AudioTag {
        sound_format: 9,
        sound_rate: 0,
        sound_size_16bit: false,
        stereo: false,
        aac_packet_type: None,
        ex_packet_type: Some(AUDIO_PACKET_TYPE_CODED_FRAMES),
        audio_fourcc: Some(fourcc),
        body: body.to_vec(),
        mod_ex: Vec::new(),
        multitrack: None,
    }
}

/// Publish direction: a two-track HEVC ladder + a two-codec audio
/// message arrive at the ingest as multitrack tags and demux to the
/// exact per-track tags the publisher multiplexed.
#[test]
fn publisher_multitrack_reaches_ingest_and_demuxes() {
    let server = RtmpServer::bind("127.0.0.1:0").expect("bind");
    let addr = server.local_addr().expect("local_addr");

    let (tx, rx) = mpsc::channel::<StreamPacket>();
    let server_thread = thread::spawn(move || {
        let req = server.accept().expect("accept");
        let mut session = req.accept().expect("session");
        session
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("timeout");
        while let Ok(Some(pkt)) = session.next_packet() {
            let _ = tx.send(pkt);
        }
    });

    thread::sleep(Duration::from_millis(50));
    let url = format!("rtmp://{}:{}/live/mt-key", addr.ip(), addr.port());
    let mut client = RtmpClient::connect(&url).expect("connect");

    // Video: ManyTracks — shared hvc1, two rungs of an ABR ladder with
    // distinct per-track composition times.
    let v0 = ex_video(FOURCC_HEVC, 17, b"hevc-1080p-au");
    let v1 = ex_video(FOURCC_HEVC, -4, b"hevc-480p-au");
    client
        .send_video_multitrack(1000, AV_MULTITRACK_TYPE_MANY_TRACKS, &[(0, &v0), (1, &v1)])
        .expect("send video multitrack");

    // Audio: ManyTracksManyCodecs — Opus default + FLAC alternate.
    let a0 = ex_audio(FOURCC_OPUS, b"opus-frame");
    let a1 = ex_audio(FOURCC_FLAC, b"flac-frame");
    client
        .send_audio_multitrack(
            1000,
            AV_MULTITRACK_TYPE_MANY_TRACKS_MANY_CODECS,
            &[(0, &a0), (7, &a1)],
        )
        .expect("send audio multitrack");

    client.close().expect("close");

    let mut got_video = false;
    let mut got_audio = false;
    while let Ok(pkt) = rx.recv_timeout(Duration::from_secs(5)) {
        match pkt {
            StreamPacket::Video { timestamp, tag } => {
                assert_eq!(timestamp, 1000);
                assert!(tag.is_multitrack());
                let tracks = tag.demux_tracks().expect("demux video");
                assert_eq!(tracks.len(), 2);
                assert_eq!(tracks[0].0, 0);
                assert_eq!(tracks[0].1, v0);
                assert_eq!(tracks[1].0, 1);
                assert_eq!(tracks[1].1, v1);
                got_video = true;
            }
            StreamPacket::Audio { timestamp, tag } => {
                assert_eq!(timestamp, 1000);
                let tracks = tag.demux_tracks().expect("demux audio");
                assert_eq!(tracks.len(), 2);
                assert_eq!(tracks[0].0, 0);
                assert_eq!(tracks[0].1, a0);
                assert_eq!(tracks[1].0, 7);
                assert_eq!(tracks[1].1, a1);
                got_audio = true;
            }
            _ => {}
        }
        if got_video && got_audio {
            break;
        }
    }
    assert!(got_video, "multitrack video never arrived");
    assert!(got_audio, "multitrack audio never arrived");
    server_thread.join().expect("server thread");
}

/// Play direction: the server serves a multi-codec video variant set
/// through one play stream; the subscriber demuxes per-track tags.
#[test]
fn play_session_serves_multitrack_to_subscriber() {
    let server = RtmpServer::bind("127.0.0.1:0").expect("bind");
    let addr = server.local_addr().expect("local_addr");

    let v_hevc = ex_video(FOURCC_HEVC, 33, b"hevc-au");
    let v_hevc_srv = v_hevc.clone();

    let server_thread = thread::spawn(move || {
        let req = match server.accept_any().expect("accept_any") {
            SessionRequest::Play(req) => req,
            SessionRequest::Publish(_) => panic!("expected play"),
        };
        let mut session = req.accept().expect("accept");
        // ManyTracks: two HEVC rungs on one message.
        let alt = ex_video(FOURCC_HEVC, 0, b"hevc-alt-au");
        session
            .send_video_multitrack(
                2000,
                AV_MULTITRACK_TYPE_MANY_TRACKS,
                &[(0, &v_hevc_srv), (1, &alt)],
            )
            .expect("send multitrack");
        session.close().expect("close");
    });

    thread::sleep(Duration::from_millis(50));
    let url = format!("rtmp://{}:{}/vod/mt-clip", addr.ip(), addr.port());
    let mut player = RtmpPlayer::connect_with_options(&url, &PlayOptions::default()).expect("play");

    let mut got = false;
    while let Some(pkt) = player.next_packet().expect("next_packet") {
        if let PlayerPacket::Video { timestamp, tag } = pkt {
            assert_eq!(timestamp, 2000);
            let tracks = tag.demux_tracks().expect("demux");
            assert_eq!(tracks.len(), 2);
            assert_eq!(tracks[0].1, v_hevc);
            assert_eq!(tracks[1].1.body, b"hevc-alt-au");
            got = true;
        }
    }
    assert!(got, "subscriber never saw the multitrack message");
    server_thread.join().expect("server thread");
}
