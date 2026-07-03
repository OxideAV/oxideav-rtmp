//! End-to-end play (subscribe) loopback: the real [`RtmpPlayer`]
//! against the real [`RtmpServer`] / [`PlaySession`].
//!
//! Covers the §4.2.1 setup handshake with explicit Start / Duration /
//! Reset arguments and a §3.7 `SetBufferLength`, media + metadata
//! delivery through [`RtmpPlayer::next_packet`], the §4.2.7 / §4.2.8
//! pause + seek control round-trips, the receiveAudio /
//! receiveVideo toggles (§4.2.4 / §4.2.5), the §7.1.7 `StreamEOF`
//! clean end, the §4.2.1 `NetStream.Play.StreamNotFound` refusal, and
//! the §4.2.3 `deleteStream` teardown observed server-side.

use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use oxideav_rtmp::flv::{
    AudioTag, VideoTag, AAC_PACKET_TYPE_RAW, AAC_PACKET_TYPE_SEQUENCE_HEADER, AUDIO_FORMAT_AAC,
    AVC_PACKET_TYPE_NALU, AVC_PACKET_TYPE_SEQUENCE_HEADER, VIDEO_CODEC_AVC, VIDEO_FRAME_INTER,
    VIDEO_FRAME_KEYFRAME,
};
use oxideav_rtmp::message::{
    NetStreamCommand, STATUS_PAUSE_NOTIFY, STATUS_PLAY_START, STATUS_SEEK_NOTIFY,
    STATUS_UNPAUSE_NOTIFY,
};
use oxideav_rtmp::{
    Amf0Value, Error, PlayOptions, PlaySessionEvent, PlayerPacket, RtmpPlayer, RtmpServer,
    SessionRequest,
};

fn video_tag(keyframe: bool, body: Vec<u8>, avc_packet_type: u8) -> VideoTag {
    VideoTag {
        mod_ex: Vec::new(),
        frame_type: if keyframe {
            VIDEO_FRAME_KEYFRAME
        } else {
            VIDEO_FRAME_INTER
        },
        codec_id: VIDEO_CODEC_AVC,
        avc_packet_type: Some(avc_packet_type),
        composition_time: 0,
        body,
        ex_packet_type: None,
        fourcc: None,
        multitrack: None,
    }
}

fn audio_tag(body: Vec<u8>, aac_packet_type: u8) -> AudioTag {
    AudioTag {
        mod_ex: Vec::new(),
        sound_format: AUDIO_FORMAT_AAC,
        sound_rate: 3,
        sound_size_16bit: true,
        stereo: true,
        aac_packet_type: Some(aac_packet_type),
        body,
        ex_packet_type: None,
        audio_fourcc: None,
        multitrack: None,
    }
}

