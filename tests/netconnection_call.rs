//! §7.2.1.2 NetConnection `call` — RPC interop over loopback in all
//! four directions:
//!
//! 1. publisher client → server (`RtmpClient::call` →
//!    `StreamPacket::Call`), answered with
//!    `RtmpSession::reply_call_result` → `ClientEvent::Result`;
//! 2. fire-and-forget (transaction id 0, no reply expected);
//! 3. server → publisher client (`RtmpSession::send_call` →
//!    `ClientEvent::Call`), answered with
//!    `RtmpClient::reply_call_result` → `StreamPacket::CallReply`;
//! 4. the wire round-trip of the `CallCommand` frame itself
//!    (`to_message` → `amf::decode_all` → `parse`);
//! 5. the play direction both ways (`PlaySession::send_call` →
//!    `PlayerPacket::Call` → `RtmpPlayer::reply_call_result` →
//!    `PlaySessionEvent::CallReply`, and `RtmpPlayer::call` →
//!    `PlaySessionEvent::Call` → `PlaySession::reply_call_result` →
//!    `PlayerPacket::CallReply`).
//!
//! Per spec: "The call method of the NetConnection object runs remote
//! procedure calls (RPC) at the receiving end. The called RPC name is
//! passed as a parameter to the call command" — i.e. the wire
//! command-name field carries the procedure name, and "If a response
//! is expected we give a transaction Id. Else we pass a value of 0."

use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use oxideav_rtmp::{Amf0Value, CallCommand, ClientEvent, RtmpClient, RtmpServer, StreamPacket};

const APP: &str = "live";
const KEY: &str = "call-key";

fn connect_pair() -> (RtmpServer, String) {
    let server = RtmpServer::bind("127.0.0.1:0").expect("bind");
    let addr = server.local_addr().expect("local_addr");
    let url = format!("rtmp://{}:{}/{APP}/{KEY}", addr.ip(), addr.port());
    (server, url)
}

#[test]
fn client_rpc_reaches_server_and_result_returns() {
    let (server, url) = connect_pair();
    let (call_tx, call_rx) = mpsc::channel::<CallCommand>();

    let server_thread = thread::spawn(move || {
        let req = server.accept().expect("accept");
        let mut session = req.accept().expect("session");
        session
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("timeout");
        loop {
            match session.next_packet() {
                Ok(Some(StreamPacket::Call(call))) => {
                    assert!(call.expects_response());
                    // Echo the arguments back as the response object.
                    let response = Amf0Value::Object(vec![
                        ("echo".into(), call.arguments[0].clone()),
                        ("proc".into(), Amf0Value::String(call.procedure.clone())),
                    ]);
                    session
                        .reply_call_result(call.transaction_id, Amf0Value::Null, response)
                        .expect("reply");
                    call_tx.send(call).unwrap();
                }
                Ok(Some(_)) => {}
                Ok(None) | Err(_) => break,
            }
        }
    });

    thread::sleep(Duration::from_millis(50));
    let mut client = RtmpClient::connect(&url).expect("connect");
    let tx_id = client
        .call(
            "checkBandwidth",
            Amf0Value::Null,
            &[Amf0Value::Number(42.0)],
            true,
        )
        .expect("call");
    assert!(tx_id != 0.0, "response-expecting call needs non-zero tx");

    // Pump events until the matching _result arrives.
    client
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("timeout");
    let values = loop {
        match client.poll_event().expect("poll") {
            Some(ClientEvent::Result {
                transaction_id,
                values,
            }) if transaction_id == tx_id => break values,
            _ => {}
        }
    };
    // [name, tx, command_object, response]
    let response = values.get(3).expect("response slot");
    assert_eq!(response.get("echo").and_then(Amf0Value::as_f64), Some(42.0));
    assert_eq!(
        response.get("proc").and_then(Amf0Value::as_str),
        Some("checkBandwidth")
    );

    client.close().expect("close");
    server_thread.join().expect("join");

    let seen = call_rx.recv_timeout(Duration::from_secs(1)).expect("call");
    assert_eq!(seen.procedure, "checkBandwidth");
    assert_eq!(seen.transaction_id, tx_id);
    assert_eq!(seen.command_object, Amf0Value::Null);
    assert_eq!(seen.arguments, vec![Amf0Value::Number(42.0)]);
}

