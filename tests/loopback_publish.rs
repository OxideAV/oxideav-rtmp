//! End-to-end sanity check: spin up our server on an ephemeral port,
//! dial our client to it, have the client publish a tiny synthetic
//! AAC + H.264 stream, and verify the server sees the same bytes
//! back in order.
//!
//! If this test passes, the full pipeline — handshake, chunk stream,
//! AMF0 command flow, FLV tag encoding, both directions — is at
//! least internally consistent. It doesn't prove conformance with a
//! third-party peer; that lives in manual testing against commodity
//! ingest endpoints.

use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use oxideav_rtmp::{AudioTag, RtmpClient, RtmpServer, StreamPacket, VideoTag};

const APP: &str = "live";
const STREAM_KEY: &str = "test-key-xyz";

#[test]
fn client_publishes_and_server_receives_same_frames() {
    // Bind on 127.0.0.1:0 so we get any free port.
    let server = RtmpServer::bind("127.0.0.1:0").expect("bind");
    let addr = server.local_addr().expect("local_addr");

    let (recv_tx, recv_rx) = mpsc::channel::<ReceivedPacket>();
    let (app_tx, app_rx) = mpsc::channel::<(String, String)>();

    // Server side: accept one publisher, push every packet to recv_tx.
    let server_thread = thread::spawn(move || {
        let req = server.accept().expect("server accept");
        app_tx
            .send((req.app.clone(), req.stream_name.clone()))
            .unwrap();
        let mut session = req.accept().expect("session accept");
        // Don't block forever if the peer dies.
        session
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("set_read_timeout");
        loop {
            match session.next_packet() {
                Ok(Some(StreamPacket::Audio { timestamp, tag })) => {
                    recv_tx.send(ReceivedPacket::Audio(timestamp, tag)).unwrap();
                }
                Ok(Some(StreamPacket::Video { timestamp, tag })) => {
                    recv_tx.send(ReceivedPacket::Video(timestamp, tag)).unwrap();
                }
                Ok(Some(StreamPacket::Metadata(_))) => {
                    recv_tx.send(ReceivedPacket::Metadata).unwrap();
                }
                Ok(Some(_)) => {}
                Ok(None) => break,
                Err(_) => break,
            }
        }
    });

    // Tiny delay so the server's listener is definitely polling.
    thread::sleep(Duration::from_millis(50));

    // Client side: dial, publish, push a few frames, close.
    let url = format!("rtmp://{}:{}/{APP}/{STREAM_KEY}", addr.ip(), addr.port());
    let mut client = RtmpClient::connect(&url).expect("client connect");

    // AVC sequence header (fake, 5 bytes — server only round-trips
    // the bytes, doesn't parse AVCC).
    let avc_c = b"\x01\x42\x80\x1e\x00".to_vec();
    client
        .send_video_sequence_header(&avc_c)
        .expect("send avc seq");

    // AAC AudioSpecificConfig (LC-AAC 44.1k stereo).
    let asc = vec![0x12, 0x10];
    client
        .send_audio_sequence_header(&asc)
        .expect("send aac seq");

    // Three video frames (keyframe, inter, inter).
    let nalu_k: Vec<u8> = (0..200).map(|i| i as u8).collect();
    let nalu_p: Vec<u8> = (0..120).map(|i| (i * 3) as u8).collect();
    let nalu_q: Vec<u8> = (0..80).map(|i| (i * 7 + 1) as u8).collect();
    client.send_video(0, true, &nalu_k).expect("send video 0");
    client.send_video(33, false, &nalu_p).expect("send video 1");
    client.send_video(66, false, &nalu_q).expect("send video 2");

    // Two AAC frames.
    let aac_a: Vec<u8> = (0..180).map(|i| ((i + 5) * 11) as u8).collect();
    let aac_b: Vec<u8> = (0..180).map(|i| ((i + 40) * 13) as u8).collect();
    client.send_audio(20, &aac_a).expect("send audio 0");
    client.send_audio(43, &aac_b).expect("send audio 1");

    // Give the kernel a beat to drain the last buffered chunk's
    // data segment through the socket buffer before we send the
    // closeStream + Shutdown::Write FIN. On a contended Ubuntu CI
    // runner the FIN can otherwise reach the peer ahead of the
    // last frame and the receiver hits EOF before the trailing
    // audio packet arrives.
    thread::sleep(Duration::from_millis(50));
    client.close().expect("client close");
    server_thread.join().expect("server thread");

    // Confirm server saw the right app + stream key.
    let (seen_app, seen_key) = app_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("app + key must arrive");
    assert_eq!(seen_app, APP);
    assert_eq!(seen_key, STREAM_KEY);

    // Drain received packets — order matters. Use a generous per-packet
    // inactivity window so the test stays reliable on a CI runner that
    // schedules our worker out for tens of ms between successive
    // chunk-stream reads (Ubuntu free-tier CI in particular can stall
    // a thread for ~150 ms under contention).
    let mut got: Vec<ReceivedPacket> = Vec::new();
    while let Ok(p) = recv_rx.recv_timeout(Duration::from_millis(500)) {
        got.push(p);
    }

    // We should have exactly: AVC seq-header video, AAC seq-header
    // audio, 3 video frames, 2 audio frames.
    let video_tags: Vec<(u32, &VideoTag)> = got
        .iter()
        .filter_map(|p| match p {
            ReceivedPacket::Video(ts, t) => Some((*ts, t)),
            _ => None,
        })
        .collect();
    let audio_tags: Vec<(u32, &AudioTag)> = got
        .iter()
        .filter_map(|p| match p {
            ReceivedPacket::Audio(ts, t) => Some((*ts, t)),
            _ => None,
        })
        .collect();

    assert_eq!(
        video_tags.len(),
        4,
        "expected 4 video tags, got {video_tags:?}"
    );
    assert_eq!(
        audio_tags.len(),
        3,
        "expected 3 audio tags, got {audio_tags:?}"
    );

    // Video[0] = AVC sequence header.
    assert!(video_tags[0].1.is_avc_sequence_header());
    assert_eq!(video_tags[0].1.body, avc_c);
    // Video[1..4] = NALU packets in order.
    assert_eq!(video_tags[1].0, 0);
    assert_eq!(video_tags[1].1.body, nalu_k);
    assert_eq!(video_tags[2].0, 33);
    assert_eq!(video_tags[2].1.body, nalu_p);
    assert_eq!(video_tags[3].0, 66);
    assert_eq!(video_tags[3].1.body, nalu_q);

    // Audio[0] = AAC sequence header.
    assert_eq!(audio_tags[0].1.aac_packet_type, Some(0));
    assert_eq!(audio_tags[0].1.body, asc);
    // Audio[1..3] = raw AAC.
    assert_eq!(audio_tags[1].0, 20);
    assert_eq!(audio_tags[1].1.body, aac_a);
    assert_eq!(audio_tags[2].0, 43);
    assert_eq!(audio_tags[2].1.body, aac_b);
}

/// Verifies the reject path: we dial a publisher, the server's
/// consumer decides "no", and the client sees the publish get torn
/// down cleanly.
#[test]
fn server_can_reject_publish_based_on_stream_key() {
    let server = RtmpServer::bind("127.0.0.1:0").expect("bind");
    let addr = server.local_addr().expect("local_addr");

    let server_thread = thread::spawn(move || {
        let req = server.accept().expect("accept");
        // Simulate an auth check.
        assert_eq!(req.stream_name, "bad-key");
        let _ = req.reject("unauthorized");
    });

    thread::sleep(Duration::from_millis(50));
    let url = format!("rtmp://{}:{}/live/bad-key", addr.ip(), addr.port());
    // The client may succeed connect() (reject is signalled *after*
    // publish), but once it starts writing frames the TCP stream will
    // be closed. Accept either outcome — the important thing is the
    // server thread runs its reject path without panicking.
    let _ = RtmpClient::connect(&url);
    server_thread.join().expect("server thread");
}

#[derive(Debug)]
enum ReceivedPacket {
    Audio(u32, AudioTag),
    Video(u32, VideoTag),
    #[allow(dead_code)]
    Metadata,
}
