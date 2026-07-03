//! Full-stack RTMP relay: a real [`RtmpClient`] publisher pushes into
//! a real [`RtmpServer`], which fans the stream out to a real
//! [`RtmpPlayer`] subscriber via [`PlaySession::forward`] — the
//! publish → ingest → play pipeline every broadcast deployment runs.
//!
//! Proves that the two directions compose: interleaved audio / video
//! / metadata packets ingested on the publish side re-frame on the
//! subscriber's stream id with their timestamps intact, sequence
//! headers survive the hop byte-for-byte, and the publisher's clean
//! teardown propagates to the subscriber as a §7.1.7 `StreamEOF`
//! (surfacing as `Ok(None)` from [`RtmpPlayer::next_packet`]).

use std::thread;
use std::time::Duration;

use oxideav_rtmp::{
    Amf0Value, PlayerPacket, RtmpClient, RtmpPlayer, RtmpServer, SessionRequest, StreamPacket,
};

#[test]
fn publisher_to_subscriber_relay_preserves_stream() {
    let server = RtmpServer::bind("127.0.0.1:0").expect("bind");
    let addr = server.local_addr().expect("local_addr");

    // Relay thread: accept the publisher first, then the subscriber,
    // then pump every publisher packet into the play session. Once all
    // 7 expected packets have been forwarded, signal the main thread —
    // the publisher must not close (and drop its socket) until the
    // ingest has drained everything, or the close-time RST can discard
    // the server's still-unread receive queue on some platforms.
    let (drained_tx, drained_rx) = std::sync::mpsc::channel::<()>();
    let relay_thread = thread::spawn(move || {
        let mut publish = match server.accept_any().expect("accept publisher") {
            SessionRequest::Publish(req) => {
                assert_eq!(req.app, "live");
                assert_eq!(req.stream_name, "relay-key");
                req.accept().expect("accept publish")
            }
            SessionRequest::Play(_) => panic!("publisher must arrive first"),
        };
        publish
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("publish timeout");

        let mut play = match server.accept_any().expect("accept subscriber") {
            SessionRequest::Play(req) => {
                assert_eq!(req.app, "live");
                assert_eq!(req.stream_name, "relay-key");
                req.accept().expect("accept play")
            }
            SessionRequest::Publish(_) => panic!("subscriber must arrive second"),
        };

        let mut forwarded = 0usize;
        while let Some(pkt) = publish.next_packet().expect("ingest packet") {
            play.forward(&pkt).expect("forward");
            if !matches!(pkt, StreamPacket::Command(_)) {
                forwarded += 1;
                if forwarded == 7 {
                    // Everything the publisher pushed has been read off
                    // the ingest socket — it is now safe to close.
                    let _ = drained_tx.send(());
                }
            }
        }
        play.close().expect("close play");
        forwarded
    });

    // Publisher: metadata + AVC/AAC sequence headers + interleaved
    // frames, then a clean close.
    let pub_url = format!("rtmp://{addr}/live/relay-key");
    let mut publisher = RtmpClient::connect(&pub_url).expect("connect publisher");
    let meta = Amf0Value::EcmaArray(vec![
        ("width".into(), Amf0Value::Number(640.0)),
        ("height".into(), Amf0Value::Number(360.0)),
        ("framerate".into(), Amf0Value::Number(25.0)),
    ]);
    publisher.send_metadata(meta).expect("metadata");
    publisher
        .send_video_sequence_header(&[0x01, 0x64, 0x00, 0x1F, 0xFF])
        .expect("avcC");
    publisher
        .send_audio_sequence_header(&[0x12, 0x10])
        .expect("asc");
    publisher
        .send_video(0, true, &[0, 0, 0, 3, 0x65, 0x11, 0x22])
        .expect("v0");
    publisher.send_audio(0, &[0xA0, 0xA1]).expect("a0");
    publisher
        .send_video(40, false, &[0, 0, 0, 2, 0x41, 0x33])
        .expect("v1");
    publisher.send_audio(23, &[0xB0, 0xB1, 0xB2]).expect("a1");

    // Subscriber connects once the publish is live.
    let play_url = format!("rtmp://{addr}/live/relay-key");
    let mut player = RtmpPlayer::connect(&play_url).expect("connect player");
    player
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("player timeout");

    // Wait until the relay has drained all 7 packets off the ingest
    // socket, then finish the publish; the relay observes the clean
    // teardown and closes the play session.
    drained_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("relay must drain the published packets");
    publisher.close().expect("close publisher");

    let mut metadata = None;
    let mut video: Vec<(u32, Vec<u8>)> = Vec::new();
    let mut audio: Vec<(u32, Vec<u8>)> = Vec::new();
    while let Some(pkt) = player.next_packet().expect("player packet") {
        match pkt {
            PlayerPacket::Metadata(m) => metadata = Some(m),
            PlayerPacket::Video { timestamp, tag } => video.push((timestamp, tag.body)),
            PlayerPacket::Audio { timestamp, tag } => audio.push((timestamp, tag.body)),
            PlayerPacket::Status { .. } | PlayerPacket::Control(_) => {}
        }
    }

    // Everything the publisher pushed arrived, in order, with the
    // original timestamps and byte-exact bodies.
    let meta = metadata.expect("relayed onMetaData");
    assert_eq!(meta.get("width").and_then(Amf0Value::as_f64), Some(640.0));
    assert_eq!(
        meta.get("framerate").and_then(Amf0Value::as_f64),
        Some(25.0)
    );
    assert_eq!(
        video,
        vec![
            (0, vec![0x01, 0x64, 0x00, 0x1F, 0xFF]),
            (0, vec![0, 0, 0, 3, 0x65, 0x11, 0x22]),
            (40, vec![0, 0, 0, 2, 0x41, 0x33]),
        ],
    );
    assert_eq!(
        audio,
        vec![
            (0, vec![0x12, 0x10]),
            (0, vec![0xA0, 0xA1]),
            (23, vec![0xB0, 0xB1, 0xB2]),
        ],
    );

    let forwarded = relay_thread.join().expect("relay thread");
    // 1 metadata + 3 video + 3 audio.
    assert_eq!(forwarded, 7);
    player.close().expect("close player");
}
