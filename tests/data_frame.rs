//! `@setDataFrame` / `@clearDataFrame` conformance — the reserved
//! server-intercepted data-frame control names per
//! docs/streaming/rtmp/rtmp-so-dataframe-digest-handshake.md §2:
//! wire-shape goldens (three AMF values for set, two for clear), the
//! server-side strip/store/replay behaviour, and the publisher-side
//! client methods end-to-end over loopback.

use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use oxideav_rtmp::message::{
    build_clear_data_frame, build_data_message, build_on_meta_data, build_set_data_frame,
    build_set_data_frame_named,
};
use oxideav_rtmp::{
    amf, parse_data_frame, Amf0Value, DataFrameCommand, RtmpClient, RtmpServer, StreamPacket,
};

fn sample_meta() -> Amf0Value {
    Amf0Value::EcmaArray(vec![
        ("width".into(), Amf0Value::Number(1280.0)),
        ("height".into(), Amf0Value::Number(720.0)),
        ("videocodecid".into(), Amf0Value::Number(7.0)),
    ])
}

/// §2.2 golden: body = three AMF0 values — "@setDataFrame" string,
/// handler string, metadata value.
#[test]
fn set_data_frame_wire_shape() {
    let msg = build_set_data_frame(5, sample_meta());
    assert_eq!(msg.msg_type_id, 18);
    assert_eq!(msg.msg_stream_id, 5);

    let mut expect = Vec::new();
    amf::encode(&mut expect, &Amf0Value::String("@setDataFrame".into()));
    amf::encode(&mut expect, &Amf0Value::String("onMetaData".into()));
    amf::encode(&mut expect, &sample_meta());
    assert_eq!(msg.payload, expect);

    // Round-trips through the classifier.
    let values = amf::decode_all(&msg.payload).expect("decode");
    assert_eq!(values.len(), 3);
    assert_eq!(
        parse_data_frame(&values),
        Some(DataFrameCommand::Set {
            handler: "onMetaData".into(),
            value: sample_meta(),
        })
    );
}

/// §2.3 golden: body = exactly two AMF0 values — "@clearDataFrame"
/// and the handler name; no payload argument.
#[test]
fn clear_data_frame_wire_shape() {
    let msg = build_clear_data_frame(5, "onMetaData");
    assert_eq!(msg.msg_type_id, 18);

    let mut expect = Vec::new();
    amf::encode(&mut expect, &Amf0Value::String("@clearDataFrame".into()));
    amf::encode(&mut expect, &Amf0Value::String("onMetaData".into()));
    assert_eq!(msg.payload, expect);

    let values = amf::decode_all(&msg.payload).expect("decode");
    assert_eq!(values.len(), 2);
    assert_eq!(
        parse_data_frame(&values),
        Some(DataFrameCommand::Clear {
            handler: "onMetaData".into(),
        })
    );
}

/// §2 strip diagram: the stored `[handler, value]` pair re-emitted to
/// a subscriber is exactly the set message minus its first value.
#[test]
fn server_replay_shape_is_set_minus_control_name() {
    let publish = build_set_data_frame_named(1, "onMetaData", sample_meta());
    let replay = build_on_meta_data(1, &sample_meta());

    let mut publish_vals = amf::decode_all(&publish.payload).expect("decode publish");
    let replay_vals = amf::decode_all(&replay.payload).expect("decode replay");
    publish_vals.remove(0); // strip "@setDataFrame"
    assert_eq!(publish_vals, replay_vals);

    // build_data_message is the generic form of the replay shape.
    let generic = build_data_message(1, "onMetaData", &sample_meta());
    assert_eq!(generic.payload, replay.payload);
}

/// Classifier negatives: bare handlers and malformed controls are not
/// data-frame commands.
#[test]
fn parse_data_frame_negatives() {
    // Bare onMetaData (no control prefix).
    let bare = [Amf0Value::String("onMetaData".into()), sample_meta()];
    assert_eq!(parse_data_frame(&bare), None);

    // Control name without its handler argument.
    let short = [Amf0Value::String("@setDataFrame".into())];
    assert_eq!(parse_data_frame(&short), None);

    // Set without the payload argument.
    let no_payload = [
        Amf0Value::String("@setDataFrame".into()),
        Amf0Value::String("onMetaData".into()),
    ];
    assert_eq!(parse_data_frame(&no_payload), None);

    // Handler of the wrong AMF type.
    let bad_handler = [
        Amf0Value::String("@clearDataFrame".into()),
        Amf0Value::Number(3.0),
    ];
    assert_eq!(parse_data_frame(&bad_handler), None);

    // Empty list.
    assert_eq!(parse_data_frame(&[]), None);
}

