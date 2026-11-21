#![no_main]

//! Whole-session state machines over a real loopback socket
//! (127.0.0.1, OS-assigned port).
//!
//! The first input byte picks the role the fuzz input plays:
//!
//! * a client that sends arbitrary bytes right after a valid simple
//!   handshake (negotiation loop);
//! * a publisher: valid `connect` / `createStream` / `publish`, then
//!   an arbitrary chunk stream into the ingest pump;
//! * a subscriber: valid `connect` / `createStream` / `play`, then an
//!   arbitrary chunk stream into the play-session event pump;
//! * a server answering this crate's publish client: valid setup
//!   replies, then an arbitrary chunk stream into `poll_event`;
//! * a server answering this crate's play client likewise.
//!
//! Contract: no panic, no hang (every socket carries a timeout), and a
//! pump that ends with `Ok(None)` always states why.

use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::OnceLock;
use std::thread;
use std::time::{Duration, Instant};

use libfuzzer_sys::fuzz_target;
use oxideav_rtmp::chunk::{ChunkWriter, Message};
use oxideav_rtmp::message::{
    STATUS_PLAY_START, STATUS_PUBLISH_START, build_connect, build_connect_result,
    build_create_stream, build_create_stream_result, build_on_status, build_publish,
};
use oxideav_rtmp::{NetStreamCommand, RtmpClient, RtmpPlayer, RtmpServer, SessionRequest, amf};

const IO_TIMEOUT: Duration = Duration::from_millis(500);
/// Keeps every write far below the loopback socket buffers, so the
/// single-threaded roles can never block on a full pipe.
const MAX_BODY: usize = 16 * 1024;
const MAX_STEPS: usize = 4096;

struct Env {
    listener: TcpListener,
    server: RtmpServer,
}

fn env() -> &'static Env {
    static ENV: OnceLock<Env> = OnceLock::new();
    ENV.get_or_init(|| {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        // Only the negotiation driver of this server is used; the
        // connections come from `listener`.
        let mut server = RtmpServer::bind("127.0.0.1:0").expect("bind");
        server.set_negotiation_timeout(Some(IO_TIMEOUT));
        Env { listener, server }
    })
}

fn chunked(msgs: &[(u32, Message)]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut w = ChunkWriter::new(&mut out);
    for (csid, m) in msgs {
        w.write_message(*csid, m).expect("setup message");
    }
    out
}

/// The fuzz input acts as a client of this crate's server.
fn as_client(prefix: &[u8], body: &[u8]) {
    let env = env();
    let Ok(addr) = env.listener.local_addr() else {
        return;
    };
    let Ok(mut client) = TcpStream::connect(addr) else {
        return;
    };
    let _ = client.set_write_timeout(Some(IO_TIMEOUT));
    // Simple handshake: C0, a zero-version C1, any C2.
    let mut wire = vec![3u8];
    wire.resize(1 + 2 * 1536, 0);
    wire.extend_from_slice(prefix);
    wire.extend_from_slice(body);
    if client.write_all(&wire).is_err() {
        return;
    }
    let _ = client.shutdown(Shutdown::Write);
    let Ok((stream, _)) = env.listener.accept() else {
        return;
    };
    let _ = stream.set_nonblocking(false);
    match env.server.negotiate(stream) {
        Ok(SessionRequest::Publish(req)) => {
            let Ok(mut session) = req.accept() else {
                return;
            };
            let _ = session.set_read_timeout(Some(IO_TIMEOUT));
            for _ in 0..MAX_STEPS {
                match session.next_packet() {
                    Ok(Some(_)) => {}
                    Ok(None) => {
                        assert!(session.end_reason().is_some());
                        break;
                    }
                    Err(_) => break,
                }
            }
            let _ = session.data_frames();
        }
        Ok(SessionRequest::Play(req)) => {
            let Ok(mut session) = req.accept() else {
                return;
            };
            let _ = session.set_read_timeout(Some(IO_TIMEOUT));
            for _ in 0..MAX_STEPS {
                match session.next_event() {
                    Ok(Some(_)) => {}
                    Ok(None) => {
                        assert!(session.end_reason().is_some());
                        break;
                    }
                    Err(_) => break,
                }
            }
        }
        Err(_) => {}
    }
}

