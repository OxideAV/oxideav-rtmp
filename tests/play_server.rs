//! Server-side play (subscribe) direction — RTMP 1.0
//! Commands-Messages §4.2.1 Figure 5.
//!
//! A raw subscriber built from the crate's public low-level modules
//! (`handshake` / `chunk` / `message` / `amf`) dials the real
//! [`RtmpServer`], negotiates connect → createStream → play, and
//! asserts:
//!
//! * `accept_any` classifies the connection as [`SessionRequest::Play`]
//!   with the §4.2.1 argument table (stream name, Start, Duration,
//!   Reset) and the §3.7 pre-play `SetBufferLength` lifted onto
//!   [`PlayRequest`];
//! * [`PlayRequest::accept`] emits the Figure 5 sequence in order —
//!   `UserControl StreamBegin`, `onStatus(NetStream.Play.Reset)` (only
//!   because the play command set the reset flag), then
//!   `onStatus(NetStream.Play.Start)`;
//! * A/V tags + `onMetaData` pushed through [`PlaySession`] arrive as
//!   correctly-framed messages on the play stream id;
//! * a subscriber `pause` surfaces as a typed
//!   [`PlaySessionEvent::Command`] and the §4.2.8
//!   `NetStream.Pause.Notify` reply reaches the wire;
//! * `closeStream` ends the event loop (`Ok(None)`) per §4.2.3;
//! * the publish-only [`RtmpServer::accept`] refuses a play connection
//!   with `onStatus(NetStream.Play.StreamNotFound)`.

use std::io::Read;
use std::net::TcpStream;
use std::thread;
use std::time::Duration;

use oxideav_rtmp::chunk::{ChunkReader, ChunkWriter, Message};
use oxideav_rtmp::flv::{
    self, AudioTag, VideoTag, AAC_PACKET_TYPE_RAW, AUDIO_FORMAT_AAC, AVC_PACKET_TYPE_NALU,
    VIDEO_CODEC_AVC, VIDEO_FRAME_KEYFRAME,
};
use oxideav_rtmp::handshake::client_handshake;
use oxideav_rtmp::message::{
    build_connect, build_create_stream, build_user_control_set_buffer_length, NetStreamCommand,
    MSG_AUDIO, MSG_COMMAND_AMF0, MSG_DATA_AMF0, MSG_SET_CHUNK_SIZE, MSG_USER_CONTROL, MSG_VIDEO,
    STATUS_PAUSE_NOTIFY, STATUS_PLAY_RESET, STATUS_PLAY_START, STATUS_PLAY_STREAM_NOT_FOUND,
    USR_STREAM_BEGIN,
};
use oxideav_rtmp::{amf, Amf0Value, PlaySessionEvent, RtmpServer, SessionRequest};

const APP: &str = "vod";
const STREAM: &str = "mp4:sample.m4v";

const CSID_CONTROL: u32 = 2;
const CSID_COMMAND: u32 = 3;

/// A raw subscriber connection after connect + createStream: returns
/// `(tcp, reader, writer, stream_id)`.
fn dial_subscriber(
    addr: std::net::SocketAddr,
) -> (
    TcpStream,
    ChunkReader<TcpStream>,
    ChunkWriter<TcpStream>,
    u32,
) {
    let tcp = TcpStream::connect(addr).expect("connect");
    tcp.set_read_timeout(Some(Duration::from_secs(5)))
        .expect("set_read_timeout");
    let mut hs = tcp.try_clone().expect("clone hs");
    client_handshake(&mut hs).expect("client handshake");

    let mut writer = ChunkWriter::new(tcp.try_clone().expect("clone w"));
    let mut reader = ChunkReader::new(tcp.try_clone().expect("clone r"));

    let tc_url = format!("rtmp://{addr}/{APP}");
    writer
        .write_message(CSID_COMMAND, &build_connect(1.0, APP, &tc_url, "test"))
        .expect("connect");
    writer.flush().expect("flush connect");
    drain_until_command(&mut reader, "_result");

    writer
        .write_message(CSID_COMMAND, &build_create_stream(2.0))
        .expect("createStream");
    writer.flush().expect("flush createStream");
    let stream_id = drain_until_create_stream_result(&mut reader);
    (tcp, reader, writer, stream_id)
}

