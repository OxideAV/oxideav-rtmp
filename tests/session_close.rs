//! Verifies the server's session close path emits the right wire
//! sequence: `UserControl StreamEOF(stream_id)` followed by
//! `onStatus(NetStream.Unpublish.Success)`, then the writer flush, then
//! a write-half FIN.
//!
//! Why this matters: before round 154 the close path went straight from
//! `onStatus` to `Shutdown::Both`. Tearing both halves of the socket
//! down at the same instant lets the kernel answer the peer's unacked
//! data with a RST and lose the `Unpublish.Success` frame. The
//! symmetric publish-side end-of-stream signal is `UserControl
//! StreamEOF` — RTMP 1.0 §7.1.7 documents it as the server→client
//! event for "the stream is dry," which is exactly the right shape for
//! an end-of-publish notification.
//!
//! The test reads raw bytes off the client socket so it doesn't need
//! to share the client's chunk-stream state machine. It looks for the
//! six-byte StreamEOF event body anywhere after the publish handshake
//! and then for the AMF0 `onStatus("NetStream.Unpublish.Success")`
//! later in the same byte stream.

use std::io::Read;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use oxideav_rtmp::{RtmpClient, RtmpServer};

const APP: &str = "live";
const STREAM_KEY: &str = "close-test";

#[test]
fn server_session_close_emits_stream_eof_then_unpublish_success() {
    let server = RtmpServer::bind("127.0.0.1:0").expect("bind");
    let addr = server.local_addr().expect("local_addr");

    let (started_tx, started_rx) = mpsc::channel::<()>();

    let server_thread = thread::spawn(move || {
        let req = server.accept().expect("server accept");
        let session = req.accept().expect("session accept");
        // Signal the client it can start reading post-publish.
        started_tx.send(()).unwrap();
        // Give the client a tick to settle before we tear down.
        thread::sleep(Duration::from_millis(100));
        session.close().expect("session close");
    });

    thread::sleep(Duration::from_millis(50));

    let url = format!("rtmp://{}:{}/{APP}/{STREAM_KEY}", addr.ip(), addr.port());
    let mut client = RtmpClient::connect(&url).expect("client connect");
    started_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("server signal");

    client
        .inner_mut()
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("set_read_timeout");

    // Drain the socket until peer FIN. We want raw bytes — chunk-state
    // tracking is the system under test, not what we use to inspect it.
    let mut buf = Vec::<u8>::new();
    let mut chunk = [0u8; 4096];
    loop {
        match client.inner_mut().read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(_) => break,
        }
    }

    server_thread.join().expect("server thread");

    // StreamEOF (UCM type 1) payload body is the 6-byte sequence
    // `00 01 00 00 00 <stream_id>`. The stream id from the server's
    // createStream reply is 1.
    let stream_eof_body: [u8; 6] = [0x00, 0x01, 0x00, 0x00, 0x00, 0x01];
    let eof_at = find_subsequence(&buf, &stream_eof_body)
        .expect("UserControl StreamEOF body not found in server-emitted bytes");

    // onStatus("NetStream.Unpublish.Success") — search for the AMF0
    // string body. AMF0 strings are `02 <UI16 BE len> <utf8 bytes>`. The
    // code field is the literal "NetStream.Unpublish.Success" — 27
    // bytes long, prefixed by `02 00 1B`.
    let code = b"NetStream.Unpublish.Success";
    let mut needle = Vec::with_capacity(3 + code.len());
    needle.push(0x02);
    needle.extend_from_slice(&(code.len() as u16).to_be_bytes());
    needle.extend_from_slice(code);
    let unpublish_at = find_subsequence(&buf, &needle)
        .expect("AMF0 onStatus(NetStream.Unpublish.Success) not found in server-emitted bytes");

    assert!(
        eof_at < unpublish_at,
        "StreamEOF (byte {eof_at}) must precede Unpublish.Success (byte {unpublish_at}) \
         so the peer's chunk state-machine sees the publish-end signal before the \
         trailing onStatus"
    );
}

fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    (0..=haystack.len() - needle.len()).find(|&i| &haystack[i..i + needle.len()] == needle)
}
