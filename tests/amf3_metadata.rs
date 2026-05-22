//! End-to-end check that an AMF3-encoded `onMetaData` data message
//! (RTMP message type 15) decodes through the server's message-dispatch
//! path and surfaces as a `StreamPacket::Metadata`.
//!
//! r93 added the AMF3 wire-format parser; this exercises r96's wiring of
//! that parser into the server's `next_packet` routing so an AMF3 data
//! message reaches the consumer with the same shape an AMF0 one would.

use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use oxideav_rtmp::{Amf0Value, Amf3Value, RtmpClient, RtmpServer, StreamPacket};

const APP: &str = "live";
const STREAM_KEY: &str = "amf3-key";

#[test]
fn server_routes_amf3_onmetadata() {
    let server = RtmpServer::bind("127.0.0.1:0").expect("bind");
    let addr = server.local_addr().expect("local_addr");

    let (meta_tx, meta_rx) = mpsc::channel::<Amf0Value>();

    let server_thread = thread::spawn(move || {
        let req = server.accept().expect("server accept");
        let mut session = req.accept().expect("session accept");
        session
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("set_read_timeout");
        loop {
            match session.next_packet() {
                Ok(Some(StreamPacket::Metadata(m))) => {
                    meta_tx.send(m).unwrap();
                }
                Ok(Some(_)) => {}
                Ok(None) => break,
                Err(_) => break,
            }
        }
    });

    thread::sleep(Duration::from_millis(50));

    let url = format!("rtmp://{}:{}/{APP}/{STREAM_KEY}", addr.ip(), addr.port());
    let mut client = RtmpClient::connect(&url).expect("client connect");

    // AMF3 onMetaData with a mix of integer / double / string fields.
    let meta = oxideav_rtmp::amf3::dynamic_object([
        ("width", Amf3Value::Integer(1920)),
        ("height", Amf3Value::Integer(1080)),
        ("framerate", Amf3Value::Double(59.94)),
        ("videocodecid", Amf3Value::String("avc1".into())),
        ("audiocodecid", Amf3Value::String("mp4a".into())),
    ]);
    client.send_metadata_amf3(meta).expect("send amf3 metadata");

    // Give the server a moment to process, then close.
    thread::sleep(Duration::from_millis(50));
    client.close().expect("client close");
    server_thread.join().expect("server thread");

    let got = meta_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("metadata must arrive");

    // The AMF3 object should have bridged onto an AMF0 Object with the
    // same fields and values.
    assert_eq!(got.get("width").and_then(Amf0Value::as_f64), Some(1920.0));
    assert_eq!(got.get("height").and_then(Amf0Value::as_f64), Some(1080.0));
    assert_eq!(
        got.get("framerate").and_then(Amf0Value::as_f64),
        Some(59.94)
    );
    assert_eq!(
        got.get("videocodecid").and_then(Amf0Value::as_str),
        Some("avc1")
    );
    assert_eq!(
        got.get("audiocodecid").and_then(Amf0Value::as_str),
        Some("mp4a")
    );
}
