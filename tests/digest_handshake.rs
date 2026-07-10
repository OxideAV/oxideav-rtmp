//! Digest (HMAC-SHA256) handshake conformance over in-process
//! loopback pairs — the byte-level scheme is specified in
//! docs/streaming/rtmp/rtmp-so-dataframe-digest-handshake.md §3.
//!
//! Covers the raw handshake functions over a real socket pair (both
//! digest schemas, plus the simple-client / digest-server and
//! digest-client / simple-server fallbacks) and the wired-up paths:
//! `RtmpClient::connect_with_digest_handshake` and
//! `PlayOptions::digest_handshake` against `RtmpServer`, which
//! auto-negotiates.

use std::net::{TcpListener, TcpStream};
use std::thread;
use std::time::Duration;

use oxideav_rtmp::handshake::{
    client_handshake, client_handshake_digest, server_handshake_negotiated,
};
use oxideav_rtmp::{
    ConnectCapabilities, DigestScheme, HandshakeKind, PlayOptions, RtmpClient, RtmpPlayer,
    RtmpServer, SessionRequest,
};

/// Run one raw handshake pair over loopback TCP, returning
/// (client kind, server kind).
fn handshake_pair(
    client: impl FnOnce(&mut TcpStream) -> oxideav_rtmp::Result<HandshakeKind> + Send + 'static,
) -> (HandshakeKind, HandshakeKind) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("local_addr");

    let server_thread = thread::spawn(move || {
        let (mut sock, _) = listener.accept().expect("accept");
        sock.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        server_handshake_negotiated(&mut sock).expect("server handshake")
    });

    let client_thread = thread::spawn(move || {
        let mut sock = TcpStream::connect(addr).expect("connect");
        sock.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        client(&mut sock).expect("client handshake")
    });

    let server_kind = server_thread.join().expect("server thread");
    let client_kind = client_thread.join().expect("client thread");
    (client_kind, server_kind)
}

#[test]
fn digest_exchange_schema1_verifies_both_sides() {
    let (ck, sk) = handshake_pair(|s| client_handshake_digest(s, DigestScheme::Schema1));
    assert_eq!(
        ck,
        HandshakeKind::Digest {
            scheme: DigestScheme::Schema1,
            peer_response_verified: true
        },
        "client must see a digested, chain-verified S1/S2"
    );
    assert_eq!(
        sk,
        HandshakeKind::Digest {
            scheme: DigestScheme::Schema1,
            peer_response_verified: true
        },
        "server must verify the client's chained C2"
    );
}

#[test]
fn digest_exchange_schema0_verifies_both_sides() {
    let (ck, sk) = handshake_pair(|s| client_handshake_digest(s, DigestScheme::Schema0));
    assert_eq!(
        ck,
        HandshakeKind::Digest {
            scheme: DigestScheme::Schema0,
            peer_response_verified: true
        }
    );
    assert_eq!(
        sk,
        HandshakeKind::Digest {
            scheme: DigestScheme::Schema0,
            peer_response_verified: true
        }
    );
}

#[test]
fn simple_client_against_negotiated_server_stays_simple() {
    let (ck, sk) = handshake_pair(|s| client_handshake(s).map(|()| HandshakeKind::Simple));
    assert_eq!(ck, HandshakeKind::Simple);
    assert_eq!(sk, HandshakeKind::Simple);
}