/// The fuzz input acts as a server; `run` drives one of this crate's
/// clients against it.
fn as_server(prefix: Vec<u8>, body: &[u8], run: impl FnOnce(&str)) {
    // Thousands of short-lived loopback connections a second can
    // leave the machine briefly out of ephemeral ports; that is the
    // harness's problem, not a finding — skip the iteration.
    let Ok(listener) = TcpListener::bind("127.0.0.1:0") else {
        return;
    };
    let Ok(addr) = listener.local_addr() else {
        return;
    };
    let mut script = vec![3u8];
    script.resize(1 + 2 * 1536, 0);
    script.extend_from_slice(&prefix);
    script.extend_from_slice(body);
    let peer = thread::spawn(move || {
        // The client may never arrive (its connect can fail when the
        // loopback runs short of ephemeral ports), so the accept is
        // bounded rather than blocking.
        let _ = listener.set_nonblocking(true);
        let deadline = Instant::now() + IO_TIMEOUT;
        let mut sock = loop {
            match listener.accept() {
                Ok((sock, _)) => break sock,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    if Instant::now() >= deadline {
                        return;
                    }
                    thread::sleep(Duration::from_micros(200));
                }
                Err(_) => return,
            }
        };
        let _ = sock.set_nonblocking(false);
        let _ = sock.set_write_timeout(Some(IO_TIMEOUT));
        let _ = sock.set_read_timeout(Some(IO_TIMEOUT));
        let _ = sock.write_all(&script);
        let _ = sock.shutdown(Shutdown::Write);
        // Drain until the client goes away (or goes quiet).
        let mut sink = [0u8; 4096];
        while matches!(sock.read(&mut sink), Ok(n) if n > 0) {}
    });
    run(&format!("rtmp://{addr}/live/fuzz"));
    let _ = peer.join();
}

fuzz_target!(|data: &[u8]| {
    let Some((&role, body)) = data.split_first() else {
        return;
    };
    let body = &body[..body.len().min(MAX_BODY)];
    let connect = (
        3u32,
        build_connect(1.0, "live", "rtmp://127.0.0.1/live", "FMLE/3.0"),
    );
    let create = (3u32, build_create_stream(2.0));
    match role % 5 {
        0 => as_client(&[], body),
        1 => {
            let publish = (3u32, build_publish(3.0, 1, "fuzz", "live"));
            as_client(&chunked(&[connect, create, publish]), body);
        }
        2 => {
            let play = NetStreamCommand::Play {
                stream_name: "fuzz".into(),
                start: None,
                duration: None,
                reset: None,
            };
            as_client(&chunked(&[connect, create, (3, play.to_message(1))]), body);
        }
        3 => {
            // Publish client: connect is transaction 1, createStream 4.
            let prefix = chunked(&[
                (3, build_connect_result(1.0)),
                (3, command_result(2.0)),
                (3, command_result(3.0)),
                (3, build_create_stream_result(4.0, 1.0)),
                (3, build_on_status(1, "status", STATUS_PUBLISH_START, "ok")),
            ]);
            as_server(prefix, body, |url| {
                let Ok(mut client) = RtmpClient::connect(url) else {
                    return;
                };
                let _ = client.set_read_timeout(Some(IO_TIMEOUT));
                for _ in 0..MAX_STEPS {
                    match client.poll_event() {
                        Ok(Some(_)) => {}
                        Ok(None) => {
                            assert!(client.end_reason().is_some());
                            break;
                        }
                        Err(_) => break,
                    }
                }
            });
        }
        _ => {
            // Play client: connect is transaction 1, createStream 2.
            let prefix = chunked(&[
                (3, build_connect_result(1.0)),
                (3, build_create_stream_result(2.0, 1.0)),
                (3, build_on_status(1, "status", STATUS_PLAY_START, "ok")),
            ]);
            as_server(prefix, body, |url| {
                let Ok(mut player) = RtmpPlayer::connect(url) else {
                    return;
                };
                let _ = player.set_read_timeout(Some(IO_TIMEOUT));
                for _ in 0..MAX_STEPS {
                    match player.next_packet() {
                        Ok(Some(_)) => {}
                        Ok(None) => {
                            assert!(player.end_reason().is_some());
                            break;
                        }
                        Err(_) => break,
                    }
                }
            });
        }
    }
});

/// A bare `_result` for the advisory commands a publisher issues
/// between `connect` and `createStream`.
fn command_result(tx: f64) -> Message {
    Message {
        msg_type_id: 20,
        msg_stream_id: 0,
        timestamp: 0,
        payload: amf::encode_command("_result", tx, amf::Amf0Value::Null, &[]),
    }
}
