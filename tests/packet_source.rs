//! End-to-end check of the `PacketSource` adapter.
//!
//! 1. Pick an ephemeral local port via a one-shot `TcpListener::bind`,
//!    then drop it so the registry opener can grab it.
//! 2. Spawn a publisher thread that pushes a small synthetic stream
//!    via [`RtmpClient`].
//! 3. Open `rtmp://127.0.0.1:N/live/key` through a real
//!    `SourceRegistry` and verify:
//!     * the resulting `SourceOutput::Packets` carries audio +
//!       video stream descriptors with the right codec ids;
//!     * `next_packet()` produces the expected packets in order
//!       with nanosecond pts/dts (`ms * RTMP_MS_TO_NS`) and the
//!       keyframe + header flags set on the right ones;
//!     * the iterator terminates with [`oxideav_core::Error::Eof`]
//!       once the publisher disconnects cleanly.
//!
//! This exercises the listen-style opener, the probing loop that
//! discovers stream codecs from the first audio + video tags, and
//! the StreamPacket → Packet conversion.

use std::net::TcpListener;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use oxideav_core::{Error as CoreError, SourceOutput, SourceRegistry};
use oxideav_rtmp::{
    register, RtmpClient, AUDIO_STREAM_INDEX, RTMP_MS_TO_NS, RTMP_TIME_BASE, VIDEO_STREAM_INDEX,
};

/// Reserve an ephemeral port by binding a TcpListener and then
/// dropping it. Race-prone in theory; in practice modern kernels
/// don't immediately re-issue a freed port and the gap between
/// drop and the registry opener's `bind` is small enough that
/// CI reliably succeeds. The same trick is used in
/// `loopback_publish.rs`.
fn pick_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("scratch bind");
    let port = listener.local_addr().expect("local_addr").port();
    drop(listener);
    port
}

const APP: &str = "live";
const STREAM_KEY: &str = "secret-key";

