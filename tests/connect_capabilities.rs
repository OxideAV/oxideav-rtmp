//! Enhanced RTMP v2 NetConnection `connect` capability negotiation —
//! end-to-end loopback (`enhanced-rtmp-v2.pdf` §"Enhancing
//! NetConnection connect Command").
//!
//! The client advertises a populated [`ConnectCapabilities`] block in
//! its `connect` command; the server lifts that off the Command Object
//! and surfaces it on [`PublishRequest::capabilities`]; the server
//! advertises its OWN capability block in the `_result(connect)` info
//! object; the client lifts that off and surfaces it via
//! [`RtmpClient::server_capabilities`]. Asserts both directions of the
//! handshake on a real TCP loopback so the framing — AMF0 encode /
//! chunk framing / `_result` info-object placement — is exercised
//! exactly like a wire publish.

use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::Duration;

use oxideav_rtmp::{
    ConnectCapabilities, RtmpClient, RtmpServer, RtmpSession, CAPS_EX_MOD_EX, CAPS_EX_MULTITRACK,
    CAPS_EX_RECONNECT, CAPS_EX_TIMESTAMP_NANO_OFFSET, FOURCC_INFO_CAN_DECODE,
    FOURCC_INFO_CAN_ENCODE, FOURCC_INFO_CAN_FORWARD,
};

/// Drive the session until the client closes (returns None) or a short
/// idle window elapses. We don't expect any media — the test just needs
/// to keep the socket alive long enough for the client to walk the full
/// publish handshake and drain the server's `_result(connect)` info
/// object.
fn drain_session(mut session: RtmpSession) {
    let _ = session.set_read_timeout(Some(Duration::from_millis(500)));
    while let Ok(Some(_)) = session.next_packet() {}
}

const APP: &str = "live";
const STREAM_KEY: &str = "caps-test";

/// Build the capability block a v2-aware publisher would advertise.
fn publisher_caps() -> ConnectCapabilities {
    use oxideav_rtmp::FourCcInfoMap;
    let mut video = FourCcInfoMap::new();
    video.insert("*", FOURCC_INFO_CAN_FORWARD);
    video.insert("hvc1", FOURCC_INFO_CAN_DECODE | FOURCC_INFO_CAN_ENCODE);
    let mut audio = FourCcInfoMap::new();
    audio.insert("*", FOURCC_INFO_CAN_FORWARD);
    audio.insert("Opus", FOURCC_INFO_CAN_DECODE | FOURCC_INFO_CAN_ENCODE);
    ConnectCapabilities {
        fourcc_list: vec![
            "av01".into(),
            "vp09".into(),
            "hvc1".into(),
            "avc1".into(),
            "Opus".into(),
            "mp4a".into(),
        ],
        video_fourcc_info_map: video,
        audio_fourcc_info_map: audio,
        caps_ex: CAPS_EX_RECONNECT | CAPS_EX_MULTITRACK | CAPS_EX_TIMESTAMP_NANO_OFFSET,
        ..Default::default()
    }
}

/// Build the capability block a v2-aware ingest server would echo back.
fn server_caps() -> ConnectCapabilities {
    use oxideav_rtmp::FourCcInfoMap;
    let mut video = FourCcInfoMap::new();
    video.insert("hvc1", FOURCC_INFO_CAN_DECODE);
    video.insert("av01", FOURCC_INFO_CAN_DECODE);
    let mut audio = FourCcInfoMap::new();
    audio.insert("Opus", FOURCC_INFO_CAN_DECODE);
    audio.insert("mp4a", FOURCC_INFO_CAN_DECODE);
    ConnectCapabilities {
        video_fourcc_info_map: video,
        audio_fourcc_info_map: audio,
        caps_ex: CAPS_EX_RECONNECT
            | CAPS_EX_MULTITRACK
            | CAPS_EX_MOD_EX
            | CAPS_EX_TIMESTAMP_NANO_OFFSET,
        ..Default::default()
    }
}

/// The legacy `_result(connect)` info object always carries
/// `objectEncoding = 0` per the pre-2023 spec, so the client sees that
/// stamped onto whatever the server-side `set_capabilities` block adds.
/// Build the "as observed by the client" expected value for our
/// [`server_caps`] block.
fn server_caps_as_observed_by_client() -> ConnectCapabilities {
    ConnectCapabilities {
        object_encoding: Some(0),
        ..server_caps()
    }
}