#[derive(Debug, PartialEq)]
enum Seen {
    Metadata(Amf0Value),
    DataFrame(String, Amf0Value),
    Cleared(String),
}

/// End-to-end over loopback: the publisher sets `onMetaData` + a
/// custom `onCuePoint` frame, then clears both; the server session
/// surfaces the typed packets and its stored data-frame state tracks
/// set → upsert → clear.
#[test]
fn publish_set_and_clear_data_frames_end_to_end() {
    let server = RtmpServer::bind("127.0.0.1:0").expect("bind");
    let addr = server.local_addr().expect("local_addr");

    let (tx, rx) = mpsc::channel::<(Seen, Vec<(String, Amf0Value)>)>();
    let server_thread = thread::spawn(move || {
        let req = server.accept().expect("accept");
        let mut session = req.accept().expect("session");
        session
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("set timeout");
        loop {
            match session.next_packet() {
                Ok(Some(pkt)) => {
                    let seen = match pkt {
                        StreamPacket::Metadata(v) => Seen::Metadata(v),
                        StreamPacket::DataFrame { handler, value } => {
                            Seen::DataFrame(handler, value)
                        }
                        StreamPacket::DataFrameCleared { handler } => Seen::Cleared(handler),
                        _ => continue,
                    };
                    tx.send((seen, session.data_frames().to_vec())).unwrap();
                }
                Ok(None) => break,
                Err(_) => break,
            }
        }
    });

    thread::sleep(Duration::from_millis(50));
    let url = format!("rtmp://{}:{}/live/df-key", addr.ip(), addr.port());
    let mut client = RtmpClient::connect(&url).expect("connect");

    let cue = Amf0Value::Object(vec![("name".into(), Amf0Value::String("intro".into()))]);
    let meta2 = Amf0Value::EcmaArray(vec![("width".into(), Amf0Value::Number(1920.0))]);

    client.send_metadata(sample_meta()).expect("set onMetaData");
    client
        .send_data_frame("onCuePoint", cue.clone())
        .expect("set onCuePoint");
    // Upsert: second set under the same handler replaces the frame.
    client.send_metadata(meta2.clone()).expect("upsert");
    client.clear_data_frame("onCuePoint").expect("clear cue");
    client.clear_metadata().expect("clear onMetaData");

    thread::sleep(Duration::from_millis(50));
    client.close().expect("close");
    server_thread.join().expect("server thread");

    let events: Vec<(Seen, Vec<(String, Amf0Value)>)> =
        std::iter::from_fn(|| rx.recv_timeout(Duration::from_millis(500)).ok()).collect();
    assert_eq!(events.len(), 5, "got {events:?}");

    // 1. onMetaData set — stored.
    assert_eq!(events[0].0, Seen::Metadata(sample_meta()));
    assert_eq!(events[0].1, vec![("onMetaData".to_owned(), sample_meta())]);
    // 2. onCuePoint set — both stored, arrival order.
    assert_eq!(
        events[1].0,
        Seen::DataFrame("onCuePoint".into(), cue.clone())
    );
    assert_eq!(
        events[1].1,
        vec![
            ("onMetaData".to_owned(), sample_meta()),
            ("onCuePoint".to_owned(), cue.clone()),
        ]
    );
    // 3. onMetaData upsert — replaced in place, order kept.
    assert_eq!(events[2].0, Seen::Metadata(meta2.clone()));
    assert_eq!(
        events[2].1,
        vec![
            ("onMetaData".to_owned(), meta2.clone()),
            ("onCuePoint".to_owned(), cue.clone()),
        ]
    );
    // 4. onCuePoint cleared.
    assert_eq!(events[3].0, Seen::Cleared("onCuePoint".into()));
    assert_eq!(events[3].1, vec![("onMetaData".to_owned(), meta2.clone())]);
    // 5. onMetaData cleared — store empty.
    assert_eq!(events[4].0, Seen::Cleared("onMetaData".into()));
    assert!(events[4].1.is_empty());
}

