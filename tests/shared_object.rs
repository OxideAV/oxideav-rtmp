//! Shared Object message body conformance — framing per
//! docs/streaming/rtmp/rtmp-so-dataframe-digest-handshake.md §1:
//! header (UI16 name len + name + UI32 version + UI32 flags + 4
//! reserved), then back-to-back events (UI8 type + UI32 length +
//! data), property names as bare UTF-8 strings and values as full
//! AMF0/AMF3 values.

use oxideav_rtmp::shared_object::{
    parse_shared_object, SharedObjectMessage, SoEvent, SO_EVENT_CHANGE, SO_EVENT_SEND_MESSAGE,
    SO_EVENT_STATUS, SO_EVENT_USE, SO_FLAG_PERSISTENT,
};
use oxideav_rtmp::{Amf0Value, Amf3Value};

fn all_events_amf0() -> Vec<SoEvent<Amf0Value>> {
    vec![
        SoEvent::Use,
        SoEvent::Release,
        SoEvent::RequestChange {
            name: "score".into(),
            value: Amf0Value::Number(41.0),
        },
        SoEvent::Change {
            pairs: vec![
                ("score".into(), Amf0Value::Number(42.0)),
                ("label".into(), Amf0Value::String("high".into())),
                ("live".into(), Amf0Value::Boolean(true)),
            ],
        },
        SoEvent::Success {
            name: "score".into(),
        },
        SoEvent::SendMessage {
            handler: "chat".into(),
            args: vec![
                Amf0Value::String("hello".into()),
                Amf0Value::Number(3.0),
                Amf0Value::Null,
            ],
        },
        SoEvent::Status {
            code: "SharedObject.NoReadAccess".into(),
            level: "error".into(),
        },
        SoEvent::Clear,
        SoEvent::Remove {
            name: "label".into(),
        },
        SoEvent::RequestRemove {
            name: "live".into(),
        },
        SoEvent::UseSuccess,
    ]
}

/// Every documented event type round-trips through the AMF0 (type 19)
/// encoding.
#[test]
fn amf0_all_event_types_round_trip() {
    let mut so = SharedObjectMessage::new("game/lobby-1");
    so.version = 1234;
    so.flags = SO_FLAG_PERSISTENT;
    so.events = all_events_amf0();

    let body = so.build_amf0().expect("build");
    let back = SharedObjectMessage::parse_amf0(&body).expect("parse");
    assert_eq!(back, so);
    assert!(back.is_persistent());

    // Wire event-type codes must match the §1.3 table order 1..=11.
    let codes: Vec<u8> = so.events.iter().map(|e| e.event_type()).collect();
    assert_eq!(codes, (1..=11).collect::<Vec<u8>>());
}

/// The AMF3 (type 16) flavour: identical framing, AMF3-marked values.
#[test]
fn amf3_events_round_trip_and_bridge_to_amf0() {
    let mut so: SharedObjectMessage<Amf3Value> = SharedObjectMessage::new("scores");
    so.version = 9;
    so.events = vec![
        SoEvent::Use,
        SoEvent::Change {
            pairs: vec![
                ("count".into(), Amf3Value::Integer(17)),
                ("who".into(), Amf3Value::String("ai".into())),
            ],
        },
        SoEvent::SendMessage {
            handler: "notify".into(),
            args: vec![Amf3Value::Double(2.5), Amf3Value::Boolean(true)],
        },
    ];

    let body = so.build_amf3().expect("build");
    let back = SharedObjectMessage::parse_amf3(&body).expect("parse");
    assert_eq!(back, so);

    // The one-stop Message-level entry point bridges AMF3 onto AMF0.
    let msg = so.to_message_amf3(0).expect("to_message");
    assert_eq!(msg.msg_type_id, 16);
    let bridged = parse_shared_object(&msg).expect("bridged parse");
    assert_eq!(bridged.name, "scores");
    assert_eq!(bridged.version, 9);
    match &bridged.events[1] {
        SoEvent::Change { pairs } => {
            assert_eq!(pairs[0], ("count".into(), Amf0Value::Number(17.0)));
            assert_eq!(pairs[1], ("who".into(), Amf0Value::String("ai".into())));
        }
        other => panic!("expected Change, got {other:?}"),
    }
    match &bridged.events[2] {
        SoEvent::SendMessage { handler, args } => {
            assert_eq!(handler, "notify");
            assert_eq!(args, &[Amf0Value::Number(2.5), Amf0Value::Boolean(true)]);
        }
        other => panic!("expected SendMessage, got {other:?}"),
    }
}

