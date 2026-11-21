//! Session-level robustness on real loopback sockets, driven by a
//! hand-rolled peer built from the public low-level modules:
//!
//! * RTMP 1.0 §5.4.2 Abort Message — a partially sent message is
//!   discarded and the chunk stream stays in sync;
//! * §5.4.1 Set Chunk Size changing several times mid-stream;
//! * §7.1.7 PingRequest answered by the ingest server;
//! * the three ways a publish can end (protocol-level, orderly TCP
//!   close, truncated mid-message) reported through `end_reason`;
//! * the negotiation deadline: a peer that connects and stalls does
//!   not pin the accept loop;
//! * unknown and out-of-order commands before and after `publish`.

use std::io::Write;
use std::net::{SocketAddr, TcpStream};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use oxideav_rtmp::chunk::{ChunkReader, ChunkWriter, Message};
use oxideav_rtmp::handshake::client_handshake;
use oxideav_rtmp::message::{
    MSG_COMMAND_AMF0, MSG_SET_CHUNK_SIZE, MSG_USER_CONTROL, MSG_VIDEO, UserControlEvent,
    build_abort, build_connect, build_create_stream, build_publish, build_set_chunk_size,
    build_user_control_ping_request,
};
use oxideav_rtmp::{Amf0Value, RtmpClient, RtmpServer, StreamEnd, StreamPacket, amf};

const CSID_CONTROL: u32 = 2;
const CSID_COMMAND: u32 = 3;
const CSID_VIDEO: u32 = 5;

/// A hand-driven publisher: raw socket + chunk codec.
struct RawPublisher {
    tcp: TcpStream,
    reader: ChunkReader<TcpStream>,
    writer: ChunkWriter<TcpStream>,
}

impl RawPublisher {
    /// Handshake + `connect` only.
    fn connect(addr: SocketAddr) -> Self {
        let tcp = TcpStream::connect(addr).expect("connect");
        tcp.set_read_timeout(Some(Duration::from_secs(5)))
            .expect("timeout");
        let mut hs = tcp.try_clone().expect("clone");
        client_handshake(&mut hs).expect("handshake");
        let mut p = RawPublisher {
            reader: ChunkReader::new(tcp.try_clone().expect("clone")),
            writer: ChunkWriter::new(tcp.try_clone().expect("clone")),
            tcp,
        };
        p.writer
            .write_message(
                CSID_COMMAND,
                &build_connect(1.0, "live", "rtmp://127.0.0.1/live", "FMLE/3.0"),
            )
            .expect("connect cmd");
        p.wait_command("_result");
        p
    }

    /// `createStream` + `publish`, through `NetStream.Publish.Start`.
    fn publish(&mut self, key: &str) {
        self.writer
            .write_message(CSID_COMMAND, &build_create_stream(2.0))
            .expect("createStream");
        self.wait_command("_result");
        self.writer
            .write_message(CSID_COMMAND, &build_publish(3.0, 1, key, "live"))
            .expect("publish");
        self.wait_command("onStatus");
    }

    fn open(addr: SocketAddr, key: &str) -> Self {
        let mut p = Self::connect(addr);
        p.publish(key);
        p
    }

    /// Read until a command with the given name arrives, applying the
    /// server's Set Chunk Size on the way.
    fn wait_command(&mut self, name: &str) -> Vec<Amf0Value> {
        loop {
            let msg = self.reader.read_message().expect("server reply");
            match msg.msg_type_id {
                MSG_SET_CHUNK_SIZE => {
                    let n = u32::from_be_bytes(msg.payload[..4].try_into().expect("4 bytes"));
                    self.reader.set_chunk_size(n as usize);
                }
                MSG_COMMAND_AMF0 => {
                    let values = amf::decode_all(&msg.payload).expect("amf0");
                    if values.first().and_then(Amf0Value::as_str) == Some(name) {
                        return values;
                    }
                }
                _ => {}
            }
        }
    }