/// A digest client dialing a *plain echo* server must degrade to the
/// simple exchange without erroring.
#[test]
fn digest_client_against_echo_server_falls_back() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("local_addr");

    // Hand-rolled echo server: S0 + zero-version random S1 + S2 = echo
    // of C1, then drain C2.
    let server_thread = thread::spawn(move || {
        use std::io::{Read, Write};
        let (mut sock, _) = listener.accept().expect("accept");
        sock.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut c0c1 = [0u8; 1537];
        sock.read_exact(&mut c0c1).expect("read C0C1");
        assert_eq!(c0c1[0], 0x03);
        let mut s1 = [0u8; 1536];
        // Deterministic junk random body, version field left zero.
        for (i, b) in s1.iter_mut().enumerate().skip(8) {
            *b = (i * 31 + 7) as u8;
        }
        sock.write_all(&[0x03]).unwrap();
        sock.write_all(&s1).unwrap();
        sock.write_all(&c0c1[1..]).unwrap(); // S2 = echo of C1
        let mut c2 = [0u8; 1536];
        sock.read_exact(&mut c2).expect("read C2");
        // Simple-fallback client echoes S1.
        assert_eq!(c2, s1);
    });

    let mut sock = TcpStream::connect(addr).expect("connect");
    sock.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let kind = client_handshake_digest(&mut sock, DigestScheme::Schema1).expect("handshake");
    assert_eq!(kind, HandshakeKind::Simple);
    server_thread.join().expect("server thread");
}

/// Full publish setup through `RtmpClient::connect_with_digest_handshake`
/// against our auto-negotiating server: the connection must complete
/// the whole connect/createStream/publish flow after a digest
/// handshake, and the client must report a verified digest exchange.
#[test]
fn client_digest_publish_full_flow() {
    let server = RtmpServer::bind("127.0.0.1:0").expect("bind");
    let addr = server.local_addr().expect("local_addr");

    let server_thread = thread::spawn(move || {
        let req = server.accept().expect("accept");
        assert_eq!(req.stream_name, "digest-key");
        let mut session = req.accept().expect("session");
        session
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        // Drain until the client closes.
        while let Ok(Some(_)) = session.next_packet() {}
    });

    thread::sleep(Duration::from_millis(50));
    let url = format!("rtmp://{}:{}/live/digest-key", addr.ip(), addr.port());
    let mut client =
        RtmpClient::connect_with_digest_handshake(&url, "live", &ConnectCapabilities::default())
            .expect("digest connect");
    assert_eq!(
        client.handshake_kind(),
        HandshakeKind::Digest {
            scheme: DigestScheme::Schema1,
            peer_response_verified: true
        }
    );
    client.send_audio(0, &[0x11u8; 16]).expect("send audio");
    thread::sleep(Duration::from_millis(50));
    client.close().expect("close");
    server_thread.join().expect("server thread");
}

/// Play flow with `PlayOptions::digest_handshake` against the
/// auto-negotiating server, exercised through `accept_any`.
#[test]
fn player_digest_play_full_flow() {
    let server = RtmpServer::bind("127.0.0.1:0").expect("bind");
    let addr = server.local_addr().expect("local_addr");

    let server_thread = thread::spawn(move || match server.accept_any().expect("accept_any") {
        SessionRequest::Play(req) => {
            assert_eq!(req.stream_name, "digest-play");
            let mut session = req.accept().expect("play session");
            session
                .send_metadata(&oxideav_rtmp::amf::obj([(
                    "width",
                    oxideav_rtmp::Amf0Value::Number(320.0),
                )]))
                .expect("send metadata");
            session.close().expect("close play session");
        }
        SessionRequest::Publish(_) => panic!("expected a play request"),
    });

    thread::sleep(Duration::from_millis(50));
    let url = format!("rtmp://{}:{}/live/digest-play", addr.ip(), addr.port());
    let opts = PlayOptions {
        digest_handshake: true,
        ..PlayOptions::default()
    };
    let mut player = RtmpPlayer::connect_with_options(&url, &opts).expect("digest play connect");
    assert_eq!(
        player.handshake_kind(),
        HandshakeKind::Digest {
            scheme: DigestScheme::Schema1,
            peer_response_verified: true
        }
    );
    // Pump until the server closes; we must at least see the metadata.
    let mut saw_metadata = false;
    while let Ok(Some(pkt)) = player.next_packet() {
        if let oxideav_rtmp::PlayerPacket::Metadata(_) = pkt {
            saw_metadata = true;
        }
    }
    assert!(saw_metadata, "metadata must arrive over the digest session");
    server_thread.join().expect("server thread");
}