/// Full happy path: options-carrying play setup, sequence headers +
/// coded frames + metadata delivered in order, `StreamEOF` clean end.
#[test]
fn player_receives_full_stream_until_eof() {
    let server = RtmpServer::bind("127.0.0.1:0").expect("bind");
    let addr = server.local_addr().expect("local_addr");

    let server_thread = thread::spawn(move || {
        let req = match server.accept_any().expect("accept_any") {
            SessionRequest::Play(req) => req,
            SessionRequest::Publish(_) => panic!("expected play"),
        };
        assert_eq!(req.app, "vod");
        assert_eq!(req.stream_name, "sample");
        assert_eq!(req.start, Some(-2.0));
        assert_eq!(req.duration, Some(-1.0));
        assert_eq!(req.reset, Some(true));
        assert_eq!(req.buffer_length_ms, Some(2500));

        let mut session = req.accept_recorded().expect("accept_recorded");
        let meta = Amf0Value::EcmaArray(vec![
            ("duration".into(), Amf0Value::Number(12.5)),
            ("videocodecid".into(), Amf0Value::Number(7.0)),
        ]);
        session.send_metadata(&meta).expect("metadata");
        session
            .send_video(
                0,
                &video_tag(true, vec![1, 2, 3], AVC_PACKET_TYPE_SEQUENCE_HEADER),
            )
            .expect("video sh");
        session
            .send_audio(
                0,
                &audio_tag(vec![0x12, 0x10], AAC_PACKET_TYPE_SEQUENCE_HEADER),
            )
            .expect("audio sh");
        session
            .send_video(40, &video_tag(true, vec![9, 9, 9, 9], AVC_PACKET_TYPE_NALU))
            .expect("video 1");
        session
            .send_audio(23, &audio_tag(vec![0xAA, 0xBB], AAC_PACKET_TYPE_RAW))
            .expect("audio 1");
        session
            .send_video(80, &video_tag(false, vec![7, 7], AVC_PACKET_TYPE_NALU))
            .expect("video 2");
        session.close().expect("close");
    });

    let opts = PlayOptions {
        start: Some(-2.0),
        duration: Some(-1.0),
        reset: Some(true),
        buffer_length_ms: Some(2500),
        ..Default::default()
    };
    let url = format!("rtmp://{addr}/vod/sample");
    let mut player = RtmpPlayer::connect_with_options(&url, &opts).expect("connect play");
    assert_eq!(player.stream_name(), "sample");
    assert!(
        player.is_recorded(),
        "accept_recorded announces StreamIsRecorded during setup"
    );
    player
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("set_read_timeout");

    // Collect the media/metadata events until the clean end. Control
    // events (StreamBegin already consumed during setup) may
    // interleave; filter to the four content kinds.
    let mut metadata = None;
    let mut video = Vec::new();
    let mut audio = Vec::new();
    while let Some(pkt) = player.next_packet().expect("next_packet") {
        match pkt {
            PlayerPacket::Metadata(meta) => metadata = Some(meta),
            PlayerPacket::Video { timestamp, tag } => video.push((timestamp, tag)),
            PlayerPacket::Audio { timestamp, tag } => audio.push((timestamp, tag)),
            PlayerPacket::Status { .. } | PlayerPacket::Control(_) => {}
        }
    }

    let meta = metadata.expect("onMetaData must arrive");
    assert_eq!(meta.get("duration").and_then(Amf0Value::as_f64), Some(12.5));
    assert_eq!(video.len(), 3);
    assert_eq!(
        video[0].1.avc_packet_type,
        Some(AVC_PACKET_TYPE_SEQUENCE_HEADER)
    );
    assert_eq!(video[0].1.body, vec![1, 2, 3]);
    assert_eq!(video[1].0, 40);
    assert_eq!(video[1].1.body, vec![9, 9, 9, 9]);
    assert_eq!(video[2].0, 80);
    assert_eq!(video[2].1.frame_type, VIDEO_FRAME_INTER);
    assert_eq!(audio.len(), 2);
    assert_eq!(audio[0].1.body, vec![0x12, 0x10]);
    assert_eq!(audio[1].0, 23);
    assert_eq!(audio[1].1.body, vec![0xAA, 0xBB]);

    player.close().expect("close player");
    server_thread.join().expect("server thread");
}

