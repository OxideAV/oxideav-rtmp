//! Symmetric end-of-stream recognition on the client side.
//!
//! Round 154 added the server-side teardown that emits a
//! `UserControl StreamEOF(stream_id)` (RTMP 1.0 §7.1.7) before
//! `onStatus(NetStream.Unpublish.Success)` and the write-half FIN. The
//! pre-r158 client was happy to read the FIN but silently dropped the
//! StreamEOF on the floor — leaving it on the chunk-state machine's
//! buffer until the underlying `TcpStream` dropped. That means a
//! publisher couldn't distinguish "server cleanly closed our publish"
//! from "TCP connection died unexpectedly."
//!
//! This test verifies the new [`RtmpClient::poll_event`] surface:
//!   * Bind our [`RtmpServer`], connect our [`RtmpClient`].
//!   * Server closes the session via [`RtmpSession::close`], which
//!     emits `StreamEOF(1)` followed by `onStatus(...Unpublish.Success)`.
//!   * Client polls events and observes
//!     [`ClientEvent::StreamEof { stream_id: 1 }`] AND then `Ok(None)`
//!     on a subsequent poll once the read half drains, so an outer
//!     event loop can drop the client cleanly.
//!
//! The two-way contract — server emits StreamEOF on its side, client
//! recognises StreamEOF as a clean end on its side — matches the
//! framing the rest of the commodity streaming ecosystem has used in
//! practice for end-of-publish since 2009-ish.

use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use oxideav_rtmp::{ClientEvent, RtmpClient, RtmpServer};

const APP: &str = "live";
const STREAM_KEY: &str = "client-eof-test";

#[test]
fn client_observes_stream_eof_from_server_clean_close() {
    let server = RtmpServer::bind("127.0.0.1:0").expect("bind");
    let addr = server.local_addr().expect("local_addr");

    let (started_tx, started_rx) = mpsc::channel::<()>();

    let server_thread = thread::spawn(move || {
        let req = server.accept().expect("server accept");
        let session = req.accept().expect("session accept");
        // Let the client finish the publish handshake before we tear
        // down. We don't push any A/V — just close cleanly so the only
        // post-publish bytes the client sees are
        // StreamEOF + Unpublish.Success.
        started_tx.send(()).unwrap();
        thread::sleep(Duration::from_millis(80));
        session.close().expect("session close");
    });

    thread::sleep(Duration::from_millis(50));

    let url = format!("rtmp://{}:{}/{APP}/{STREAM_KEY}", addr.ip(), addr.port());
    let mut client = RtmpClient::connect(&url).expect("client connect");
    started_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("server signal");

    // Cap how long any single poll can wait for bytes on the socket.
    client
        .inner_mut()
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("set_read_timeout");

    // Pump events until we hit StreamEof or Ok(None). We tolerate a
    // small handful of `Other` events first — ack windows, peer
    // bandwidth, and the server's own StreamBegin for the publish
    // stream all show up here.
    let mut saw_stream_eof = false;
    let mut saw_unpublish_success = false;
    let mut saw_stream_begin = false;
    for _ in 0..32 {
        match client.poll_event() {
            Ok(Some(ClientEvent::StreamEof { stream_id })) => {
                // The server-side createStream reply assigned stream id 1.
                assert_eq!(stream_id, 1, "StreamEOF must carry the publish stream id");
                saw_stream_eof = true;
            }
            Ok(Some(ClientEvent::OnStatus {
                level,
                code,
                description,
            })) => {
                // Should match the close path's onStatus payload from
                // crate::server::RtmpSession::close.
                if code == "NetStream.Unpublish.Success" {
                    assert_eq!(level, "status");
                    assert!(!description.is_empty());
                    saw_unpublish_success = true;
                }
            }
            Ok(Some(ClientEvent::StreamBegin { stream_id })) => {
                // RtmpServer emits StreamBegin(1) right after the
                // publish handshake settles. It may arrive before or
                // after the publish onStatus, depending on the kernel's
                // packet arrival order — the test doesn't care which.
                assert_eq!(stream_id, 1);
                saw_stream_begin = true;
            }
            Ok(Some(ClientEvent::Other)) => continue,
            Ok(Some(ClientEvent::Result { .. })) | Ok(Some(ClientEvent::ErrorReply { .. })) => {
                // The client_handshake already consumed _result for
                // connect / createStream; a post-publish _result is
                // unexpected from our own server but harmless.
                continue;
            }
            Ok(Some(ClientEvent::StreamDry { .. }))
            | Ok(Some(ClientEvent::StreamIsRecorded { .. }))
            | Ok(Some(ClientEvent::PingResponse { .. })) => {
                // The session_close path doesn't emit these UCM events
                // — but a defensive `match` keeps the test forward-
                // compatible if the server's teardown adds one later.
                continue;
            }
            Ok(None) => {
                // Read half drained. We expect to have already seen
                // StreamEof before this point — server.close() emits it
                // first, then the FIN.
                break;
            }
            Err(e) => panic!("poll_event errored: {e}"),
        }
    }

    server_thread.join().expect("server thread");

    assert!(
        saw_stream_eof,
        "client must surface UserControl StreamEOF as ClientEvent::StreamEof"
    );
    assert!(
        saw_unpublish_success,
        "client must surface onStatus(NetStream.Unpublish.Success) as ClientEvent::OnStatus"
    );
    // StreamBegin is sent on most paths but isn't strictly required by
    // §7.1.7 — the server SHOULD emit it but a missing StreamBegin is
    // not a teardown protocol violation. Don't fail if we missed it,
    // just record what happened.
    let _ = saw_stream_begin;
}