    fn command(&mut self, name: &str, tx: f64, args: &[Amf0Value]) {
        let payload = amf::encode_command(name, tx, Amf0Value::Null, args);
        self.writer
            .write_message(
                CSID_COMMAND,
                &Message {
                    msg_type_id: MSG_COMMAND_AMF0,
                    msg_stream_id: 1,
                    timestamp: 0,
                    payload,
                },
            )
            .expect("command");
    }

    fn video(&mut self, ts: u32, body_len: usize, fill: u8) {
        self.writer
            .write_message(
                CSID_VIDEO,
                &Message {
                    msg_type_id: MSG_VIDEO,
                    msg_stream_id: 1,
                    timestamp: ts,
                    payload: video_payload(body_len, fill),
                },
            )
            .expect("video");
    }
}

/// An AVC coded-frame video message body: FrameType/CodecID, AVC
/// packet type 1, zero composition time, then `body_len` filler bytes.
fn video_payload(body_len: usize, fill: u8) -> Vec<u8> {
    let mut v = vec![0x27, 0x01, 0x00, 0x00, 0x00];
    v.resize(5 + body_len, fill);
    v
}

/// Run an ingest server for one publisher; every video packet is
/// reported as `(timestamp, body)` and the end reason follows.
type Ingested = (Vec<(u32, Vec<u8>)>, Option<StreamEnd>);

fn ingest_one(server: RtmpServer) -> mpsc::Receiver<Ingested> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut session = server
            .accept()
            .expect("accept")
            .accept()
            .expect("accept publish");
        session
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("timeout");
        let mut video = Vec::new();
        while let Some(pkt) = session.next_packet().expect("next_packet") {
            if let StreamPacket::Video { timestamp, tag } = pkt {
                video.push((timestamp, tag.body));
            }
        }
        let _ = tx.send((video, session.end_reason()));
    });
    rx
}

fn bind() -> (RtmpServer, SocketAddr) {
    let server = RtmpServer::bind("127.0.0.1:0").expect("bind");
    let addr = server.local_addr().expect("addr");
    (server, addr)
}

#[test]
fn abort_message_discards_partial_and_keeps_chunk_stream_in_sync() {
    let (server, addr) = bind();
    let rx = ingest_one(server);
    let mut p = RawPublisher::open(addr, "abort-key");

    // Message A: 205 bytes at the default 128-byte chunk size → a
    // fmt-0 chunk + a fmt-3 continuation. Registers delta 1000.
    p.video(1000, 200, 0xA1);

    // Message B, abandoned: a fmt-3 *head* chunk (same header as A,
    // timestamp 2000) carrying only its first 128 bytes.
    let b = video_payload(200, 0xB2);
    let mut partial = vec![0xC0 | CSID_VIDEO as u8];
    partial.extend_from_slice(&b[..128]);
    p.tcp.write_all(&partial).expect("partial chunk");

    // §5.4.2: tell the peer to discard what it holds on that csid.
    p.writer
        .write_message(CSID_CONTROL, &build_abort(CSID_VIDEO))
        .expect("abort");

    // Message C: again fmt-3 head (timestamp 3000) + continuation.
    // Without the abort its first chunk would be spliced onto B.
    let c = video_payload(200, 0xC3);
    let mut wire = vec![0xC0 | CSID_VIDEO as u8];
    wire.extend_from_slice(&c[..128]);
    wire.push(0xC0 | CSID_VIDEO as u8);
    wire.extend_from_slice(&c[128..]);
    p.tcp.write_all(&wire).expect("message C");

    p.command("deleteStream", 0.0, &[Amf0Value::Number(1.0)]);
    let (video, end) = rx.recv_timeout(Duration::from_secs(10)).expect("ingest");
    assert_eq!(
        video,
        vec![(1000, vec![0xA1; 200]), (3000, vec![0xC3; 200])],
        "A and C arrive intact; the aborted B is dropped"
    );
    assert_eq!(end, Some(StreamEnd::Signalled));
}