/// Read one message, applying inbound SetChunkSize to the reader so
/// larger server frames keep parsing.
fn read_next<R: Read>(reader: &mut ChunkReader<R>) -> Message {
    loop {
        let msg = reader.read_message().expect("read message");
        if msg.msg_type_id == MSG_SET_CHUNK_SIZE && msg.payload.len() >= 4 {
            let size = u32::from_be_bytes([
                msg.payload[0],
                msg.payload[1],
                msg.payload[2],
                msg.payload[3],
            ]) & 0x7FFF_FFFF;
            reader.set_chunk_size(size as usize);
            continue;
        }
        return msg;
    }
}

fn drain_until_command<R: Read>(reader: &mut ChunkReader<R>, want: &str) -> Vec<Amf0Value> {
    for _ in 0..50 {
        let msg = read_next(reader);
        if msg.msg_type_id == MSG_COMMAND_AMF0 {
            let values = amf::decode_all(&msg.payload).unwrap_or_default();
            let name = values.first().and_then(Amf0Value::as_str).unwrap_or("");
            if name == want {
                return values;
            }
        }
    }
    panic!("never saw command `{want}`");
}

fn drain_until_create_stream_result<R: Read>(reader: &mut ChunkReader<R>) -> u32 {
    for _ in 0..50 {
        let msg = read_next(reader);
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

fn on_status_code(values: &[Amf0Value]) -> String {
    values
        .get(3)
        .and_then(|info| info.get("code"))
        .and_then(Amf0Value::as_str)
        .unwrap_or("")
        .to_owned()
}

/// Full §4.2.1 Figure 5 walk: play with Start/Duration/Reset + a §3.7
/// SetBufferLength ahead of it, the ordered acceptance sequence, media
/// + metadata delivery, pause round-trip, closeStream teardown.
#[test]
fn play_session_serves_figure5_flow() {
    let server = RtmpServer::bind("127.0.0.1:0").expect("bind");
    let addr = server.local_addr().expect("local_addr");

    let server_thread = thread::spawn(move || {
        let req = match server.accept_any().expect("accept_any") {
            SessionRequest::Play(req) => req,
            SessionRequest::Publish(_) => panic!("expected a play request"),
        };
        assert_eq!(req.app, APP);
        assert_eq!(req.stream_name, STREAM);
        assert_eq!(req.start, Some(0.0));
        assert_eq!(req.duration, Some(-1.0));
        assert_eq!(req.reset, Some(true));
        assert_eq!(req.buffer_length_ms, Some(3000));

        let mut session = req.accept().expect("accept play");
        assert_eq!(session.app(), APP);
        assert_eq!(session.stream_name(), STREAM);
        session
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("set_read_timeout");

        // Serve metadata + one video keyframe + one audio frame.
        let meta = Amf0Value::EcmaArray(vec![
            ("width".into(), Amf0Value::Number(320.0)),
            ("height".into(), Amf0Value::Number(180.0)),
        ]);
        session.send_metadata(&meta).expect("send metadata");
        let vtag = VideoTag {
            mod_ex: Vec::new(),
            frame_type: VIDEO_FRAME_KEYFRAME,
            codec_id: VIDEO_CODEC_AVC,
            avc_packet_type: Some(AVC_PACKET_TYPE_NALU),
            composition_time: 0,
            body: vec![0, 0, 0, 2, 0x65, 0x88],
            ex_packet_type: None,
            fourcc: None,
            multitrack: None,
        };
        session.send_video(40, &vtag).expect("send video");
        let atag = AudioTag {
            mod_ex: Vec::new(),
            sound_format: AUDIO_FORMAT_AAC,
            sound_rate: 3,
            sound_size_16bit: true,
            stereo: true,
            aac_packet_type: Some(AAC_PACKET_TYPE_RAW),
            body: vec![0x21, 0x42, 0x63],
            ex_packet_type: None,
            audio_fourcc: None,
            multitrack: None,
        };
        session.send_audio(63, &atag).expect("send audio");

        // The subscriber pauses: surface the typed command, then reply
        // with NetStream.Pause.Notify per §4.2.8.
        match session.next_event().expect("next_event") {
            Some(PlaySessionEvent::Command(NetStreamCommand::Pause {
                pause,
                milliseconds,
            })) => {
                assert!(pause);
                assert_eq!(milliseconds, 63.0);
            }
            other => panic!("expected Pause command, got {other:?}"),
        }
        session.notify_pause(true).expect("notify_pause");

        // closeStream ends the session cleanly.
        assert!(session.next_event().expect("next_event 2").is_none());
        session.close().expect("close");
    });

    let (tcp, mut reader, mut writer, stream_id) = dial_subscriber(addr);

    // §3.7: SetBufferLength is sent before the server starts
    // processing the stream.
    writer
        .write_message(
            CSID_CONTROL,
            &build_user_control_set_buffer_length(stream_id, 3000),
        )
        .expect("set buffer length");
    // play with Start = 0 (recorded from the beginning), Duration = -1,
    // Reset = true.
    let play = NetStreamCommand::Play {
        stream_name: STREAM.into(),
        start: Some(0.0),
        duration: Some(-1.0),
        reset: Some(true),
    };
    writer
        .write_message(CSID_COMMAND, &play.to_message(stream_id))
        .expect("play");
    writer.flush().expect("flush play");

    // Figure 5 ordering: StreamBegin → onStatus(Play.Reset) →
    // onStatus(Play.Start).
    let m = read_next(&mut reader);
    assert_eq!(m.msg_type_id, MSG_USER_CONTROL, "StreamBegin first");
    assert_eq!(
        u16::from_be_bytes([m.payload[0], m.payload[1]]),
        USR_STREAM_BEGIN
    );
    assert_eq!(
        u32::from_be_bytes([m.payload[2], m.payload[3], m.payload[4], m.payload[5]]),
        stream_id
    );
    let reset_status = drain_until_command(&mut reader, "onStatus");
    assert_eq!(on_status_code(&reset_status), STATUS_PLAY_RESET);
    let start_status = drain_until_command(&mut reader, "onStatus");
    assert_eq!(on_status_code(&start_status), STATUS_PLAY_START);

    // Metadata: bare ["onMetaData", meta] pair (no @setDataFrame).
    let m = read_next(&mut reader);
    assert_eq!(m.msg_type_id, MSG_DATA_AMF0);
    assert_eq!(m.msg_stream_id, stream_id);
    let vals = amf::decode_all(&m.payload).expect("decode metadata");
    assert_eq!(vals[0].as_str(), Some("onMetaData"));
    assert_eq!(
        vals[1].get("width").and_then(Amf0Value::as_f64),
        Some(320.0)
    );

    // Video then audio, on the play stream id, re-parseable via flv.
    let m = read_next(&mut reader);
    assert_eq!(m.msg_type_id, MSG_VIDEO);
    assert_eq!(m.msg_stream_id, stream_id);
    assert_eq!(m.timestamp, 40);
    let vtag = flv::parse_video(&m.payload).expect("parse video");
    assert_eq!(vtag.body, vec![0, 0, 0, 2, 0x65, 0x88]);

    let m = read_next(&mut reader);
    assert_eq!(m.msg_type_id, MSG_AUDIO);
    assert_eq!(m.msg_stream_id, stream_id);
    assert_eq!(m.timestamp, 63);
    let atag = flv::parse_audio(&m.payload).expect("parse audio");
    assert_eq!(atag.body, vec![0x21, 0x42, 0x63]);

    // pause(true, 63 ms) → server replies NetStream.Pause.Notify.
    let pause = NetStreamCommand::Pause {
        pause: true,
        milliseconds: 63.0,
    };
    writer
        .write_message(CSID_COMMAND, &pause.to_message(stream_id))
        .expect("pause");
    writer.flush().expect("flush pause");
    let pause_status = drain_until_command(&mut reader, "onStatus");
    assert_eq!(on_status_code(&pause_status), STATUS_PAUSE_NOTIFY);

    // closeStream → the server-side event loop ends.
    let close = amf::encode_command("closeStream", 0.0, Amf0Value::Null, &[]);
    writer
        .write_message(
            CSID_COMMAND,
            &Message {
                msg_type_id: MSG_COMMAND_AMF0,
                msg_stream_id: stream_id,
                timestamp: 0,
                payload: close,
            },
        )
        .expect("closeStream");
    writer.flush().expect("flush closeStream");

    let _ = tcp.shutdown(std::net::Shutdown::Write);
    server_thread.join().expect("server thread");
}

/// A recorded stream announces `UserControl StreamIsRecorded` ahead of
/// `StreamBegin` per the Figure 5 flow, and skips `Play.Reset` when
/// the play command did not set the reset flag.
#[test]
fn play_accept_recorded_emits_stream_is_recorded_first() {
    let server = RtmpServer::bind("127.0.0.1:0").expect("bind");
    let addr = server.local_addr().expect("local_addr");

    let server_thread = thread::spawn(move || {
        let req = match server.accept_any().expect("accept_any") {
            SessionRequest::Play(req) => req,
            SessionRequest::Publish(_) => panic!("expected a play request"),
        };
        assert_eq!(req.reset, None);
        assert_eq!(req.buffer_length_ms, None);
        let session = req.accept_recorded().expect("accept_recorded");
        session.close().expect("close");
    });

    let (tcp, mut reader, mut writer, stream_id) = dial_subscriber(addr);
    let play = NetStreamCommand::Play {
        stream_name: STREAM.into(),
        start: None,
        duration: None,
        reset: None,
    };
    writer
        .write_message(CSID_COMMAND, &play.to_message(stream_id))
        .expect("play");
    writer.flush().expect("flush play");

    // StreamIsRecorded (UCM 4) → StreamBegin (UCM 0) → Play.Start; no
    // Play.Reset in between.
    let m = read_next(&mut reader);
    assert_eq!(m.msg_type_id, MSG_USER_CONTROL);
    assert_eq!(u16::from_be_bytes([m.payload[0], m.payload[1]]), 4);
    let m = read_next(&mut reader);
    assert_eq!(m.msg_type_id, MSG_USER_CONTROL);
    assert_eq!(
        u16::from_be_bytes([m.payload[0], m.payload[1]]),
        USR_STREAM_BEGIN
    );
    let status = drain_until_command(&mut reader, "onStatus");
    assert_eq!(on_status_code(&status), STATUS_PLAY_START);

    let _ = tcp.shutdown(std::net::Shutdown::Write);
    server_thread.join().expect("server thread");
}

/// The publish-only [`RtmpServer::accept`] path politely refuses a
/// play connection with `onStatus(NetStream.Play.StreamNotFound)`
/// (§4.2.1's stream-not-found refusal) instead of hanging.
#[test]
fn publish_only_accept_rejects_play_with_stream_not_found() {
    let server = RtmpServer::bind("127.0.0.1:0").expect("bind");
    let addr = server.local_addr().expect("local_addr");

    // `accept()` keeps listening after the refusal, so run it on a
    // side thread and let it park there once the play peer is gone.
    let _server_thread = thread::spawn(move || {
        let _ = server.accept();
    });

    let (tcp, mut reader, mut writer, stream_id) = dial_subscriber(addr);
    let play = NetStreamCommand::Play {
        stream_name: STREAM.into(),
        start: None,
        duration: None,
        reset: None,
    };
    writer
        .write_message(CSID_COMMAND, &play.to_message(stream_id))
        .expect("play");
    writer.flush().expect("flush play");

    let status = drain_until_command(&mut reader, "onStatus");
    assert_eq!(on_status_code(&status), STATUS_PLAY_STREAM_NOT_FOUND);
    assert_eq!(
        status
            .get(3)
            .and_then(|info| info.get("level"))
            .and_then(Amf0Value::as_str),
        Some("error")
    );
    let _ = tcp.shutdown(std::net::Shutdown::Both);
}