#[test]
fn client_poll_event_returns_none_after_read_half_drains() {
    // After the read half has drained (TCP FIN observed via
    // ChunkReader returning UnexpectedEof / ConnectionReset),
    // subsequent poll_event calls must short-circuit to Ok(None)
    // without re-entering a blocking read on a dead socket. The
    // `read_eof` latch in RtmpClient is the system under test.
    //
    // Note: StreamEOF itself is NOT terminal — the server sends a
    // trailing onStatus(Unpublish.Success) after it, then half-closes.
    // The latch fires once we observe the FIN, not when we observe the
    // §7.1.7 event.
    let server = RtmpServer::bind("127.0.0.1:0").expect("bind");
    let addr = server.local_addr().expect("local_addr");

    let (started_tx, started_rx) = mpsc::channel::<()>();

    let server_thread = thread::spawn(move || {
        let req = server.accept().expect("server accept");
        let session = req.accept().expect("session accept");
        started_tx.send(()).unwrap();
        thread::sleep(Duration::from_millis(60));
        session.close().expect("session close");
    });

    thread::sleep(Duration::from_millis(50));
    let url = format!("rtmp://{}:{}/{APP}/{STREAM_KEY}", addr.ip(), addr.port());
    let mut client = RtmpClient::connect(&url).expect("client connect");
    started_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("server signal");
    client
        .inner_mut()
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("set_read_timeout");

    // Drain to None.
    let mut drained = false;
    for _ in 0..64 {
        match client.poll_event() {
            Ok(Some(_)) => continue,
            Ok(None) => {
                drained = true;
                break;
            }
            Err(e) => panic!("poll_event errored: {e}"),
        }
    }
    assert!(drained, "test precondition: poll_event must reach Ok(None)");

    // The next poll_event MUST return Ok(None) immediately — no more
    // socket read should happen.
    let t0 = std::time::Instant::now();
    let next = client.poll_event().expect("post-EOF poll");
    let elapsed = t0.elapsed();
    assert!(
        next.is_none(),
        "post-EOF poll_event must short-circuit to Ok(None), got {next:?}"
    );
    assert!(
        elapsed < Duration::from_millis(50),
        "post-EOF poll_event must short-circuit without blocking on the socket (took {elapsed:?})"
    );

    server_thread.join().expect("server thread");
}