/// A bare `["onMetaData", meta]` (no control prefix) still surfaces
/// as Metadata and still updates the stored onMetaData frame, while a
/// bare `["onCuePoint", obj]` stays live-only (surfaced, not stored).
#[test]
fn bare_data_messages_store_only_metadata() {
    let server = RtmpServer::bind("127.0.0.1:0").expect("bind");
    let addr = server.local_addr().expect("local_addr");

    let (tx, rx) = mpsc::channel::<(Amf0Value, Vec<(String, Amf0Value)>)>();
    let server_thread = thread::spawn(move || {
        let req = server.accept().expect("accept");
        let mut session = req.accept().expect("session");
        session
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("set timeout");
        loop {
            match session.next_packet() {
                Ok(Some(StreamPacket::Metadata(v))) => {
                    tx.send((v, session.data_frames().to_vec())).unwrap();
                }
                Ok(Some(_)) => {}
                Ok(None) => break,
                Err(_) => break,
            }
        }
    });

    thread::sleep(Duration::from_millis(50));
    let url = format!("rtmp://{}:{}/live/bare-key", addr.ip(), addr.port());
    let mut client = RtmpClient::connect(&url).expect("connect");

    // Reach the raw-message surface through the AMF3 metadata path
    // (bare ["onMetaData", meta], no @setDataFrame prefix) — this is
    // also the type-15 bridge coverage.
    let meta3 =
        oxideav_rtmp::amf3::dynamic_object([("duration", oxideav_rtmp::Amf3Value::Double(12.5))]);
    client.send_metadata_amf3(meta3).expect("amf3 metadata");

    thread::sleep(Duration::from_millis(50));
    client.close().expect("close");
    server_thread.join().expect("server thread");

    let (seen, stored) = rx
        .recv_timeout(Duration::from_secs(5))
        .expect("metadata must arrive");
    assert_eq!(seen.get("duration").and_then(Amf0Value::as_f64), Some(12.5));
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].0, "onMetaData");
}

/// The full §2 replay story: a publisher stores data frames, a *late*
/// subscriber connects afterwards, and the server hands it the stored
/// state (`RtmpSession::data_frames` → `PlaySession::send_data`) —
/// each frame arriving unwrapped (no `@setDataFrame` prefix).
#[test]
fn late_subscriber_receives_replayed_data_frames() {
    let server = RtmpServer::bind("127.0.0.1:0").expect("bind");
    let addr = server.local_addr().expect("local_addr");

    let (ready_tx, ready_rx) = mpsc::channel::<()>();
    let server_thread = thread::spawn(move || {
        // Connection 1: the publisher.
        let mut publish = match server.accept_any().expect("accept publisher") {
            oxideav_rtmp::SessionRequest::Publish(req) => req.accept().expect("session"),
            oxideav_rtmp::SessionRequest::Play(_) => panic!("expected publisher first"),
        };
        publish
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("timeout");
        // Pump until both frames are stored.
        while publish.data_frames().len() < 2 {
            match publish.next_packet() {
                Ok(Some(_)) => {}
                Ok(None) | Err(_) => panic!("publisher ended before frames stored"),
            }
        }
        let stored = publish.data_frames().to_vec();
        ready_tx.send(()).unwrap();

        // Connection 2: the late subscriber — replay the stored state.
        let mut play = match server.accept_any().expect("accept subscriber") {
            oxideav_rtmp::SessionRequest::Play(req) => req.accept().expect("play session"),
            oxideav_rtmp::SessionRequest::Publish(_) => panic!("expected subscriber second"),
        };
        for (handler, value) in &stored {
            play.send_data(handler, value).expect("replay frame");
        }
        play.close().expect("close play");

        // Let the publisher finish.
        while let Ok(Some(_)) = publish.next_packet() {}
    });

    thread::sleep(Duration::from_millis(50));
    let url = format!("rtmp://{}:{}/live/replay-key", addr.ip(), addr.port());
    let mut publisher = RtmpClient::connect(&url).expect("publisher connect");
    let cue = Amf0Value::Object(vec![("t".into(), Amf0Value::Number(0.0))]);
    publisher.send_metadata(sample_meta()).expect("metadata");
    publisher
        .send_data_frame("onCuePoint", cue.clone())
        .expect("cue");

    // Wait until the server confirms both frames are stored, then
    // bring up the late subscriber.
    ready_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("server must store both frames");
    let play_url = format!("rtmp://{}:{}/live/replay-play", addr.ip(), addr.port());
    let mut player = oxideav_rtmp::RtmpPlayer::connect(&play_url).expect("player connect");

    let mut got: Vec<Amf0Value> = Vec::new();
    while let Ok(Some(pkt)) = player.next_packet() {
        if let oxideav_rtmp::PlayerPacket::Metadata(v) = pkt {
            got.push(v);
        }
    }
    assert_eq!(
        got,
        vec![sample_meta(), cue],
        "late subscriber must receive both stored frames, unwrapped, in order"
    );

    thread::sleep(Duration::from_millis(50));
    publisher.close().expect("publisher close");
    server_thread.join().expect("server thread");
}
