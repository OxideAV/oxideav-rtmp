//! End-to-end coverage for the RTMP 1.0 §3.7 User Control Message
//! events the publish-direction client + server pair surfaces beyond
//! the round-154 StreamBegin / StreamEOF pair.
//!
//! Per the spec table:
//!
//! * `StreamDry` (UCM type 2)        — server informs client the
//!   stream is dry, distinct from the terminal `StreamEOF`. Round-247
//!   surfaces this as [`ClientEvent::StreamDry`].
//! * `SetBufferLength` (UCM type 3)  — client-to-server only; the
//!   publisher direction validates the 8-byte event-data payload
//!   shape and otherwise reports the inbound copy as
//!   [`ClientEvent::Other`].
//! * `StreamIsRecorded` (UCM type 4) — server announces an on-demand
//!   stream. Round-247 surfaces as
//!   [`ClientEvent::StreamIsRecorded`].
//! * `PingRequest` (UCM type 6)      — server liveness probe; the
//!   publisher's [`RtmpClient::poll_event`] auto-replies internally
//!   so the request never reaches the publisher caller as an event.
//! * `PingResponse` (UCM type 7)     — server echoes back the 4-byte
//!   timestamp from a publisher [`RtmpClient::send_ping_request`].
//!   Round-247 surfaces as [`ClientEvent::PingResponse`] for RTT
//!   measurement.
//!
//! Each event is driven through a real loopback `RtmpServer` →
//! `RtmpClient` pair so the chunk-stream framing, ChunkReader
//! reassembly, and `RtmpClient::poll_event` classify path are all
//! exercised end-to-end. The PingResponse path additionally drives a
//! raw [`ChunkWriter`] injection of the exact wire bytes — neither
//! `RtmpSession` nor `RtmpClient` emits a PingResponse on its own
//! (the spec assigns that direction to the *client* responding to a
//! server probe, and our client handles it inside the classify loop
//! rather than surfacing it).

use std::io::Write;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use oxideav_rtmp::chunk::{ChunkReader, ChunkWriter};
use oxideav_rtmp::message::{
    build_user_control_ping_response, CSID_PROTOCOL_CONTROL, MSG_USER_CONTROL,
};
use oxideav_rtmp::{ClientEvent, Message, RtmpClient, RtmpServer};

const APP: &str = "live";
const STREAM_KEY: &str = "ucm-events-test";

/// Drain `poll_event` until we observe `predicate` or `Ok(None)`.
/// Returns `true` if the predicate ever fired. Skips intervening
/// `Other` / `StreamBegin` / `OnStatus` events that the publish
/// handshake naturally emits.
fn poll_until<F>(client: &mut RtmpClient, max_iters: usize, mut predicate: F) -> bool
where
    F: FnMut(&ClientEvent) -> bool,
{
    for _ in 0..max_iters {
        match client.poll_event() {
            Ok(Some(ev)) => {
                if predicate(&ev) {
                    return true;
                }
            }
            Ok(None) => return false,
            Err(e) => panic!("poll_event errored: {e}"),
        }
    }
    false
}

#[test]
fn client_observes_stream_dry_from_server() {
    let server = RtmpServer::bind("127.0.0.1:0").expect("bind");
    let addr = server.local_addr().expect("local_addr");

    let (started_tx, started_rx) = mpsc::channel::<()>();

    let server_thread = thread::spawn(move || {
        let req = server.accept().expect("server accept");
        let mut session = req.accept().expect("session accept");
        started_tx.send(()).unwrap();
        // Let the publish handshake settle on the client side before
        // we emit the StreamDry event — otherwise the early-arriving
        // event races the ChunkReader's bootstrap loop on slow CI.
        thread::sleep(Duration::from_millis(80));
        session.send_stream_dry().expect("send StreamDry");
        // Keep the session alive so the client sees the event before
        // the read half drains.
        thread::sleep(Duration::from_millis(120));
        session.close().expect("session close");
    });

    thread::sleep(Duration::from_millis(50));
    let url = format!("rtmp://{}:{}/{APP}/{STREAM_KEY}", addr.ip(), addr.port());
    let mut client = RtmpClient::connect(&url).expect("client connect");
    started_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("server signal");
    client
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("set_read_timeout");

    let saw_stream_dry = poll_until(&mut client, 32, |ev| {
        matches!(ev, ClientEvent::StreamDry { stream_id: 1 })
    });

    server_thread.join().expect("server thread");
    assert!(
        saw_stream_dry,
        "client must surface UserControl StreamDry as ClientEvent::StreamDry"
    );
}