#[test]
fn chunk_size_changes_mid_stream_are_followed() {
    let (server, addr) = bind();
    let rx = ingest_one(server);
    let mut p = RawPublisher::open(addr, "chunk-key");
    let mut expected = Vec::new();
    // Grow, shrink to the minimum, jump past the message size, return
    // to the default — a message of every size class at each setting.
    for (i, size) in [128usize, 4096, 1, 65_536, 300, 128]
        .into_iter()
        .enumerate()
    {
        p.writer
            .write_message(CSID_CONTROL, &build_set_chunk_size(size as u32))
            .expect("set chunk size");
        p.writer.set_chunk_size(size);
        for (j, len) in [0usize, 1, 127, 128, 129, 5000].into_iter().enumerate() {
            let ts = (i * 100 + j) as u32;
            let fill = (i * 16 + j) as u8;
            p.video(ts, len, fill);
            expected.push((ts, vec![fill; len]));
        }
    }
    p.command("deleteStream", 0.0, &[Amf0Value::Number(1.0)]);
    let (video, end) = rx.recv_timeout(Duration::from_secs(10)).expect("ingest");
    assert_eq!(video.len(), expected.len());
    assert_eq!(video, expected);
    assert_eq!(end, Some(StreamEnd::Signalled));
}

#[test]
fn ingest_server_answers_ping_request() {
    let (server, addr) = bind();
    let rx = ingest_one(server);
    let mut p = RawPublisher::open(addr, "ping-key");
    p.writer
        .write_message(CSID_CONTROL, &build_user_control_ping_request(0xDEAD_BEEF))
        .expect("ping");
    let echoed = loop {
        let msg = p.reader.read_message().expect("server message");
        if msg.msg_type_id == MSG_USER_CONTROL
            && let Ok(UserControlEvent::PingResponse { timestamp_ms }) =
                UserControlEvent::parse(&msg.payload)
        {
            break timestamp_ms;
        }
    };
    assert_eq!(echoed, 0xDEAD_BEEF, "§7.1.7: response echoes the timestamp");
    p.command("deleteStream", 0.0, &[Amf0Value::Number(1.0)]);
    rx.recv_timeout(Duration::from_secs(10)).expect("ingest");
}

#[test]
fn end_reason_orderly_close_without_goodbye() {
    let (server, addr) = bind();
    let rx = ingest_one(server);
    let mut p = RawPublisher::open(addr, "fin-key");
    p.video(0, 300, 0x11);
    p.video(40, 300, 0x22);
    drop(p); // TCP FIN between two messages, no deleteStream.
    let (video, end) = rx.recv_timeout(Duration::from_secs(10)).expect("ingest");
    assert_eq!(video.len(), 2);
    assert_eq!(end, Some(StreamEnd::PeerClosed));
    assert!(end.expect("ended").is_complete());
}

#[test]
fn end_reason_truncated_mid_message() {
    let (server, addr) = bind();
    let rx = ingest_one(server);
    let mut p = RawPublisher::open(addr, "cut-key");
    p.video(0, 300, 0x11);
    // Start a 1000-byte message and vanish after its first chunk.
    let mut head = vec![
        CSID_VIDEO as u8,
        0,
        0,
        40,
        0,
        0x03,
        0xE8,
        MSG_VIDEO,
        1,
        0,
        0,
        0,
    ];
    head.extend_from_slice(&video_payload(123, 0x33));
    p.tcp.write_all(&head).expect("first chunk");
    drop(p);
    let (video, end) = rx.recv_timeout(Duration::from_secs(10)).expect("ingest");
    assert_eq!(video.len(), 1, "only the complete message is delivered");
    assert_eq!(end, Some(StreamEnd::Truncated));
    assert!(!end.expect("ended").is_complete());
}