/// The AMF0 Message wrapper carries type 19 and the exact body bytes.
#[test]
fn amf0_message_wrapper_type_and_payload() {
    let mut so = SharedObjectMessage::new("room");
    so.events = vec![SoEvent::Use];
    let msg = so.to_message_amf0(0).expect("to_message");
    assert_eq!(msg.msg_type_id, 19);
    assert_eq!(msg.msg_stream_id, 0);
    assert_eq!(msg.payload, so.build_amf0().expect("build"));
    let back = parse_shared_object(&msg).expect("parse");
    assert_eq!(back, so);
}

/// Unknown event codes must be preserved verbatim (relay round-trip).
#[test]
fn unknown_event_type_round_trips_verbatim() {
    let mut so = SharedObjectMessage::new("x");
    so.events = vec![
        SoEvent::Unknown {
            event_type: 0x2A,
            data: vec![1, 2, 3, 4, 5],
        },
        SoEvent::Clear,
    ];
    let body = so.build_amf0().expect("build");
    let back = SharedObjectMessage::parse_amf0(&body).expect("parse");
    assert_eq!(back, so);
}

/// A server-style empty Success ack parses as an empty name and
/// re-encodes as a zero-length payload.
#[test]
fn empty_success_ack_round_trips_as_zero_length_event() {
    let mut body = Vec::new();
    body.extend_from_slice(&[0x00, 0x01]);
    body.push(b'z');
    body.extend_from_slice(&[0u8; 12]); // version + flags + reserved
    body.push(5); // Success
    body.extend_from_slice(&[0, 0, 0, 0]); // zero-length payload

    let so = SharedObjectMessage::parse_amf0(&body).expect("parse");
    assert_eq!(
        so.events,
        vec![SoEvent::Success {
            name: String::new()
        }]
    );
    assert_eq!(so.build_amf0().expect("build"), body);
}

/// Golden §1 layout check: hand-assembled multi-event message.
#[test]
fn golden_multi_event_layout() {
    let mut body = Vec::new();
    body.extend_from_slice(&[0x00, 0x04]);
    body.extend_from_slice(b"chat");
    body.extend_from_slice(&[0x00, 0x00, 0x00, 0x02]); // version 2
    body.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]); // flags 0
    body.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]); // reserved
                                                       // Use (1), empty.
    body.push(SO_EVENT_USE);
    body.extend_from_slice(&[0, 0, 0, 0]);
    // Status (7): code then level.
    let mut status = Vec::new();
    status.extend_from_slice(&[0x00, 0x02]);
    status.extend_from_slice(b"ok");
    status.extend_from_slice(&[0x00, 0x06]);
    status.extend_from_slice(b"status");
    body.push(SO_EVENT_STATUS);
    body.extend_from_slice(&(status.len() as u32).to_be_bytes());
    body.extend_from_slice(&status);
    // SendMessage (6): handler + one AMF0 number arg.
    let mut send = Vec::new();
    send.extend_from_slice(&[0x00, 0x04]);
    send.extend_from_slice(b"ping");
    send.push(0x00); // AMF0 number marker
    send.extend_from_slice(&7.0f64.to_be_bytes());
    body.push(SO_EVENT_SEND_MESSAGE);
    body.extend_from_slice(&(send.len() as u32).to_be_bytes());
    body.extend_from_slice(&send);

    let so = SharedObjectMessage::parse_amf0(&body).expect("parse");
    assert_eq!(so.name, "chat");
    assert_eq!(so.version, 2);
    assert!(!so.is_persistent());
    assert_eq!(
        so.events,
        vec![
            SoEvent::Use,
            SoEvent::Status {
                code: "ok".into(),
                level: "status".into(),
            },
            SoEvent::SendMessage {
                handler: "ping".into(),
                args: vec![Amf0Value::Number(7.0)],
            },
        ]
    );
    assert_eq!(so.build_amf0().expect("build"), body);
}

