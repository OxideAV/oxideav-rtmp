//! End-to-end coverage for the RTMP 1.0 Commands-Messages §4.2
//! NetStream control commands a server receives on a stream —
//! `pause`, `seek`, `receiveAudio`, `receiveVideo`, `play`, `play2`.
//!
//! These are the subscriber-side (play) and shared control commands a
//! peer issues after `createStream`. The publish-direction
//! [`RtmpServer`] surfaces a recognised one as
//! [`StreamPacket::Command`] carrying a typed [`NetStreamCommand`], so
//! a server application can react (e.g. honour `receiveAudio false` by
//! suspending audio forwarding). Teardown commands (`closeStream` /
//! `deleteStream` / `FCUnpublish`) are still consumed silently and end
//! the session.
//!
//! The test drives a real [`RtmpServer`] and a hand-rolled minimal
//! client that completes the C0/C1/C2 handshake, `connect`,
//! `createStream`, and `publish`, then writes the NetStream command
//! onto the wire via a raw [`ChunkWriter`]. This exercises the
//! chunk-stream framing + the server's `handle_message` command
//! dispatch end-to-end, not just the unit-level
//! [`NetStreamCommand::parse`].

use std::io::Write;
use std::net::TcpStream;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use oxideav_rtmp::chunk::{ChunkReader, ChunkWriter};
use oxideav_rtmp::message::{build_connect, build_create_stream, build_publish, CSID_COMMAND};
use oxideav_rtmp::{Message, NetStreamCommand, RtmpServer, StreamPacket};

const APP: &str = "live";
const STREAM_KEY: &str = "netstream-cmd-test";

/// Hand-rolled minimal publisher: handshake → connect → createStream →
/// publish, mirroring what `RtmpServer::accept` expects, then invoke
/// `body` with the writer + negotiated stream id so the caller can push
/// arbitrary NetStream commands onto the wire.
fn run_minimal_client<F>(stream: TcpStream, body: F)
where
    F: FnOnce(&mut ChunkWriter<TcpStream>, u32),
{
    let mut stream = stream;
    oxideav_rtmp::handshake::client_handshake(&mut stream).expect("client handshake");

    let read_clone = stream.try_clone().expect("clone reader");
    let write_clone = stream.try_clone().expect("clone writer");
    let mut reader = ChunkReader::new(read_clone);
    let mut writer = ChunkWriter::new(write_clone);

    let tc_url = format!("rtmp://127.0.0.1/{APP}");
    // connect — the server reads `app` + `tcUrl` from the command object.
    writer
        .write_message(CSID_COMMAND, &build_connect(1.0, APP, &tc_url, "test"))
        .expect("write connect");
    writer.flush().expect("flush connect");

    // Drain server replies until we see the connect `_result`, honouring
    // the server's Set Chunk Size so subsequent reads reassemble right.
    loop {
        let m = reader.read_message().expect("read post-connect");
        if m.msg_type_id == 1 {
            let size = u32::from_be_bytes([m.payload[0], m.payload[1], m.payload[2], m.payload[3]])
                & 0x7FFF_FFFF;
            reader.set_chunk_size(size as usize);
        }
        if m.msg_type_id == 20 {
            let vals = oxideav_rtmp::amf::decode_all(&m.payload).expect("amf");
            if vals.first().and_then(oxideav_rtmp::Amf0Value::as_str) == Some("_result") {
                break;
            }
        }
    }

    // createStream — wait for the `_result` carrying the stream id.
    writer
        .write_message(CSID_COMMAND, &build_create_stream(2.0))
        .expect("write createStream");
    writer.flush().expect("flush createStream");
    let stream_id;
    loop {
        let m = reader.read_message().expect("read post-createStream");
        if m.msg_type_id == 20 {
            let vals = oxideav_rtmp::amf::decode_all(&m.payload).expect("amf");
            if vals.first().and_then(oxideav_rtmp::Amf0Value::as_str) == Some("_result") {
                stream_id = vals
                    .get(3)
                    .and_then(oxideav_rtmp::Amf0Value::as_f64)
                    .unwrap_or(1.0) as u32;
                break;
            }
        }
    }

    // publish — the server transitions into the session after this.
    writer
        .write_message(
            CSID_COMMAND,
            &build_publish(3.0, stream_id, STREAM_KEY, "live"),
        )
        .expect("write publish");
    writer.flush().expect("flush publish");

    body(&mut writer, stream_id);

    let _ = writer.flush();
    let _ = stream.flush();
}

