//! Adversarial-input coverage for the play-direction surfaces:
//! [`RtmpPlayer::connect`] against hostile / broken servers, and
//! [`PlaySession::next_event`] against a subscriber that turns to
//! garbage after the play handshake. Every case must produce a clean
//! `Err` / end-of-stream — never a panic, hang, or runaway
//! allocation. Complements `injection_robustness.rs`, which fuzzes
//! the underlying parsers directly.

use std::io::Write as IoWrite;
use std::net::{TcpListener, TcpStream};
use std::thread;
use std::time::Duration;

use oxideav_rtmp::chunk::{ChunkReader, ChunkWriter};
use oxideav_rtmp::handshake::{client_handshake, server_handshake};
use oxideav_rtmp::message::{build_connect, build_create_stream, NetStreamCommand};
use oxideav_rtmp::{amf, Amf0Value, RtmpPlayer, RtmpServer, SessionRequest};

/// Deterministic xorshift32 byte stream — same generator family the
/// parser fuzz suite uses, so failures reproduce byte-for-byte.
fn xorshift_bytes(seed: u32, len: usize) -> Vec<u8> {
    let mut state = seed.max(1);
    let mut out = Vec::with_capacity(len);
    while out.len() < len {
        state ^= state << 13;
        state ^= state >> 17;
        state ^= state << 5;
        out.extend_from_slice(&state.to_le_bytes());
    }
    out.truncate(len);
    out
}

/// A "server" that completes the RTMP handshake, then answers every
/// subsequent expectation with deterministic garbage and closes.
/// `RtmpPlayer::connect` must fail cleanly (chunk / AMF / command
/// parse error, or EOF surfaced as an error before Play.Start).
#[test]
fn player_connect_survives_garbage_after_handshake() {
    for seed in [1u32, 7, 42, 0xDEAD, 0xBEEF, 12345, 777, 31337] {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let garbage = xorshift_bytes(seed, 4096);
        let peer = thread::spawn(move || {
            let (mut sock, _) = listener.accept().expect("accept");
            if server_handshake(&mut sock).is_err() {
                return;
            }
            let _ = sock.write_all(&garbage);
            // Drop → FIN, so a reader waiting for more chunk bytes
            // observes EOF instead of blocking forever.
        });
        let url = format!("rtmp://{addr}/live/key");
        match RtmpPlayer::connect(&url) {
            Err(_) => {}
            Ok(_) => panic!("seed {seed}: connect must not succeed against garbage"),
        }
        peer.join().expect("peer thread");
    }
}

/// A server that speaks a *valid* connect reply and then dies before
/// the play status: the player's setup driver surfaces a clean error
/// (EOF mid-setup), not a hang.
#[test]
fn player_connect_survives_server_death_before_play_start() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    let peer = thread::spawn(move || {
        let (mut sock, _) = listener.accept().expect("accept");
        if server_handshake(&mut sock).is_err() {
            return;
        }
        let mut reader = ChunkReader::new(sock.try_clone().expect("clone r"));
        let mut writer = ChunkWriter::new(sock.try_clone().expect("clone w"));
        // Read messages until `connect` arrives, honouring the
        // client's SetChunkSize.
        while let Ok(msg) = reader.read_message() {
            match msg.msg_type_id {
                1 if msg.payload.len() >= 4 => {
                    let size = u32::from_be_bytes([
                        msg.payload[0],
                        msg.payload[1],
                        msg.payload[2],
                        msg.payload[3],
                    ]) & 0x7FFF_FFFF;
                    reader.set_chunk_size(size as usize);
                }
                20 => {
                    let values = amf::decode_all(&msg.payload).unwrap_or_default();
                    if values.first().and_then(Amf0Value::as_str) == Some("connect") {
                        let tx = values.get(1).and_then(Amf0Value::as_f64).unwrap_or(1.0);
                        let reply = oxideav_rtmp::message::build_connect_result(tx);
                        let _ = writer.write_message(3, &reply);
                        let _ = writer.flush();
                        // Die before createStream ever gets a result.
                        return;
                    }
                }
                _ => {}
            }
        }
    });
    let url = format!("rtmp://{addr}/live/key");
    match RtmpPlayer::connect(&url) {
        Err(_) => {}
        Ok(_) => panic!("connect must not succeed when the server dies mid-setup"),
    }
    peer.join().expect("peer thread");
}

/// A subscriber that negotiates a legitimate play session and then
/// floods the command channel with garbage bytes: the server-side
/// [`PlaySession::next_event`] pump must surface a clean `Err` or a
/// clean end — never a panic.
#[test]
fn play_session_next_event_survives_garbage_subscriber() {
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
        // Pump until error or clean end; both are acceptable, panics
        // and hangs are not.
        while let Ok(Some(_)) = session.next_event() {}
    });

    // Legitimate handshake + connect + createStream + play from a raw
    // subscriber…
    let tcp = TcpStream::connect(addr).expect("connect");
    tcp.set_read_timeout(Some(Duration::from_secs(5)))
        .expect("timeout");
    let mut hs = tcp.try_clone().expect("clone hs");
    client_handshake(&mut hs).expect("client handshake");
    let mut writer = ChunkWriter::new(tcp.try_clone().expect("clone w"));
    let mut reader = ChunkReader::new(tcp.try_clone().expect("clone r"));
    let tc_url = format!("rtmp://{addr}/live");
    writer
        .write_message(3, &build_connect(1.0, "live", &tc_url, "test"))
        .expect("connect");
    writer.flush().expect("flush");
    wait_command(&mut reader, "_result");
    writer
        .write_message(3, &build_create_stream(2.0))
        .expect("createStream");
    writer.flush().expect("flush");
    wait_command(&mut reader, "_result");
    let play = NetStreamCommand::Play {
        stream_name: "key".into(),
        start: None,
        duration: None,
        reset: None,
    };
    writer.write_message(3, &play.to_message(1)).expect("play");
    writer.flush().expect("flush");

    // …then raw garbage straight onto the socket, then FIN.
    let mut raw = tcp.try_clone().expect("clone raw");
    raw.write_all(&xorshift_bytes(0xC0FFEE, 8192))
        .expect("garbage");
    let _ = tcp.shutdown(std::net::Shutdown::Write);

    server_thread.join().expect("server must not panic");
}

fn wait_command(reader: &mut ChunkReader<TcpStream>, want: &str) {
    for _ in 0..50 {
        let msg = reader.read_message().expect("read");
        if msg.msg_type_id == 1 && msg.payload.len() >= 4 {
            let size = u32::from_be_bytes([
                msg.payload[0],
                msg.payload[1],
                msg.payload[2],
                msg.payload[3],
            ]) & 0x7FFF_FFFF;
            reader.set_chunk_size(size as usize);
            continue;
        }
        if msg.msg_type_id == 20 {
            let values = amf::decode_all(&msg.payload).unwrap_or_default();
            if values.first().and_then(Amf0Value::as_str) == Some(want) {
                return;
            }
        }
    }
    panic!("never saw `{want}`");
}