/// Pause / seek / resume control loop: each §4.2 command surfaces
/// server-side as a typed event, the notify replies come back as
/// [`PlayerPacket::Status`] with the spec code strings, and the
/// receiveAudio / receiveVideo toggles arrive typed too.
#[test]
fn player_pause_seek_resume_round_trip() {
    let server = RtmpServer::bind("127.0.0.1:0").expect("bind");
    let addr = server.local_addr().expect("local_addr");

    let server_thread = thread::spawn(move || {
        let req = match server.accept_any().expect("accept_any") {
            SessionRequest::Play(req) => req,
            SessionRequest::Publish(_) => panic!("expected play"),
        };
        let mut session = req.accept().expect("accept");
        session
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("set_read_timeout");

        // pause(true, 1000).
        match session.next_event().expect("event 1") {
            Some(PlaySessionEvent::Command(NetStreamCommand::Pause {
                pause: true,
                milliseconds,
            })) => assert_eq!(milliseconds, 1000.0),
            other => panic!("expected pause, got {other:?}"),
        }
        session.notify_pause(true).expect("notify pause");

        // seek(5000).
        match session.next_event().expect("event 2") {
            Some(PlaySessionEvent::Command(NetStreamCommand::Seek { milliseconds })) => {
                assert_eq!(milliseconds, 5000.0)
            }
            other => panic!("expected seek, got {other:?}"),
        }
        session.notify_seek().expect("notify seek");

        // pause(false, 5000) = resume.
        match session.next_event().expect("event 3") {
            Some(PlaySessionEvent::Command(NetStreamCommand::Pause {
                pause: false,
                milliseconds,
            })) => assert_eq!(milliseconds, 5000.0),
            other => panic!("expected resume, got {other:?}"),
        }
        session.notify_pause(false).expect("notify unpause");

        // receiveAudio(false) + receiveVideo(false): per §4.2.4/§4.2.5
        // "the server does not send any response" for false.
        match session.next_event().expect("event 4") {
            Some(PlaySessionEvent::Command(NetStreamCommand::ReceiveAudio(false))) => {}
            other => panic!("expected receiveAudio(false), got {other:?}"),
        }
        match session.next_event().expect("event 5") {
            Some(PlaySessionEvent::Command(NetStreamCommand::ReceiveVideo(false))) => {}
            other => panic!("expected receiveVideo(false), got {other:?}"),
        }

        // Mid-stream SetBufferLength re-announcement.
        match session.next_event().expect("event 6") {
            Some(PlaySessionEvent::SetBufferLength { buffer_ms }) => {
                assert_eq!(buffer_ms, 7000)
            }
            other => panic!("expected SetBufferLength, got {other:?}"),
        }

        // receiveAudio(true): §4.2.4 mandates the two-status reply
        // (Seek.Notify then Play.Start) — notify_receive_resumed.
        match session.next_event().expect("event 7") {
            Some(PlaySessionEvent::Command(NetStreamCommand::ReceiveAudio(true))) => {}
            other => panic!("expected receiveAudio(true), got {other:?}"),
        }
        session
            .notify_receive_resumed()
            .expect("notify receive resumed");

        session.close().expect("close");
    });

    let url = format!("rtmp://{addr}/live/key");
    let mut player = RtmpPlayer::connect(&url).expect("connect play");
    assert!(!player.is_recorded());
    player
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("set_read_timeout");

    let expect_status = |player: &mut RtmpPlayer, want: &str| loop {
        match player.next_packet().expect("next_packet") {
            Some(PlayerPacket::Status { level, code, .. }) => {
                assert_eq!(level, "status");
                assert_eq!(code, want);
                break;
            }
            Some(_) => continue,
            None => panic!("stream ended while waiting for {want}"),
        }
    };

    player.pause(1000.0).expect("pause");
    expect_status(&mut player, STATUS_PAUSE_NOTIFY);
    player.seek(5000.0).expect("seek");
    expect_status(&mut player, STATUS_SEEK_NOTIFY);
    player.resume(5000.0).expect("resume");
    expect_status(&mut player, STATUS_UNPAUSE_NOTIFY);
    player.set_receive_audio(false).expect("receiveAudio");
    player.set_receive_video(false).expect("receiveVideo");
    player.set_buffer_length(7000).expect("set_buffer_length");

    // receiveAudio(true) → §4.2.4: "server responds with status
    // messages NetStream.Seek.Notify and NetStream.Play.Start", in
    // that order.
    player.set_receive_audio(true).expect("receiveAudio true");
    expect_status(&mut player, STATUS_SEEK_NOTIFY);
    expect_status(&mut player, STATUS_PLAY_START);

    // Server closes with StreamEOF → clean end.
    assert!(player.next_packet().expect("final").is_none());
    player.close().expect("close player");
    server_thread.join().expect("server thread");
}

/// A rejected play request surfaces as [`Error::Rejected`] carrying
/// the §4.2.1 `NetStream.Play.StreamNotFound` code.
#[test]
fn player_surfaces_stream_not_found_rejection() {
    let server = RtmpServer::bind("127.0.0.1:0").expect("bind");
    let addr = server.local_addr().expect("local_addr");

    let server_thread = thread::spawn(move || {
        let req = match server.accept_any().expect("accept_any") {
            SessionRequest::Play(req) => req,
            SessionRequest::Publish(_) => panic!("expected play"),
        };
        // Refuse: no such stream.
        let _ = req.reject("no such stream");
    });

    let url = format!("rtmp://{addr}/vod/missing");
    match RtmpPlayer::connect(&url) {
        Err(Error::Rejected(reason)) => {
            assert!(
                reason.contains("NetStream.Play.StreamNotFound"),
                "rejection reason must carry the spec code, got: {reason}"
            );
            assert!(reason.contains("no such stream"));
        }
        Ok(_) => panic!("play must be refused"),
        Err(other) => panic!("expected Rejected, got {other:?}"),
    }
    server_thread.join().expect("server thread");
}