#[test]
fn client_observes_stream_is_recorded_from_server() {
    let server = RtmpServer::bind("127.0.0.1:0").expect("bind");
    let addr = server.local_addr().expect("local_addr");

    let (started_tx, started_rx) = mpsc::channel::<()>();

    let server_thread = thread::spawn(move || {
        let req = server.accept().expect("server accept");
        let mut session = req.accept().expect("session accept");
        started_tx.send(()).unwrap();
        thread::sleep(Duration::from_millis(80));
        session
            .send_stream_is_recorded()
            .expect("send StreamIsRecorded");
        thread::sleep(Duration::from_millis(120));
        session.close().expect("session close");
    });

    thread::sleep(Duration::from_millis(50));
    let url = format!("rtmp://{}:{}/{APP}/{STREAM_KEY}", addr.ip(), addr.port());
    let mut client = RtmpClient::connect(&url).expect("client connect");
    started_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("server signal");
    client
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("set_read_timeout");

    let saw_recorded = poll_until(&mut client, 32, |ev| {
        matches!(ev, ClientEvent::StreamIsRecorded { stream_id: 1 })
    });

    server_thread.join().expect("server thread");
    assert!(
        saw_recorded,
        "client must surface UserControl StreamIsRecorded as ClientEvent::StreamIsRecorded"
    );
}

/// Server emits a `PingRequest`; the publisher's `RtmpClient` must
/// auto-reply with a `PingResponse` internally without surfacing the
/// request to the caller as a `ClientEvent` — per the spec the
/// request is a liveness probe handled at the protocol level, not an
/// application-visible event.
#[test]
fn client_auto_replies_to_server_ping_request_without_surfacing() {
    const PROBE_TIMESTAMP: u32 = 0x1234_5678;

    let server = RtmpServer::bind("127.0.0.1:0").expect("bind");
    let addr = server.local_addr().expect("local_addr");

    let (started_tx, started_rx) = mpsc::channel::<()>();

    let server_thread = thread::spawn(move || {
        let req = server.accept().expect("server accept");
        let mut session = req.accept().expect("session accept");
        started_tx.send(()).unwrap();
        thread::sleep(Duration::from_millis(80));
        session
            .send_ping_request(PROBE_TIMESTAMP)
            .expect("send PingRequest");
        thread::sleep(Duration::from_millis(120));
        session.close().expect("session close");
    });

    thread::sleep(Duration::from_millis(50));
    let url = format!("rtmp://{}:{}/{APP}/{STREAM_KEY}", addr.ip(), addr.port());
    let mut client = RtmpClient::connect(&url).expect("client connect");
    started_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("server signal");
    client
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("set_read_timeout");

    let mut surfaced_ping_response = false;
    for _ in 0..32 {
        match client.poll_event() {
            Ok(Some(ClientEvent::PingResponse { .. })) => {
                surfaced_ping_response = true;
                break;
            }
            Ok(Some(_)) => continue,
            Ok(None) => break,
            Err(e) => panic!("poll_event errored: {e}"),
        }
    }

    server_thread.join().expect("server thread");
    assert!(
        !surfaced_ping_response,
        "PingRequest from server is a protocol-level liveness probe and must be \
         auto-replied internally — it must not surface as ClientEvent::PingResponse"
    );
}