#[test]
fn registry_open_rtmp_url_yields_packet_source_with_expected_packets() {
    let port = pick_port();
    let url = format!("rtmp://127.0.0.1:{port}/{APP}/{STREAM_KEY}");

    // Channel so the publisher thread can hand any error back to
    // the test for assertion (rather than panic in another thread
    // and produce a confusing failure mode).
    let (tx, rx) = mpsc::channel::<Result<(), String>>();
    let push_url = url.clone();
    let publisher = thread::spawn(move || {
        // Brief warm-up: give the registry opener a beat to bind
        // the listener before we dial.
        thread::sleep(Duration::from_millis(150));
        let mut client = match RtmpClient::connect(&push_url) {
            Ok(c) => c,
            Err(e) => {
                let _ = tx.send(Err(format!("client connect: {e}")));
                return;
            }
        };
        // Inline error helper so each step keeps the test concise.
        let push = |c: &mut RtmpClient| -> Result<(), String> {
            let avc_c = b"\x01\x42\x80\x1e\x00".to_vec();
            c.send_video_sequence_header(&avc_c)
                .map_err(|e| format!("send avc seq: {e}"))?;
            let asc = vec![0x12, 0x10];
            c.send_audio_sequence_header(&asc)
                .map_err(|e| format!("send aac seq: {e}"))?;
            // Three video frames: keyframe, inter, inter.
            let nalu_k: Vec<u8> = (0..200).map(|i| i as u8).collect();
            let nalu_p: Vec<u8> = (0u32..120).map(|i| (i.wrapping_mul(3)) as u8).collect();
            let nalu_q: Vec<u8> = (0u32..80)
                .map(|i| (i.wrapping_mul(7).wrapping_add(1)) as u8)
                .collect();
            c.send_video(0, true, &nalu_k)
                .map_err(|e| format!("send video k: {e}"))?;
            c.send_video(33, false, &nalu_p)
                .map_err(|e| format!("send video p: {e}"))?;
            c.send_video(66, false, &nalu_q)
                .map_err(|e| format!("send video q: {e}"))?;
            // Two AAC frames.
            let aac_a: Vec<u8> = (0..180)
                .map(|i: u32| ((i.wrapping_add(5)).wrapping_mul(11)) as u8)
                .collect();
            let aac_b: Vec<u8> = (0..180)
                .map(|i: u32| ((i.wrapping_add(40)).wrapping_mul(13)) as u8)
                .collect();
            c.send_audio(20, &aac_a)
                .map_err(|e| format!("send audio a: {e}"))?;
            c.send_audio(43, &aac_b)
                .map_err(|e| format!("send audio b: {e}"))?;
            Ok(())
        };
        let push_result = push(&mut client);
        // Give the kernel a beat to drain the final A/V chunks
        // through the socket buffer before we send the
        // closeStream + Shutdown::Write FIN. Without this, a
        // contended Ubuntu CI runner can occasionally observe
        // the FIN reach the peer ahead of the last buffered
        // chunk's data segment — the receiver then sees EOF
        // before the trailing audio frame arrives.
        thread::sleep(Duration::from_millis(50));
        let result =
            push_result.and_then(|()| client.close().map_err(|e| format!("client close: {e}")));
        let _ = tx.send(result);
    });

    // Build a registry and run the opener — synchronous accept.
    let mut reg = SourceRegistry::new();
    register(&mut reg);
    let mut source = match reg.open(&url).expect("open rtmp source") {
        SourceOutput::Packets(p) => p,
        other => panic!(
            "expected SourceOutput::Packets, got {:?}",
            std::mem::discriminant(&other)
        ),
    };

    // After probing we expect both stream descriptors. Order is
    // stable: audio first (index 0), video second (index 1).
    let streams = source.streams().to_vec();
    assert_eq!(streams.len(), 2, "expected 2 streams (audio+video)");
    assert_eq!(streams[0].index, AUDIO_STREAM_INDEX);
    assert_eq!(streams[0].time_base, RTMP_TIME_BASE);
    assert_eq!(streams[0].params.codec_id.as_str(), "aac");
    assert_eq!(streams[1].index, VIDEO_STREAM_INDEX);
    assert_eq!(streams[1].time_base, RTMP_TIME_BASE);
    assert_eq!(streams[1].params.codec_id.as_str(), "h264");
    // Sequence-header extradata should be on both stream params.
    assert_eq!(streams[0].params.extradata, vec![0x12, 0x10]);
    assert_eq!(
        streams[1].params.extradata,
        b"\x01\x42\x80\x1e\x00".to_vec()
    );

    // Drain packets in arrival order until EOF. A 5s overall cap
    // keeps a hung test from holding CI; in practice this finishes
    // in tens of milliseconds.
    let mut packets = Vec::new();
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        if std::time::Instant::now() >= deadline {
            panic!(
                "deadline before EOF; got {} packets so far: {:?}",
                packets.len(),
                packets
                    .iter()
                    .map(|p: &oxideav_core::Packet| (p.stream_index, p.pts))
                    .collect::<Vec<_>>()
            );
        }
        match source.next_packet() {
            Ok(p) => packets.push(p),
            Err(CoreError::Eof) => break,
            Err(e) => panic!("unexpected error from next_packet(): {e}"),
        }
    }
    drop(source);
    publisher.join().expect("publisher thread joined");
    rx.recv_timeout(Duration::from_secs(1))
        .expect("publisher result delivered")
        .expect("publisher succeeded");

    // Sort packets by stream for assertion clarity. We expect:
    //   audio: seq_header (pts 0), raw a (pts 20), raw b (pts 43)
    //   video: seq_header (pts 0), keyframe k (pts 0), inter p (pts 33), inter q (pts 66)
    let audio: Vec<_> = packets
        .iter()
        .filter(|p| p.stream_index == AUDIO_STREAM_INDEX)
        .collect();
    let video: Vec<_> = packets
        .iter()
        .filter(|p| p.stream_index == VIDEO_STREAM_INDEX)
        .collect();
    assert_eq!(
        audio.len(),
        3,
        "expected 3 audio packets, got {}",
        audio.len()
    );
    assert_eq!(
        video.len(),
        4,
        "expected 4 video packets, got {}",
        video.len()
    );

    // pts/dts are emitted on the nanosecond RTMP_TIME_BASE timeline;
    // ms * RTMP_MS_TO_NS recovers the wire ms value.
    // Audio[0] = AAC sequence header (header flag set, pts/dts 0).
    assert_eq!(audio[0].pts, Some(0));
    assert_eq!(audio[0].dts, Some(0));
    assert!(audio[0].flags.header);
    // 1 byte AAC packet-type marker (0 = seq header) + 2 byte ASC.
    assert_eq!(audio[0].data, vec![0x00, 0x12, 0x10]);
    // Audio[1] = raw AAC at pts 20 ms.
    assert_eq!(audio[1].pts, Some(20 * RTMP_MS_TO_NS));
    assert!(!audio[1].flags.header);
    assert_eq!(audio[1].data[0], 0x01); // raw marker
                                        // Audio[2] = raw AAC at pts 43 ms.
    assert_eq!(audio[2].pts, Some(43 * RTMP_MS_TO_NS));

    // Video[0] = AVC sequence header (keyframe + header flags, pts/dts 0).
    assert_eq!(video[0].pts, Some(0));
    assert_eq!(video[0].dts, Some(0));
    assert!(video[0].flags.keyframe);
    assert!(video[0].flags.header);
    assert_eq!(video[0].data, b"\x01\x42\x80\x1e\x00".to_vec());
    // Video[1] = keyframe NALU at pts 0.
    assert_eq!(video[1].pts, Some(0));
    assert!(video[1].flags.keyframe);
    assert!(!video[1].flags.header);
    let nalu_k: Vec<u8> = (0..200).map(|i| i as u8).collect();
    assert_eq!(video[1].data, nalu_k);
    // Video[2] = inter at pts 33 ms.
    assert_eq!(video[2].pts, Some(33 * RTMP_MS_TO_NS));
    assert!(!video[2].flags.keyframe);
    let nalu_p: Vec<u8> = (0u32..120).map(|i| (i.wrapping_mul(3)) as u8).collect();
    assert_eq!(video[2].data, nalu_p);
    // Video[3] = inter at pts 66 ms.
    assert_eq!(video[3].pts, Some(66 * RTMP_MS_TO_NS));
    let nalu_q: Vec<u8> = (0u32..80)
        .map(|i| (i.wrapping_mul(7).wrapping_add(1)) as u8)
        .collect();
    assert_eq!(video[3].data, nalu_q);
}

/// Mismatched stream key on the publisher side must be rejected
/// by the opener (rather than silently accepted), and the
/// resulting error mentions "stream-name".
#[test]
fn registry_open_rtmp_rejects_mismatched_stream_key() {
    let port = pick_port();
    let url = format!("rtmp://127.0.0.1:{port}/{APP}/{STREAM_KEY}");
    let bad_url = format!("rtmp://127.0.0.1:{port}/{APP}/wrong-key");

    let publisher = thread::spawn(move || {
        thread::sleep(Duration::from_millis(150));
        // Connect with the wrong key. The server should reject;
        // the client may still report success because the reject
        // only fires after publish — accept either outcome here.
        let _ = RtmpClient::connect(&bad_url);
    });

    let mut reg = SourceRegistry::new();
    register(&mut reg);
    let result = reg.open(&url);
    match result {
        Ok(_) => panic!("expected open() to fail on stream-name mismatch"),
        Err(CoreError::InvalidData(msg)) => {
            assert!(
                msg.contains("stream-name"),
                "error should mention stream-name: got {msg:?}"
            );
        }
        Err(e) => panic!("expected InvalidData, got {e}"),
    }
    publisher.join().expect("publisher thread joined");
}