/// Both sides advertise non-default capability blocks; each one
/// observes the other through the public accessors.
#[test]
fn loopback_round_trips_full_capability_block() {
    // Server with its own capability advertisement.
    let mut server = RtmpServer::bind("127.0.0.1:0").expect("bind");
    server.set_capabilities(server_caps());
    let addr = server.local_addr().expect("local_addr");

    let (client_caps_tx, client_caps_rx) = mpsc::channel::<ConnectCapabilities>();
    let server_thread = thread::spawn(move || {
        let req = server.accept().expect("server accept");
        client_caps_tx.send(req.capabilities.clone()).unwrap();
        // Accept the publish so the client's `wait_for_publish_start`
        // sees an onStatus, then drain until the client closes its
        // write half on Drop.
        let session = req.accept().expect("session accept");
        drain_session(session);
    });

    thread::sleep(Duration::from_millis(50));

    let url = format!("rtmp://{}:{}/{APP}/{STREAM_KEY}", addr.ip(), addr.port());
    let client_caps = publisher_caps();
    let client =
        RtmpClient::connect_with_capabilities(&url, "live", &client_caps).expect("client connect");

    // Server-observed client caps must equal what we sent.
    let observed_client_caps = client_caps_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    assert_eq!(observed_client_caps, client_caps);

    // Client-observed server caps must equal what the server set.
    assert_eq!(
        client.server_capabilities(),
        &server_caps_as_observed_by_client()
    );

    drop(client);
    let _ = server_thread.join();
}

/// A legacy (default-empty) client against a v2-aware server: the
/// publisher's `capabilities` field is empty (it sent the pre-2023 byte
/// shape) but the client still receives the server's advertisement.
#[test]
fn legacy_client_still_receives_server_capabilities() {
    let mut server = RtmpServer::bind("127.0.0.1:0").expect("bind");
    server.set_capabilities(server_caps());
    let addr = server.local_addr().expect("local_addr");

    let server_caps_observed = Arc::new(Mutex::new(None::<ConnectCapabilities>));
    let observed = server_caps_observed.clone();
    let server_thread = thread::spawn(move || {
        let req = server.accept().expect("server accept");
        *observed.lock().unwrap() = Some(req.capabilities.clone());
        let session = req.accept().expect("session accept");
        drain_session(session);
    });

    thread::sleep(Duration::from_millis(50));

    let url = format!("rtmp://{}:{}/{APP}/{STREAM_KEY}", addr.ip(), addr.port());
    let client = RtmpClient::connect(&url).expect("legacy client connect");

    // Server's view of the client's caps must be empty.
    let _ = server_thread.join();
    let observed = server_caps_observed.lock().unwrap();
    let observed = observed.as_ref().expect("server caps populated");
    assert!(observed.is_empty(), "legacy client must advertise nothing");

    // Server's caps still surface on the client.
    assert_eq!(
        client.server_capabilities(),
        &server_caps_as_observed_by_client()
    );
}

/// A v2-aware client against a legacy server: the server's advertised
/// caps stay empty, but the server still observes everything the
/// client advertised.
#[test]
fn v2_client_against_legacy_server_observes_empty() {
    let server = RtmpServer::bind("127.0.0.1:0").expect("bind");
    let addr = server.local_addr().expect("local_addr");

    let (client_caps_tx, client_caps_rx) = mpsc::channel::<ConnectCapabilities>();
    let server_thread = thread::spawn(move || {
        let req = server.accept().expect("server accept");
        client_caps_tx.send(req.capabilities.clone()).unwrap();
        let session = req.accept().expect("session accept");
        drain_session(session);
    });

    thread::sleep(Duration::from_millis(50));

    let url = format!("rtmp://{}:{}/{APP}/{STREAM_KEY}", addr.ip(), addr.port());
    let client_caps = publisher_caps();
    let client = RtmpClient::connect_with_capabilities(&url, "live", &client_caps)
        .expect("v2 client connect");

    let observed = client_caps_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    assert_eq!(observed, client_caps);
    assert!(
        client.server_capabilities().is_empty(),
        "legacy server must advertise nothing",
    );

    drop(client);
    let _ = server_thread.join();
}

/// `capsEx` bit-test surfaces the documented Reconnect / Multitrack /
/// ModEx / TimestampNanoOffset features after a full loopback.
#[test]
fn caps_ex_bits_are_observable_after_loopback() {
    let mut server = RtmpServer::bind("127.0.0.1:0").expect("bind");
    server.set_capabilities(server_caps());
    let addr = server.local_addr().expect("local_addr");

    let server_thread = thread::spawn(move || {
        let req = server.accept().expect("server accept");
        let session = req.accept().expect("session accept");
        drain_session(session);
    });

    thread::sleep(Duration::from_millis(50));
    let url = format!("rtmp://{}:{}/{APP}/{STREAM_KEY}", addr.ip(), addr.port());
    let client = RtmpClient::connect_with_capabilities(&url, "live", &publisher_caps())
        .expect("v2 client connect");

    let observed_server = client.server_capabilities();
    // The server advertises all four bits — every individual `supports`
    // probe must return true.
    assert!(observed_server.supports_caps_ex(CAPS_EX_RECONNECT));
    assert!(observed_server.supports_caps_ex(CAPS_EX_MULTITRACK));
    assert!(observed_server.supports_caps_ex(CAPS_EX_MOD_EX));
    assert!(observed_server.supports_caps_ex(CAPS_EX_TIMESTAMP_NANO_OFFSET));

    drop(client);
    let _ = server_thread.join();
}
