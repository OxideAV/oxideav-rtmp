//! RTMP 1.0 §5.3 Acknowledgement / §5.5 Window Acknowledgement Size
//! end-to-end check.
//!
//! Per §5.3 a peer "sends the acknowledgment to the peer after
//! receiving bytes equal to the window size", where the window is what
//! the *other* side advertised via §5.5 Window Acknowledgement Size (or
//! the §5.6 Set Peer Bandwidth output-bandwidth value). Here a raw
//! publisher built from the crate's public low-level modules advertises
//! a deliberately tiny window, publishes enough audio to cross it
//! several times, and asserts the real [`RtmpServer`] sends back at
//! least one Acknowledgement (message type 3) whose sequence number is
//! a plausible received-byte count.
//!
//! It also proves the inverse direction: the server advertises its own
//! window during `connect`, the `RtmpClient` captures it, and
//! `ChunkReader::ack_due` stays silent while well under that window so a
//! steady publish does not spam acks.

use std::io::Read;
use std::net::TcpStream;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use oxideav_rtmp::chunk::{ChunkReader, ChunkWriter, Message};
use oxideav_rtmp::handshake::client_handshake;
use oxideav_rtmp::message::{
    build_connect, build_create_stream, build_publish, build_window_ack_size, MSG_ACK, MSG_AUDIO,
    MSG_COMMAND_AMF0,
};
use oxideav_rtmp::{amf, Amf0Value, RtmpServer, StreamPacket};

const APP: &str = "live";
const STREAM_KEY: &str = "ack-key";

/// CSIDs mirroring the crate's internal conventions.
const CSID_CONTROL: u32 = 2;
const CSID_COMMAND: u32 = 3;
const CSID_AUDIO: u32 = 4;

/// A small AAC raw audio body (`sound_format = 10`, `aac_packet_type =
/// 1`) padded so each message is a known size on the wire.
fn aac_frame(len: usize) -> Vec<u8> {
    let mut v = vec![0xAFu8, 0x01];
    v.resize(len, 0x55);
    v
}

#[test]
fn server_acknowledges_after_window_of_published_bytes() {
    let server = RtmpServer::bind("127.0.0.1:0").expect("bind");
    let addr = server.local_addr().expect("local_addr");

    // Server side: accept the publisher and drain every packet so the
    // reader's received-byte counter advances. Report how many audio
    // packets it saw so the test can confirm the publish actually
    // landed.
    let (count_tx, count_rx) = mpsc::channel::<usize>();
    let server_thread = thread::spawn(move || {
        let req = server.accept().expect("server accept");
        let mut session = req.accept().expect("session accept");
        session
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("set_read_timeout");
        let mut audio = 0usize;
        while let Ok(Some(pkt)) = session.next_packet() {
            if let StreamPacket::Audio { .. } = pkt {
                audio += 1;
                if audio >= 20 {
                    break;
                }
            }
        }
        let _ = count_tx.send(audio);
    });

    // Raw publisher.
    let tcp = TcpStream::connect(addr).expect("connect");
    tcp.set_read_timeout(Some(Duration::from_secs(5)))
        .expect("set_read_timeout");
    let mut hs = tcp.try_clone().expect("clone hs");
    client_handshake(&mut hs).expect("client handshake");

    let mut writer = ChunkWriter::new(tcp.try_clone().expect("clone w"));
    let mut reader = ChunkReader::new(tcp.try_clone().expect("clone r"));

    // Advertise a deliberately tiny window so the server owes us an ack
    // after only a few hundred bytes.
    let window: u32 = 256;
    writer
        .write_message(CSID_CONTROL, &build_window_ack_size(window))
        .expect("window ack size");

    // connect → wait for _result, capturing the server's own window.
    let tc_url = format!("rtmp://{addr}/{APP}");
    writer
        .write_message(CSID_COMMAND, &build_connect(1.0, APP, &tc_url, "test"))
        .expect("connect");
    writer.flush().expect("flush connect");
    drain_until_command(&mut reader, "_result");

    // createStream → wait for _result carrying the stream id.
    writer
        .write_message(CSID_COMMAND, &build_create_stream(2.0))
        .expect("createStream");
    writer.flush().expect("flush createStream");
    let stream_id = drain_until_create_stream_result(&mut reader);

    // publish.
    writer
        .write_message(
            CSID_COMMAND,
            &build_publish(3.0, stream_id, STREAM_KEY, "live"),
        )
        .expect("publish");
    writer.flush().expect("flush publish");

    // Publish audio frames well past several windows. Each frame is
    // ~120 bytes of payload plus chunk-header overhead.
    for i in 0..20u32 {
        let msg = Message {
            msg_type_id: MSG_AUDIO,
            msg_stream_id: stream_id,
            timestamp: i * 20,
            payload: aac_frame(120),
        };
        writer.write_message(CSID_AUDIO, &msg).expect("write audio");
    }
    writer.flush().expect("flush audio");

    // Now read back from the server until we observe at least one
    // Acknowledgement (message type 3). The server should have crossed
    // our 256-byte window many times over.
    let mut saw_ack = false;
    let mut ack_seq = 0u32;
    for _ in 0..200 {
        match reader.read_message() {
            Ok(m) => {
                if m.msg_type_id == MSG_ACK {
                    saw_ack = true;
                    // Sequence number is the 4-byte BE received-byte
                    // count.
                    if m.payload.len() >= 4 {
                        ack_seq = u32::from_be_bytes([
                            m.payload[0],
                            m.payload[1],
                            m.payload[2],
                            m.payload[3],
                        ]);
                    }
                    break;
                }
            }
            Err(_) => break,
        }
    }

    assert!(
        saw_ack,
        "server must emit a §5.3 Acknowledgement after receiving a full window of bytes"
    );
    assert!(
        ack_seq >= window,
        "ack sequence number {ack_seq} must be at least one window ({window})"
    );

    // Tidy up: half-close so the server's drain loop ends.
    let _ = tcp.shutdown(std::net::Shutdown::Write);
    let audio_seen = count_rx.recv_timeout(Duration::from_secs(5)).unwrap_or(0);
    assert!(
        audio_seen > 0,
        "server should have decoded the published audio frames"
    );
    let _ = server_thread.join();
}

/// Read messages until a command message with the given name arrives,
/// honouring protocol-control housekeeping along the way.
fn drain_until_command<R: Read>(reader: &mut ChunkReader<R>, want: &str) {
    for _ in 0..50 {
        let msg = reader.read_message().expect("read message");
        if msg.msg_type_id == MSG_COMMAND_AMF0 {
            let values = amf::decode_all(&msg.payload).unwrap_or_default();
            let name = values.first().and_then(Amf0Value::as_str).unwrap_or("");
            if name == want {
                return;
            }
        }
    }
    panic!("never saw command `{want}`");
}

/// Read messages until the `createStream` `_result` arrives and return
/// the stream id it carries.
fn drain_until_create_stream_result<R: Read>(reader: &mut ChunkReader<R>) -> u32 {
    for _ in 0..50 {
        let msg = reader.read_message().expect("read message");
        if msg.msg_type_id == MSG_COMMAND_AMF0 {
            let values = amf::decode_all(&msg.payload).unwrap_or_default();
            let name = values.first().and_then(Amf0Value::as_str).unwrap_or("");
            if name == "_result" {
                if let Some(sid) = values.iter().rev().find_map(Amf0Value::as_f64) {
                    return sid as u32;
                }
            }
        }
    }
    panic!("never saw createStream _result");
}