/// Drive a NetStream command from a hand-rolled client to a real
/// `RtmpServer` session and assert the server surfaces it verbatim.
fn assert_command_surfaced(cmd: NetStreamCommand) {
    let server = RtmpServer::bind("127.0.0.1:0").expect("bind");
    let addr = server.local_addr().expect("local_addr");
    let (got_tx, got_rx) = mpsc::channel::<Option<NetStreamCommand>>();

    let server_thread = thread::spawn(move || {
        let req = server.accept().expect("server accept");
        let mut session = req.accept().expect("session accept");
        // Pump packets until we see a Command (skip any incidental
        // metadata) or the session ends.
        let mut found = None;
        for _ in 0..16 {
            match session.next_packet() {
                Ok(Some(StreamPacket::Command(c))) => {
                    found = Some(c);
                    break;
                }
                Ok(Some(_)) => continue,
                Ok(None) => break,
                Err(_) => break,
            }
        }
        got_tx.send(found).unwrap();
    });

    thread::sleep(Duration::from_millis(40));
    let stream = TcpStream::connect(addr).expect("client connect");
    let cmd_for_client = cmd.clone();
    let client_thread = thread::spawn(move || {
        run_minimal_client(stream, |writer, stream_id| {
            // Let the server settle into the session before the command.
            thread::sleep(Duration::from_millis(60));
            writer
                .write_message(CSID_COMMAND, &cmd_for_client.to_message(stream_id))
                .expect("write netstream command");
            writer.flush().expect("flush command");
            thread::sleep(Duration::from_millis(150));
        });
    });

    let observed = got_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("server signal");
    client_thread.join().expect("client thread");
    server_thread.join().expect("server thread");

    assert_eq!(
        observed,
        Some(cmd),
        "server must surface the NetStream command as StreamPacket::Command"
    );
}

#[test]
fn server_surfaces_pause_command() {
    assert_command_surfaced(NetStreamCommand::Pause {
        pause: true,
        milliseconds: 5000.0,
    });
}

#[test]
fn server_surfaces_receive_audio_command() {
    assert_command_surfaced(NetStreamCommand::ReceiveAudio(false));
}

#[test]
fn server_surfaces_seek_command() {
    assert_command_surfaced(NetStreamCommand::Seek {
        milliseconds: 12_000.0,
    });
}

#[test]
fn server_surfaces_play_command() {
    assert_command_surfaced(NetStreamCommand::Play {
        stream_name: "mp4:clip.m4v".into(),
        start: Some(-2.0),
        duration: Some(-1.0),
        reset: Some(true),
    });
}

/// A teardown command (`closeStream`) must NOT surface as a
/// `StreamPacket::Command`: it ends the session silently. The server's
/// pump sees `None` (end of session) rather than a Command.
#[test]
fn server_does_not_surface_teardown_as_command() {
    let server = RtmpServer::bind("127.0.0.1:0").expect("bind");
    let addr = server.local_addr().expect("local_addr");
    let (got_tx, got_rx) = mpsc::channel::<bool>();

    let server_thread = thread::spawn(move || {
        let req = server.accept().expect("server accept");
        let mut session = req.accept().expect("session accept");
        let mut saw_command = false;
        for _ in 0..16 {
            match session.next_packet() {
                Ok(Some(StreamPacket::Command(_))) => {
                    saw_command = true;
                    break;
                }
                Ok(Some(_)) => continue,
                Ok(None) => break,
                Err(_) => break,
            }
        }
        got_tx.send(saw_command).unwrap();
    });

    thread::sleep(Duration::from_millis(40));
    let stream = TcpStream::connect(addr).expect("client connect");
    let client_thread = thread::spawn(move || {
        run_minimal_client(stream, |writer, stream_id| {
            thread::sleep(Duration::from_millis(60));
            // closeStream is a teardown command — consumed silently.
            let teardown = Message {
                msg_type_id: 20,
                msg_stream_id: stream_id,
                timestamp: 0,
                payload: oxideav_rtmp::amf::encode_command(
                    "closeStream",
                    0.0,
                    oxideav_rtmp::Amf0Value::Null,
                    &[],
                ),
            };
            writer
                .write_message(CSID_COMMAND, &teardown)
                .expect("write closeStream");
            writer.flush().expect("flush closeStream");
            thread::sleep(Duration::from_millis(150));
        });
    });

    let saw_command = got_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("server signal");
    client_thread.join().expect("client thread");
    server_thread.join().expect("server thread");

    assert!(
        !saw_command,
        "teardown command (closeStream) must end the session silently, not surface as Command"
    );
}