#[test]
fn stalled_connections_do_not_pin_the_accept_loop() {
    let (mut server, addr) = bind();
    server.set_negotiation_timeout(Some(Duration::from_millis(300)));
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let req = server.accept().expect("accept");
        let _ = tx.send(req.stream_name.clone());
        let mut s = req.accept().expect("accept publish");
        while let Ok(Some(_)) = s.next_packet() {}
    });
    // Two peers that never get to `publish`: one silent from the
    // start, one that stops half-way through its handshake.
    let silent = TcpStream::connect(addr).expect("connect");
    let mut half = TcpStream::connect(addr).expect("connect");
    half.write_all(&[3u8; 700]).expect("partial C0+C1");
    let started = Instant::now();
    // The accept loop serves connections in order, so the real
    // publisher only gets through once each stalled peer timed out.
    let mut real = RtmpClient::connect(&format!("rtmp://{addr}/live/after-stall")).expect("client");
    let name = rx.recv_timeout(Duration::from_secs(10)).expect("accepted");
    assert_eq!(name, "after-stall");
    assert!(
        started.elapsed() < Duration::from_secs(8),
        "stalled peers must be dropped at the negotiation deadline"
    );
    real.send_video(0, true, &[0, 0, 0, 1, 0x65])
        .expect("video");
    real.close().expect("close");
    drop(silent);
    drop(half);
}

#[test]
fn unknown_and_out_of_order_commands_are_tolerated() {
    let (server, addr) = bind();
    let rx = ingest_one(server);
    let mut p = RawPublisher::connect(addr);
    // Before createStream: an RPC nobody defined, a teardown for a
    // stream that does not exist, a bandwidth probe, a publish-side
    // advisory, and a NetStream command on stream 0.
    p.command("frobnicate", 7.0, &[Amf0Value::String("x".into())]);
    p.command("deleteStream", 0.0, &[Amf0Value::Number(9.0)]);
    p.command("_checkbw", 8.0, &[]);
    p.command("FCPublish", 9.0, &[Amf0Value::String("ooo-key".into())]);
    p.command(
        "pause",
        0.0,
        &[Amf0Value::Boolean(true), Amf0Value::Number(0.0)],
    );
    // Two createStream calls: both must be answered.
    p.writer
        .write_message(CSID_COMMAND, &build_create_stream(20.0))
        .expect("createStream");
    p.wait_command("_result");
    p.publish("ooo-key");
    // After publish: unknown commands interleaved with media.
    p.video(0, 10, 0x01);
    p.command("frobnicate", 0.0, &[]);
    p.command(
        "getStreamLength",
        31.0,
        &[Amf0Value::String("ooo-key".into())],
    );
    p.video(40, 10, 0x02);
    p.command("deleteStream", 0.0, &[Amf0Value::Number(1.0)]);
    let (video, end) = rx.recv_timeout(Duration::from_secs(10)).expect("ingest");
    assert_eq!(video, vec![(0, vec![0x01; 10]), (40, vec![0x02; 10])]);
    assert_eq!(end, Some(StreamEnd::Signalled));
}

#[test]
fn nested_aggregate_is_refused() {
    use oxideav_rtmp::{Error, build_aggregate};
    let (server, addr) = bind();
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut session = server
            .accept()
            .expect("accept")
            .accept()
            .expect("accept publish");
        session
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("timeout");
        let mut seen = 0usize;
        let outcome = loop {
            match session.next_packet() {
                Ok(Some(_)) => seen += 1,
                Ok(None) => break Ok(seen),
                Err(e) => break Err((seen, e)),
            }
        };
        let _ = tx.send(outcome);
    });
    let mut p = RawPublisher::open(addr, "nested-key");
    let frame = Message {
        msg_type_id: MSG_VIDEO,
        msg_stream_id: 1,
        timestamp: 40,
        payload: video_payload(8, 0x44),
    };
    // A plain aggregate is fine…
    let flat = build_aggregate(1, &[frame.clone(), frame.clone()]).expect("aggregate");
    p.writer.write_message(6, &flat).expect("flat aggregate");
    // …an aggregate carrying an aggregate is a protocol violation.
    let nested = build_aggregate(1, &[frame, flat]).expect("nested aggregate");
    p.writer
        .write_message(6, &nested)
        .expect("nested aggregate");
    match rx.recv_timeout(Duration::from_secs(10)).expect("ingest") {
        Err((seen, Error::ProtocolViolation(m))) => {
            assert_eq!(seen, 2, "the flat aggregate's two frames were delivered");
            assert!(m.contains("nested"), "{m}");
        }
        other => panic!("expected a protocol violation, got {other:?}"),
    }
}