// ---------------------------------------------------------------------------
// Robustness: truncations and overruns must be clean errors, never
// panics or hangs.
// ---------------------------------------------------------------------------

#[test]
fn truncation_matrix_errors_cleanly() {
    // Build a healthy two-event message, then re-parse every prefix.
    let mut so = SharedObjectMessage::new("prefix-fuzz");
    so.version = 3;
    so.events = vec![
        SoEvent::Change {
            pairs: vec![("k".into(), Amf0Value::String("v".into()))],
        },
        SoEvent::Remove { name: "k".into() },
    ];
    let body = so.build_amf0().expect("build");
    for cut in 0..body.len() {
        // Every strict prefix either errors or (for prefixes that end
        // exactly on an event boundary) parses fewer events; it must
        // never panic.
        let _ = SharedObjectMessage::parse_amf0(&body[..cut]);
    }
    // Full body still parses.
    assert!(SharedObjectMessage::parse_amf0(&body).is_ok());
}

#[test]
fn oversize_declared_lengths_error() {
    // Name length overrunning the body.
    let bad_name = [0x00u8, 0xFF, b'a'];
    assert!(SharedObjectMessage::<Amf0Value>::parse_amf0(&bad_name).is_err());

    // Event data length overrunning the body.
    let mut body = Vec::new();
    body.extend_from_slice(&[0x00, 0x01]);
    body.push(b'n');
    body.extend_from_slice(&[0u8; 12]);
    body.push(SO_EVENT_CHANGE);
    body.extend_from_slice(&[0xFF, 0xFF, 0xFF, 0xFF]); // 4 GiB event
    body.push(0x00);
    assert!(SharedObjectMessage::<Amf0Value>::parse_amf0(&body).is_err());
}

#[test]
fn invalid_utf8_and_bad_amf_error() {
    // Non-UTF-8 SO name.
    let mut body = Vec::new();
    body.extend_from_slice(&[0x00, 0x02, 0xFF, 0xFE]);
    body.extend_from_slice(&[0u8; 12]);
    assert!(SharedObjectMessage::<Amf0Value>::parse_amf0(&body).is_err());

    // Change event whose value has an invalid AMF0 marker.
    let mut body = Vec::new();
    body.extend_from_slice(&[0x00, 0x01]);
    body.push(b'n');
    body.extend_from_slice(&[0u8; 12]);
    body.push(SO_EVENT_CHANGE);
    let ev = [0x00u8, 0x01, b'k', 0xEE]; // bare "k" + junk marker 0xEE
    body.extend_from_slice(&(ev.len() as u32).to_be_bytes());
    body.extend_from_slice(&ev);
    assert!(SharedObjectMessage::<Amf0Value>::parse_amf0(&body).is_err());
}

/// Deterministic byte-mutation sweep over a valid message: parsing
/// must never panic whatever single byte is corrupted.
#[test]
fn single_byte_mutation_never_panics() {
    let mut so = SharedObjectMessage::new("mutate");
    so.events = vec![
        SoEvent::RequestChange {
            name: "p".into(),
            value: Amf0Value::Number(1.0),
        },
        SoEvent::Status {
            code: "c".into(),
            level: "l".into(),
        },
    ];
    let body = so.build_amf0().expect("build");
    for i in 0..body.len() {
        for delta in [1u8, 0x7F, 0xFF] {
            let mut mutated = body.clone();
            mutated[i] = mutated[i].wrapping_add(delta);
            let _ = SharedObjectMessage::<Amf0Value>::parse_amf0(&mutated);
        }
    }
}

/// parse_shared_object rejects non-SO message types.
#[test]
fn parse_shared_object_rejects_wrong_type() {
    let msg = oxideav_rtmp::Message {
        msg_type_id: 18,
        msg_stream_id: 0,
        timestamp: 0,
        payload: vec![],
    };
    assert!(parse_shared_object(&msg).is_err());
}