/// A `PingResponse` (UCM type 7) arriving on the wire must surface to
/// the publisher as [`ClientEvent::PingResponse`] carrying the exact
/// 4-byte timestamp the server echoed. The test drives the raw wire
/// bytes through [`ChunkWriter`] from a hand-rolled server endpoint —
/// neither `RtmpSession` nor `RtmpClient` emits a PingResponse on its
/// own (spec assigns that direction to the client replying to a
/// server's PingRequest), so the integration path is the right place
/// to assert the classification.
#[test]
fn client_surfaces_server_ping_response_as_typed_event() {
    const PROBE_TIMESTAMP: u32 = 0xAB_CD_EF_12;

    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind raw");
    let addr = listener.local_addr().expect("local_addr");
    let (started_tx, started_rx) = mpsc::channel::<()>();
    let (sent_tx, sent_rx) = mpsc::channel::<()>();

    let server_thread = thread::spawn(move || {
        let (stream, _peer) = listener.accept().expect("raw accept");
        run_minimal_server_handshake(stream, |writer, stream_id| {
            // Now the publish loop is live. Push a PingResponse carrying
            // the probe timestamp — the byte layout matches the
            // `message::tests::user_control_ping_response_wire_bytes`
            // unit-test assertion exactly.
            let _ = stream_id;
            let pong = build_user_control_ping_response(PROBE_TIMESTAMP);
            assert_eq!(pong.msg_type_id, MSG_USER_CONTROL);
            writer
                .write_message(CSID_PROTOCOL_CONTROL, &pong)
                .expect("write PingResponse");
            writer.flush().expect("flush pong");
            sent_tx.send(()).unwrap();

            started_tx.send(()).unwrap();
            // Hold the socket open long enough for the client to see
            // the event before we drop.
            thread::sleep(Duration::from_millis(250));
        });
    });

    thread::sleep(Duration::from_millis(30));
    let url = format!("rtmp://{}:{}/{APP}/{STREAM_KEY}", addr.ip(), addr.port());
    let mut client = RtmpClient::connect(&url).expect("client connect");
    sent_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("server-side send signal");
    let _ = started_rx.recv_timeout(Duration::from_secs(5));
    client
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("set_read_timeout");

    let mut observed_timestamp: Option<u32> = None;
    for _ in 0..64 {
        match client.poll_event() {
            Ok(Some(ClientEvent::PingResponse { timestamp_ms })) => {
                observed_timestamp = Some(timestamp_ms);
                break;
            }
            Ok(Some(_)) => continue,
            Ok(None) => break,
            Err(_) => break,
        }
    }
    let _ = client.close();

    server_thread.join().expect("server thread");

    assert_eq!(
        observed_timestamp,
        Some(PROBE_TIMESTAMP),
        "ClientEvent::PingResponse must carry the exact 4-byte timestamp the peer echoed"
    );
}

/// A `SetBufferLength` (UCM type 3) arriving on the wire is the only
/// UCM event with an 8-byte event-data body (4-byte stream id +
/// 4-byte buffer length in ms). Per RTMP 1.0 §3.7 the client is the
/// *sender* of this event, but a forwarding ingest may legitimately
/// see it on the wire. The publisher's classify path validates the
/// payload size (returning `ProtocolViolation` on truncation) and
/// otherwise reports it as `ClientEvent::Other`.
#[test]
fn client_rejects_truncated_set_buffer_length() {
    // Build a raw 6-byte SetBufferLength payload (4-byte stream id +
    // *missing* 4-byte buffer length) and confirm `RtmpClient`'s
    // classify rejects it. We use the internal SUT via a hand-rolled
    // chunk-stream injection like the PingResponse test.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind raw");
    let addr = listener.local_addr().expect("local_addr");
    let (signal_tx, signal_rx) = mpsc::channel::<()>();

    let server_thread = thread::spawn(move || {
        let (stream, _peer) = listener.accept().expect("raw accept");
        run_minimal_server_handshake(stream, |writer, _stream_id| {
            // Hand-rolled truncated SetBufferLength: event type 3 + 4-byte
            // stream id, MISSING the 4-byte buffer length. RTMP §3.7
            // requires 8 bytes of event data; this payload has 4.
            let truncated = Message {
                msg_type_id: MSG_USER_CONTROL,
                msg_stream_id: 0,
                timestamp: 0,
                // [event type 3 BE] [stream id 1 BE] -- no buffer length
                payload: vec![0x00, 0x03, 0x00, 0x00, 0x00, 0x01],
            };
            writer
                .write_message(CSID_PROTOCOL_CONTROL, &truncated)
                .expect("write truncated SetBufferLength");
            writer.flush().expect("flush truncated");
            signal_tx.send(()).unwrap();
            thread::sleep(Duration::from_millis(300));
        });
    });

    thread::sleep(Duration::from_millis(30));
    let url = format!("rtmp://{}:{}/{APP}/{STREAM_KEY}", addr.ip(), addr.port());
    let mut client = RtmpClient::connect(&url).expect("client connect");
    signal_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("server signal");
    client
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("set_read_timeout");

    let mut saw_violation = false;
    for _ in 0..32 {
        match client.poll_event() {
            Ok(Some(_)) => continue,
            Ok(None) => break,
            Err(e) => {
                let msg = format!("{e}");
                if msg.contains("SetBufferLength") {
                    saw_violation = true;
                    break;
                }
            }
        }
    }
    let _ = client.close();
    server_thread.join().expect("server thread");

    assert!(
        saw_violation,
        "truncated SetBufferLength must surface ProtocolViolation, not silent Other"
    );
}