#[test]
fn fire_and_forget_call_uses_transaction_zero() {
    let (server, url) = connect_pair();
    let (call_tx, call_rx) = mpsc::channel::<CallCommand>();

    let server_thread = thread::spawn(move || {
        let req = server.accept().expect("accept");
        let mut session = req.accept().expect("session");
        session
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("timeout");
        loop {
            match session.next_packet() {
                Ok(Some(StreamPacket::Call(call))) => call_tx.send(call).unwrap(),
                Ok(Some(_)) => {}
                Ok(None) | Err(_) => break,
            }
        }
    });

    thread::sleep(Duration::from_millis(50));
    let mut client = RtmpClient::connect(&url).expect("connect");
    let tx_id = client
        .call("logEvent", Amf0Value::Null, &[], false)
        .expect("call");
    assert_eq!(tx_id, 0.0, "§7.2.1.2: no response expected → tx 0");
    thread::sleep(Duration::from_millis(100));
    client.close().expect("close");
    server_thread.join().expect("join");

    let seen = call_rx.recv_timeout(Duration::from_secs(1)).expect("call");
    assert_eq!(seen.procedure, "logEvent");
    assert!(!seen.expects_response());
    assert!(seen.arguments.is_empty());
}

#[test]
fn server_rpc_reaches_client_and_reply_surfaces() {
    let (server, url) = connect_pair();
    let (reply_tx, reply_rx) = mpsc::channel::<(bool, f64, Vec<Amf0Value>)>();

    let server_thread = thread::spawn(move || {
        let req = server.accept().expect("accept");
        let mut session = req.accept().expect("session");
        session
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("timeout");
        // Server-initiated RPC at the client (the classic
        // bandwidth-check shape).
        session
            .send_call(&CallCommand {
                procedure: "onBWCheck".into(),
                transaction_id: 77.0,
                command_object: Amf0Value::Null,
                arguments: vec![Amf0Value::Number(0.0)],
            })
            .expect("send_call");
        loop {
            match session.next_packet() {
                Ok(Some(StreamPacket::CallReply {
                    success,
                    transaction_id,
                    values,
                })) => {
                    reply_tx.send((success, transaction_id, values)).unwrap();
                }
                Ok(Some(_)) => {}
                Ok(None) | Err(_) => break,
            }
        }
    });

    thread::sleep(Duration::from_millis(50));
    let mut client = RtmpClient::connect(&url).expect("connect");
    client
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("timeout");
    // Pump until the server's RPC arrives, then answer it.
    loop {
        if let Some(ClientEvent::Call(call)) = client.poll_event().expect("poll") {
            assert_eq!(call.procedure, "onBWCheck");
            assert_eq!(call.transaction_id, 77.0);
            assert!(call.expects_response());
            client
                .reply_call_result(
                    call.transaction_id,
                    Amf0Value::Null,
                    Amf0Value::Number(1234.5),
                )
                .expect("reply");
            break;
        }
    }
    thread::sleep(Duration::from_millis(100));
    client.close().expect("close");
    server_thread.join().expect("join");

    let (success, tx_id, values) = reply_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("reply");
    assert!(success);
    assert_eq!(tx_id, 77.0);
    assert_eq!(values.get(3).and_then(Amf0Value::as_f64), Some(1234.5));
}