/// The player's `close()` sends the §4.2.3 `deleteStream`, which ends
/// the server-side event loop the same way `closeStream` does.
#[test]
fn player_close_ends_server_session_via_delete_stream() {
    let server = RtmpServer::bind("127.0.0.1:0").expect("bind");
    let addr = server.local_addr().expect("local_addr");

    let (tx, rx) = mpsc::channel::<bool>();
    let server_thread = thread::spawn(move || {
        let req = match server.accept_any().expect("accept_any") {
            SessionRequest::Play(req) => req,
            SessionRequest::Publish(_) => panic!("expected play"),
        };
        let mut session = req.accept().expect("accept");
        session
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("set_read_timeout");
        // deleteStream must end the loop with Ok(None), not surface as
        // a command.
        let ended_clean = matches!(session.next_event(), Ok(None));
        let _ = tx.send(ended_clean);
    });

    let url = format!("rtmp://{addr}/live/key");
    let player = RtmpPlayer::connect(&url).expect("connect play");
    player.close().expect("close");

    let ended_clean = rx.recv_timeout(Duration::from_secs(5)).expect("recv");
    assert!(ended_clean, "deleteStream must end the play session");
    server_thread.join().expect("server thread");
}

/// §4.2.1 dynamic playlists + §4.2.2 play2: a mid-session `play`
/// (reset = false) and a `play2` bitrate switch both surface
/// server-side as typed commands with their arguments intact, and the
/// server's `NetStream.Play.Start` for the queued entry reaches the
/// player as a status event.
#[test]
fn player_playlist_switch_and_play2_round_trip() {
    let server = RtmpServer::bind("127.0.0.1:0").expect("bind");
    let addr = server.local_addr().expect("local_addr");

    let server_thread = thread::spawn(move || {
        let req = match server.accept_any().expect("accept_any") {
            SessionRequest::Play(req) => req,
            SessionRequest::Publish(_) => panic!("expected play"),
        };
        assert_eq!(req.stream_name, "clip-1");
        let mut session = req.accept().expect("accept");
        session
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("set_read_timeout");

        // Playlist continuation: play("clip-2", reset = false).
        match session.next_event().expect("event 1") {
            Some(PlaySessionEvent::Command(NetStreamCommand::Play {
                stream_name,
                start,
                duration,
                reset,
            })) => {
                assert_eq!(stream_name, "clip-2");
                // Omitted optionals materialise at spec defaults on
                // the wire because `reset` forces the positions.
                assert_eq!(start, Some(-2.0));
                assert_eq!(duration, Some(-1.0));
                assert_eq!(reset, Some(false));
            }
            other => panic!("expected play command, got {other:?}"),
        }
        session
            .send_status(STATUS_PLAY_START, "Started playing clip-2")
            .expect("Play.Start for clip-2");

        // §4.2.2 play2 with the parameter object preserved verbatim.
        match session.next_event().expect("event 2") {
            Some(PlaySessionEvent::Command(NetStreamCommand::Play2(params))) => {
                assert_eq!(
                    params.get("streamName").and_then(Amf0Value::as_str),
                    Some("clip-2-hi")
                );
                assert_eq!(
                    params.get("transition").and_then(Amf0Value::as_str),
                    Some("switch")
                );
                assert_eq!(params.get("start").and_then(Amf0Value::as_f64), Some(0.0));
            }
            other => panic!("expected play2 command, got {other:?}"),
        }

        session.close().expect("close");
    });

    let url = format!("rtmp://{addr}/vod/clip-1");
    let mut player = RtmpPlayer::connect(&url).expect("connect play");
    player
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("set_read_timeout");

    player
        .play("clip-2", None, None, Some(false))
        .expect("playlist play");
    assert_eq!(player.stream_name(), "clip-2");
    // The server's Play.Start for the queued entry surfaces as Status.
    loop {
        match player.next_packet().expect("next_packet") {
            Some(PlayerPacket::Status { code, .. }) => {
                assert_eq!(code, "NetStream.Play.Start");
                break;
            }
            Some(_) => continue,
            None => panic!("stream ended before Play.Start for clip-2"),
        }
    }

    let params = Amf0Value::Object(vec![
        ("len".into(), Amf0Value::Number(-1.0)),
        ("offset".into(), Amf0Value::Number(0.0)),
        ("start".into(), Amf0Value::Number(0.0)),
        ("streamName".into(), Amf0Value::String("clip-2-hi".into())),
        ("transition".into(), Amf0Value::String("switch".into())),
    ]);
    player.play2(params).expect("play2");

    // Server closes with StreamEOF once satisfied.
    assert!(player.next_packet().expect("final").is_none());
    player.close().expect("close player");
    server_thread.join().expect("server thread");
}