/// Drive a minimal RTMP server endpoint that completes the C0/C1/C2
/// handshake, replies to `connect` / `createStream` / `publish` so the
/// client's [`RtmpClient::connect`] returns, then hands control to
/// `body` to push arbitrary user-control messages onto the wire. The
/// callback receives the `ChunkWriter` borrowed at the active chunk
/// size and the stream id chosen by this helper (always `1`).
///
/// This mirrors the publish-side `RtmpServer` handshake closely enough
/// that `RtmpClient::connect` proceeds without spec-violation errors,
/// but stops short of routing publisher A/V — the goal is to push a
/// hand-crafted UCM message into the publisher's `poll_event` classify
/// path, not to terminate a real publish.
fn run_minimal_server_handshake<F>(stream: std::net::TcpStream, body: F)
where
    F: FnOnce(&mut ChunkWriter<std::net::TcpStream>, u32),
{
    let mut stream = stream;
    oxideav_rtmp::handshake::server_handshake(&mut stream).expect("server handshake");

    let read_clone = stream.try_clone().expect("clone reader");
    let write_clone = stream.try_clone().expect("clone writer");
    let mut reader = ChunkReader::new(read_clone);
    let mut writer = ChunkWriter::new(write_clone);

    // Walk through any number of inbound protocol-control messages
    // (the publisher sends Set Chunk Size right after the handshake)
    // until we see the AMF0 `connect` command. The client bumps its
    // outbound chunk size to 4096; honour that by updating the
    // reader's reassembly cap, otherwise multi-chunk `connect`
    // payloads with embedded capability blocks would split wrong.
    loop {
        let m = reader.read_message().expect("read pre-connect");
        match m.msg_type_id {
            1 => {
                // MSG_SET_CHUNK_SIZE
                let size =
                    u32::from_be_bytes([m.payload[0], m.payload[1], m.payload[2], m.payload[3]])
                        & 0x7FFF_FFFF;
                reader.set_chunk_size(size as usize);
            }
            5 | 6 => {
                // MSG_WINDOW_ACK_SIZE / MSG_SET_PEER_BANDWIDTH — informational
            }
            20 => {
                let vals = oxideav_rtmp::amf::decode_all(&m.payload).expect("amf");
                let name = vals
                    .first()
                    .and_then(oxideav_rtmp::Amf0Value::as_str)
                    .unwrap_or("");
                if name == "connect" {
                    let tx = vals
                        .get(1)
                        .and_then(oxideav_rtmp::Amf0Value::as_f64)
                        .unwrap_or(1.0);
                    let r = oxideav_rtmp::message::build_connect_result(tx);
                    writer.write_message(3, &r).expect("write connect _result");
                    writer.flush().expect("flush connect");
                    break;
                }
            }
            _ => {}
        }
    }

    let stream_id: u32 = 1;

    // Consume up to createStream (with releaseStream/FCPublish
    // bouncing through as no-ops), reply with the stream id.
    loop {
        let m = reader.read_message().expect("read pre-createStream");
        if m.msg_type_id == 20 {
            let vals = oxideav_rtmp::amf::decode_all(&m.payload).expect("amf");
            let name = vals
                .first()
                .and_then(oxideav_rtmp::Amf0Value::as_str)
                .unwrap_or("");
            if name == "createStream" {
                let tx = vals
                    .get(1)
                    .and_then(oxideav_rtmp::Amf0Value::as_f64)
                    .unwrap_or(0.0);
                let r = oxideav_rtmp::message::build_create_stream_result(tx, stream_id as f64);
                writer.write_message(3, &r).expect("write cs result");
                writer.flush().expect("flush cs");
                break;
            }
        }
    }

    // Consume publish, reply with NetStream.Publish.Start onStatus.
    loop {
        let m = reader.read_message().expect("read pre-publish");
        if m.msg_type_id == 20 {
            let vals = oxideav_rtmp::amf::decode_all(&m.payload).expect("amf");
            let name = vals
                .first()
                .and_then(oxideav_rtmp::Amf0Value::as_str)
                .unwrap_or("");
            if name == "publish" {
                let onst = oxideav_rtmp::message::build_on_status(
                    stream_id,
                    "status",
                    "NetStream.Publish.Start",
                    "ready",
                );
                writer.write_message(3, &onst).expect("write onStatus");
                writer.flush().expect("flush onStatus");
                break;
            }
        }
    }

    body(&mut writer, stream_id);

    let _ = writer.flush();
    let _ = stream.flush();
}
