//! Enhanced RTMP v2 §"Reconnect Request" — end-to-end.
//!
//! The spec introduces a `NetConnection.Connect.ReconnectRequest`
//! status event "as part of the NetConnection onStatus command" so a
//! streaming platform can ask a publisher to reconnect — e.g. "when
//! live streaming servers undergo updates" or "when there's a need to
//! redirect the client to a different server instance, ensuring
//! optimal load balancing and precise geolocation mapping."
//!
//! This test exercises the full flow:
//!
//!   * Server side: [`RtmpSession::send_reconnect_request`] emits the
//!     NetConnection-level onStatus (message stream 0, transaction id
//!     0, null Command Object) with `code =
//!     NetConnection.Connect.ReconnectRequest`, `level = status`, and
//!     the optional `tcUrl` / `description` Info-Object properties.
//!   * Client side: [`RtmpClient::poll_event`] surfaces the event as
//!     the typed [`ClientEvent::ReconnectRequest`] — distinct from the
//!     generic `OnStatus` so an outer loop can drive the spec's
//!     message flow ("persists in streaming ... up to the next
//!     appropriate media boundary, such as a keyframe. Subsequently,
//!     it establishes a connection with a new server and disconnects
//!     from the old server").
//!   * URL resolution: [`RtmpClient::resolve_reconnect_url`] applies
//!     the spec's tcUrl rules — "if not specified, use the tcUrl for
//!     the current connection. A relative URI reference should be
//!     resolved relative to the tcUrl for the current connection."
//!
//! Per the spec, the old server "SHOULD continue processing messages
//! from the client until the client disconnects" — so the server
//! thread here keeps pumping `next_packet` after sending the request,
//! and the client keeps publishing for a beat before acting, proving
//! neither side tears the session down on the event itself.

use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use oxideav_rtmp::{ClientEvent, RtmpClient, RtmpServer, StreamPacket};

const APP: &str = "live";
const STREAM_KEY: &str = "reconnect-test";

/// Drive one publish session; server asks the client to reconnect to
/// an explicit relative target mid-stream.
#[test]
fn client_surfaces_reconnect_request_with_tc_url() {
    let server = RtmpServer::bind("127.0.0.1:0").expect("bind");
    let addr = server.local_addr().expect("local_addr");

    let (sent_tx, sent_rx) = mpsc::channel::<()>();

    let server_thread = thread::spawn(move || {
        let req = server.accept().expect("server accept");
        let mut session = req.accept().expect("session accept");
        // Ask the publisher to remap to another app on this host —
        // an absolute-path URI reference per the spec's Info Object
        // table example `/realtimeapp`.
        session
            .send_reconnect_request(Some("/backup-app"), Some("server undergoing updates"))
            .expect("send_reconnect_request");
        sent_tx.send(()).unwrap();
        // Spec: the old server "SHOULD continue processing messages
        // from the client until the client disconnects." Keep
        // draining; expect at least one post-request video frame, then
        // a clean publisher-initiated end (Ok(None)).
        let mut frames_after_request = 0u32;
        while let Some(pkt) = session.next_packet().expect("next_packet") {
            if let StreamPacket::Video { .. } = pkt {
                frames_after_request += 1;
            }
        }
        frames_after_request
    });

    thread::sleep(Duration::from_millis(50));

    let url = format!("rtmp://{}:{}/{APP}/{STREAM_KEY}", addr.ip(), addr.port());
    let mut client = RtmpClient::connect(&url).expect("client connect");
    sent_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("server signal");

    client
        .inner_mut()
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("set_read_timeout");

    // Pump events until the typed ReconnectRequest surfaces.
    let mut reconnect: Option<(Option<String>, String)> = None;
    for _ in 0..32 {
        match client.poll_event() {
            Ok(Some(ClientEvent::ReconnectRequest {
                tc_url,
                description,
            })) => {
                reconnect = Some((tc_url, description));
                break;
            }
            Ok(Some(ClientEvent::OnStatus { code, .. })) => {
                assert_ne!(
                    code, "NetConnection.Connect.ReconnectRequest",
                    "a valid reconnect request must surface as the typed variant, \
                     not the generic OnStatus"
                );
            }
            Ok(Some(_)) => continue,
            Ok(None) => panic!("stream ended before ReconnectRequest surfaced"),
            Err(e) => panic!("poll_event errored: {e}"),
        }
    }
    let (tc_url, description) = reconnect.expect("ReconnectRequest event");
    assert_eq!(tc_url.as_deref(), Some("/backup-app"));
    assert_eq!(description, "server undergoing updates");

    // Spec resolution: absolute-path reference inherits our scheme +
    // authority.
    let target = client.resolve_reconnect_url(tc_url.as_deref());
    assert_eq!(
        target,
        format!("rtmp://{}:{}/backup-app", addr.ip(), addr.port())
    );

    // Per spec the client "persists in streaming to/from the current
    // server up to the next appropriate media boundary" — prove the
    // session is still fully usable after the event by pushing one
    // more keyframe before disconnecting from the old server.
    client
        .send_video(40, true, &[0, 0, 0, 1, 0x65])
        .expect("post-request video frame");
    client.close().expect("client close");

    let frames_after_request = server_thread.join().expect("server thread");
    assert!(
        frames_after_request >= 1,
        "old server must keep processing publisher messages after \
         sending the reconnect request (got {frames_after_request})"
    );
}

/// tcUrl omitted: "if not specified, use the tcUrl for the current
/// connection."
#[test]
fn reconnect_request_without_tc_url_resolves_to_current_connection() {
    let server = RtmpServer::bind("127.0.0.1:0").expect("bind");
    let addr = server.local_addr().expect("local_addr");

    let server_thread = thread::spawn(move || {
        let req = server.accept().expect("server accept");
        let mut session = req.accept().expect("session accept");
        session
            .send_reconnect_request(None, None)
            .expect("send_reconnect_request");
        // Drain until the publisher disconnects.
        while session.next_packet().expect("next_packet").is_some() {}
    });

    thread::sleep(Duration::from_millis(50));

    let url = format!("rtmp://{}:{}/{APP}/{STREAM_KEY}", addr.ip(), addr.port());
    let mut client = RtmpClient::connect(&url).expect("client connect");
    client
        .inner_mut()
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("set_read_timeout");

    let mut saw = false;
    for _ in 0..32 {
        match client.poll_event() {
            Ok(Some(ClientEvent::ReconnectRequest {
                tc_url,
                description,
            })) => {
                assert_eq!(tc_url, None, "omitted tcUrl must surface as None");
                assert!(description.is_empty(), "no description was sent");
                // Resolution falls back to the connection's own tcUrl.
                assert_eq!(
                    client.resolve_reconnect_url(tc_url.as_deref()),
                    client.tc_url()
                );
                assert_eq!(
                    client.tc_url(),
                    format!("rtmp://{}:{}/{APP}", addr.ip(), addr.port())
                );
                saw = true;
                break;
            }
            Ok(Some(_)) => continue,
            Ok(None) => panic!("stream ended before ReconnectRequest surfaced"),
            Err(e) => panic!("poll_event errored: {e}"),
        }
    }
    assert!(saw, "client must surface ReconnectRequest");

    client.close().expect("client close");
    server_thread.join().expect("server thread");
}
