//! End-to-end acceptance of AMF3-encoded (type 17) command messages by
//! the server's protocol driver.
//!
//! Enhanced RTMP v2 ("Enabling AMF3 in RTMP") requires a server to
//! accept Command Messages on message type 17 from an objectEncoding-3
//! peer. The v2 "Important AMF3-encoded Historical Specification
//! Clarification" defines the type-17 payload: a leading format
//! selector byte (only format 0 = AMF0 values is defined), with any
//! individual AMF3 value introduced by the AMF0 avmplus-object-marker
//! (0x11).
//!
//! This test hand-rolls a minimal publisher whose `connect` /
//! `createStream` / `publish` commands all ride type 17 — `connect`
//! and `publish` in the v2 selector framing, `createStream` in the
//! legacy leading-0x11 AMF3 shape — and verifies the stock
//! `RtmpServer` negotiates it to a live publish session end-to-end.

use std::net::TcpStream;
use std::thread;
use std::time::Duration;

use oxideav_rtmp::amf::{self, Amf0Value};
use oxideav_rtmp::amf3::{self, Amf3Value};
use oxideav_rtmp::chunk::{ChunkReader, ChunkWriter, Message};
use oxideav_rtmp::message::{
    CSID_AUDIO, CSID_COMMAND, MSG_AUDIO, MSG_COMMAND_AMF0, MSG_COMMAND_AMF3, MSG_SET_CHUNK_SIZE,
};
use oxideav_rtmp::{RtmpServer, StreamPacket};

const APP: &str = "live";
const STREAM_KEY: &str = "amf3-cmd-key";

/// Wrap an already-AMF0-encoded command payload in the v2-clarified
/// type-17 frame: format selector 0 followed by the AMF0 values.
fn selector_frame(amf0_payload: Vec<u8>) -> Vec<u8> {
    let mut framed = vec![amf3::FORMAT_SELECTOR_AMF0];
    framed.extend(amf0_payload);
    framed
}

/// Encode a command as the legacy (selector-less) AMF3 shape: each
/// value 0x11-prefixed AMF3.
fn legacy_amf3_command(name: &str, tx_id: f64) -> Vec<u8> {
    let mut payload = Vec::new();
    for v in [
        Amf3Value::String(name.into()),
        Amf3Value::Double(tx_id),
        Amf3Value::Null,
    ] {
        payload.push(amf3::AVMPLUS_OBJECT_MARKER);
        amf3::encode(&mut payload, &v);
    }
    payload
}

fn command_msg(payload: Vec<u8>) -> Message {
    Message {
        msg_type_id: MSG_COMMAND_AMF3,
        msg_stream_id: 0,
        timestamp: 0,
        payload,
    }
}

/// Pump the reader until an AMF0 `_result` / `onStatus` command
/// arrives, applying SetChunkSize live like a real client would.
fn await_command(reader: &mut ChunkReader<TcpStream>, want: &str) -> Vec<Amf0Value> {
    loop {
        let msg = reader.read_message().expect("read server message");
        match msg.msg_type_id {
            MSG_SET_CHUNK_SIZE => {
                let mut b = [0u8; 4];
                b.copy_from_slice(&msg.payload[..4]);
                reader.set_chunk_size((u32::from_be_bytes(b) & 0x7FFF_FFFF) as usize);
            }
            MSG_COMMAND_AMF0 => {
                let values = amf::decode_all(&msg.payload).expect("decode server command");
                if values.first().and_then(Amf0Value::as_str) == Some(want) {
                    return values;
                }
            }
            _ => {}
        }
    }
}

#[test]
fn server_negotiates_amf3_command_publisher() {
    let server = RtmpServer::bind("127.0.0.1:0").expect("bind");
    let addr = server.local_addr().expect("local_addr");

    let server_thread = thread::spawn(move || {
        let req = server.accept().expect("server accept");
        assert_eq!(req.app, APP);
        assert_eq!(req.stream_name, STREAM_KEY);
        let mut session = req.accept().expect("session accept");
        session
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("set_read_timeout");
        let mut audio = Vec::new();
        loop {
            match session.next_packet() {
                Ok(Some(StreamPacket::Audio { tag, .. })) => audio.push(tag.body.clone()),
                Ok(Some(_)) => {}
                Ok(None) | Err(_) => break,
            }
        }
        audio
    });

    thread::sleep(Duration::from_millis(50));

    // --- Hand-rolled AMF3-command publisher ---
    let stream = TcpStream::connect(addr).expect("tcp connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("timeout");
    let mut hs = stream.try_clone().expect("clone");
    oxideav_rtmp::handshake::client_handshake(&mut hs).expect("handshake");

    let mut reader = ChunkReader::new(stream.try_clone().expect("clone"));
    let mut writer = ChunkWriter::new(stream.try_clone().expect("clone"));

    // connect — v2 selector frame around AMF0 values (objectEncoding 3).
    let cmd_obj = Amf0Value::Object(vec![
        ("app".into(), Amf0Value::String(APP.into())),
        (
            "tcUrl".into(),
            Amf0Value::String(format!("rtmp://{}:{}/{APP}", addr.ip(), addr.port())),
        ),
        ("objectEncoding".into(), Amf0Value::Number(3.0)),
    ]);
    let payload = amf::encode_command("connect", 1.0, cmd_obj, &[]);
    writer
        .write_message(CSID_COMMAND, &command_msg(selector_frame(payload)))
        .expect("send connect");
    writer.flush().expect("flush");
    let result = await_command(&mut reader, "_result");
    assert_eq!(result.get(1).and_then(Amf0Value::as_f64), Some(1.0));

    // createStream — legacy leading-0x11 AMF3 shape.
    writer
        .write_message(
            CSID_COMMAND,
            &command_msg(legacy_amf3_command("createStream", 2.0)),
        )
        .expect("send createStream");
    writer.flush().expect("flush");
    let result = await_command(&mut reader, "_result");
    assert_eq!(result.get(1).and_then(Amf0Value::as_f64), Some(2.0));
    let stream_id = result
        .get(3)
        .and_then(Amf0Value::as_f64)
        .expect("stream id") as u32;

    // publish — selector frame again.
    let payload = amf::encode_command(
        "publish",
        3.0,
        Amf0Value::Null,
        &[
            Amf0Value::String(STREAM_KEY.into()),
            Amf0Value::String("live".into()),
        ],
    );
    writer
        .write_message(CSID_COMMAND, &command_msg(selector_frame(payload)))
        .expect("send publish");
    writer.flush().expect("flush");
    let status = await_command(&mut reader, "onStatus");
    let info = status.last().expect("status info");
    assert_eq!(
        info.get("code").and_then(Amf0Value::as_str),
        Some("NetStream.Publish.Start")
    );

    // One legacy AAC audio tag proves the session is live.
    let body = vec![0xAF, 0x01, 0x21, 0x43, 0x65];
    writer
        .write_message(
            CSID_AUDIO,
            &Message {
                msg_type_id: MSG_AUDIO,
                msg_stream_id: stream_id,
                timestamp: 0,
                payload: body.clone(),
            },
        )
        .expect("send audio");
    writer.flush().expect("flush");
    thread::sleep(Duration::from_millis(100));
    drop(writer);
    drop(reader);
    stream.shutdown(std::net::Shutdown::Both).ok();

    let audio = server_thread.join().expect("server thread");
    assert_eq!(audio.len(), 1, "audio tag must arrive");
    // Legacy AAC tag: AudioTag::body strips the SoundFormat byte and
    // the AACPacketType byte.
    assert_eq!(audio[0], body[2..].to_vec());
}