#[test]
fn call_command_round_trips_through_message() {
    let call = CallCommand {
        procedure: "myProc".into(),
        transaction_id: 9.0,
        command_object: Amf0Value::Object(vec![("k".into(), Amf0Value::String("v".into()))]),
        arguments: vec![Amf0Value::Number(1.0), Amf0Value::Boolean(true)],
    };
    let msg = call.to_message();
    assert_eq!(msg.msg_stream_id, 0, "NetConnection commands ride stream 0");
    let values = oxideav_rtmp::amf::decode_all(&msg.payload).expect("decode");
    let parsed = CallCommand::parse(&values).expect("parse");
    assert_eq!(parsed, call);
}

#[test]
fn play_direction_rpc_both_ways() {
    // Server → subscriber RPC (PlaySession::send_call →
    // PlayerPacket::Call → RtmpPlayer::reply_call_result →
    // PlaySessionEvent::CallReply), then subscriber → server RPC
    // (RtmpPlayer::call → PlaySessionEvent::Call →
    // PlaySession::reply_call_result → PlayerPacket::CallReply).
    use oxideav_rtmp::{PlaySessionEvent, PlayerPacket, RtmpPlayer, SessionRequest};

    let server = RtmpServer::bind("127.0.0.1:0").expect("bind");
    let addr = server.local_addr().expect("local_addr");
    let url = format!("rtmp://{}:{}/vod/clip", addr.ip(), addr.port());

    let (reply_tx, reply_rx) = mpsc::channel::<(bool, f64, Vec<Amf0Value>)>();

    let server_thread = thread::spawn(move || {
        let req = match server.accept_any().expect("accept_any") {
            SessionRequest::Play(req) => req,
            SessionRequest::Publish(_) => panic!("expected play"),
        };
        let mut session = req.accept().expect("accept");
        session
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("timeout");
        // RPC at the subscriber.
        session
            .send_call(&CallCommand {
                procedure: "onBWCheck".into(),
                transaction_id: 31.0,
                command_object: Amf0Value::Null,
                arguments: vec![],
            })
            .expect("send_call");
        // Pump subscriber events: expect our CallReply, then their
        // RPC, then teardown.
        loop {
            match session.next_event() {
                Ok(Some(PlaySessionEvent::CallReply {
                    success,
                    transaction_id,
                    values,
                })) => reply_tx.send((success, transaction_id, values)).unwrap(),
                Ok(Some(PlaySessionEvent::Call(call))) => {
                    assert_eq!(call.procedure, "getQuality");
                    assert!(call.expects_response());
                    session
                        .reply_call_result(
                            call.transaction_id,
                            Amf0Value::Null,
                            Amf0Value::String("hd".into()),
                        )
                        .expect("reply");
                }
                Ok(Some(_)) => {}
                Ok(None) | Err(_) => break,
            }
        }
    });

    thread::sleep(Duration::from_millis(50));
    let mut player = RtmpPlayer::connect(&url).expect("player connect");
    player
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("timeout");

    // Answer the server's RPC when it surfaces.
    loop {
        if let Some(PlayerPacket::Call(call)) = player.next_packet().expect("next_packet") {
            assert_eq!(call.procedure, "onBWCheck");
            assert_eq!(call.transaction_id, 31.0);
            player
                .reply_call_result(call.transaction_id, Amf0Value::Null, Amf0Value::Number(9.0))
                .expect("reply");
            break;
        }
    }

    // Now issue our own RPC and wait for the matching reply.
    let tx_id = player
        .call("getQuality", Amf0Value::Null, &[], true)
        .expect("call");
    assert!(tx_id != 0.0);
    let values = loop {
        match player.next_packet().expect("next_packet") {
            Some(PlayerPacket::CallReply {
                success,
                transaction_id,
                values,
            }) if transaction_id == tx_id => {
                assert!(success);
                break values;
            }
            _ => {}
        }
    };
    assert_eq!(values.get(3).and_then(Amf0Value::as_str), Some("hd"));

    player.close().expect("close");
    server_thread.join().expect("join");

    let (success, tx, values) = reply_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("server-side reply");
    assert!(success);
    assert_eq!(tx, 31.0);
    assert_eq!(values.get(3).and_then(Amf0Value::as_f64), Some(9.0));
}
