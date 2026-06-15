//! End-to-end test for [`oxideav_rtmp::flv_file::FlvWriter`]: drive
//! an RTMP loopback, record every received `StreamPacket` to an
//! in-memory `.flv` byte stream, then walk the resulting buffer
//! tag-by-tag and assert it matches the publisher's input verbatim.
//!
//! Source of truth for the wire layout: `docs/container/flv/
//! flv_v10_1.pdf` Annex E (§E.2 file header, §E.3 file body
//! alternation, §E.4.1 FLVTAG, §E.4.2 audio body, §E.4.3 video body).

use std::io::Cursor;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use oxideav_rtmp::flv::{
    self, AAC_PACKET_TYPE_SEQUENCE_HEADER, AUDIO_FORMAT_AAC, AVC_PACKET_TYPE_SEQUENCE_HEADER,
    VIDEO_CODEC_AVC, VIDEO_FRAME_INTER, VIDEO_FRAME_KEYFRAME,
};
use oxideav_rtmp::flv_file::{
    build_flv_header, FlvHeaderFlags, FlvReader, FlvTag, FlvWriter, FLV_HEADER_SIZE,
    FLV_TAG_TYPE_AUDIO, FLV_TAG_TYPE_SCRIPT_DATA, FLV_TAG_TYPE_VIDEO,
};
use oxideav_rtmp::{Amf0Value, AudioTag, RtmpClient, RtmpServer, StreamPacket, VideoTag};

const APP: &str = "live";
const STREAM_KEY: &str = "flv-record-test";

/// Walk an FLV byte stream and emit `(tag_type, timestamp, payload)`
/// tuples. Verifies the alternating `PreviousTagSize` back-pointers
/// per §E.3 and the 11-byte FLVTAG header per §E.4.1.
fn read_flv_stream(buf: &[u8]) -> Vec<(u8, u32, Vec<u8>)> {
    assert!(buf.len() >= 13, "FLV stream too small: {}", buf.len());
    // §E.2 — signature + version + flags + DataOffset.
    assert_eq!(&buf[0..3], b"FLV", "bad FLV signature");
    assert_eq!(buf[3], 1, "FLV version must be 1");
    let data_offset = u32::from_be_bytes(buf[5..9].try_into().unwrap());
    assert_eq!(
        data_offset, FLV_HEADER_SIZE,
        "DataOffset must equal header size"
    );
    // §E.3 — PreviousTagSize0 is mandated zero.
    let prev0 = u32::from_be_bytes(buf[9..13].try_into().unwrap());
    assert_eq!(prev0, 0, "PreviousTagSize0 must be 0");

    let mut pos = 13;
    let mut tags = Vec::new();
    let mut expected_prev = 0u32;
    while pos < buf.len() {
        // FLVTAG header: 11 bytes.
        assert!(pos + 11 <= buf.len(), "truncated FLVTAG header at {pos}");
        let tag_type = buf[pos] & 0x1F; // UB[5] tag type, reserved bits zero.
        let data_size =
            ((buf[pos + 1] as u32) << 16) | ((buf[pos + 2] as u32) << 8) | buf[pos + 3] as u32;
        let ts_lo =
            ((buf[pos + 4] as u32) << 16) | ((buf[pos + 5] as u32) << 8) | buf[pos + 6] as u32;
        let ts_hi = buf[pos + 7] as u32;
        let timestamp = (ts_hi << 24) | ts_lo;
        let stream_id =
            ((buf[pos + 8] as u32) << 16) | ((buf[pos + 9] as u32) << 8) | buf[pos + 10] as u32;
        assert_eq!(stream_id, 0, "FLV StreamID must be 0");
        pos += 11;
        assert!(
            pos + data_size as usize <= buf.len(),
            "FLVTAG payload runs past EOF: pos={pos} data_size={data_size}"
        );
        let payload = buf[pos..pos + data_size as usize].to_vec();
        pos += data_size as usize;
        // PreviousTagSize trailing this tag.
        assert!(pos + 4 <= buf.len(), "missing PreviousTagSize at {pos}");
        let prev = u32::from_be_bytes(buf[pos..pos + 4].try_into().unwrap());
        let tag_total = 11 + data_size;
        assert_eq!(
            prev, tag_total,
            "PreviousTagSize {prev} != expected 11+DataSize {tag_total}"
        );
        expected_prev = prev;
        pos += 4;
        tags.push((tag_type, timestamp, payload));
    }
    let _ = expected_prev;
    tags
}

#[test]
fn record_rtmp_publish_into_flv_file_byte_stream() {
    let server = RtmpServer::bind("127.0.0.1:0").expect("bind");
    let addr = server.local_addr().expect("local_addr");

    let (sink_tx, sink_rx) = mpsc::channel::<Vec<u8>>();

    // Server-side: accept the publisher, pipe every packet straight
    // into an in-memory FLV byte stream via FlvWriter.
    let server_thread = thread::spawn(move || {
        let req = server.accept().expect("server accept");
        let mut session = req.accept().expect("session accept");
        session
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("set_read_timeout");
        let buf: Vec<u8> = Vec::new();
        let mut writer = FlvWriter::new(
            Cursor::new(buf),
            FlvHeaderFlags {
                audio: true,
                video: true,
            },
        )
        .expect("flv new");
        // Emit an onMetaData script tag first — typical FLV layout.
        let meta = Amf0Value::EcmaArray(vec![
            ("encoder".into(), Amf0Value::String("oxideav-rtmp".into())),
            ("hasAudio".into(), Amf0Value::Boolean(true)),
            ("hasVideo".into(), Amf0Value::Boolean(true)),
        ]);
        writer
            .write_script_data(0, "onMetaData", &meta)
            .expect("write meta");
        loop {
            match session.next_packet() {
                Ok(Some(StreamPacket::Audio { timestamp, tag })) => writer
                    .write_audio_tag(timestamp, &tag)
                    .expect("write audio"),
                Ok(Some(StreamPacket::Video { timestamp, tag })) => writer
                    .write_video_tag(timestamp, &tag)
                    .expect("write video"),
                Ok(Some(StreamPacket::Metadata(value))) => writer
                    .write_script_data(0, "onMetaData", &value)
                    .expect("write meta runtime"),
                Ok(Some(StreamPacket::Command(_))) => {}
                Ok(None) | Err(_) => break,
            }
        }
        let cursor = writer.finish().expect("finish");
        sink_tx
            .send(cursor.into_inner())
            .expect("ship flv bytes back");
    });

    thread::sleep(Duration::from_millis(50));

    // Client side: dial, publish a known sequence of frames.
    let url = format!("rtmp://{}:{}/{APP}/{STREAM_KEY}", addr.ip(), addr.port());
    let mut client = RtmpClient::connect(&url).expect("client connect");
    let avc_c = b"\x01\x42\x80\x1e\x00".to_vec();
    client
        .send_video_sequence_header(&avc_c)
        .expect("send avc seq");
    let asc = vec![0x12, 0x10];
    client
        .send_audio_sequence_header(&asc)
        .expect("send aac seq");
    let nalu_k: Vec<u8> = (0..200).map(|i| i as u8).collect();
    let nalu_p: Vec<u8> = (0..120).map(|i| (i * 3) as u8).collect();
    client.send_video(0, true, &nalu_k).expect("send video 0");
    client.send_video(33, false, &nalu_p).expect("send video 1");
    let aac_a: Vec<u8> = (0..150).map(|i| ((i + 5) * 11) as u8).collect();
    client.send_audio(20, &aac_a).expect("send audio 0");
    client.close().expect("client close");
    server_thread.join().expect("server thread");

    let flv = sink_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("flv bytes ship back");
    let tags = read_flv_stream(&flv);

    // Expected order, ignoring any extra onMetaData echo if the
    // server-side `RtmpSession::next_packet` happened to surface a
    // setDataFrame from the client (it won't in this test, the
    // client doesn't send one).
    let video_tags: Vec<&(u8, u32, Vec<u8>)> = tags
        .iter()
        .filter(|(t, ..)| *t == FLV_TAG_TYPE_VIDEO)
        .collect();
    let audio_tags: Vec<&(u8, u32, Vec<u8>)> = tags
        .iter()
        .filter(|(t, ..)| *t == FLV_TAG_TYPE_AUDIO)
        .collect();
    let script_tags: Vec<&(u8, u32, Vec<u8>)> = tags
        .iter()
        .filter(|(t, ..)| *t == FLV_TAG_TYPE_SCRIPT_DATA)
        .collect();

    assert_eq!(video_tags.len(), 3, "expected 3 video tags");
    assert_eq!(audio_tags.len(), 2, "expected 2 audio tags");
    assert!(
        !script_tags.is_empty(),
        "expected at least the onMetaData script tag"
    );

    // Walk the recorded videos back through parse_video — every one
    // must round-trip.
    let v0 = flv::parse_video(&video_tags[0].2).expect("v0 parse");
    assert!(v0.is_avc_sequence_header());
    assert_eq!(v0.body, avc_c);
    let v1 = flv::parse_video(&video_tags[1].2).expect("v1 parse");
    assert_eq!(v1.frame_type, VIDEO_FRAME_KEYFRAME);
    assert_eq!(v1.codec_id, VIDEO_CODEC_AVC);
    assert_eq!(v1.body, nalu_k);
    assert_eq!(video_tags[1].1, 0);
    let v2 = flv::parse_video(&video_tags[2].2).expect("v2 parse");
    assert_eq!(v2.frame_type, VIDEO_FRAME_INTER);
    assert_eq!(v2.body, nalu_p);
    assert_eq!(video_tags[2].1, 33);

    let a0 = flv::parse_audio(&audio_tags[0].2).expect("a0 parse");
    assert_eq!(a0.sound_format, AUDIO_FORMAT_AAC);
    assert_eq!(
        a0.aac_packet_type,
        Some(AAC_PACKET_TYPE_SEQUENCE_HEADER),
        "first audio tag MUST be the ASC"
    );
    assert_eq!(a0.body, asc);
    let a1 = flv::parse_audio(&audio_tags[1].2).expect("a1 parse");
    assert_eq!(a1.body, aac_a);
    assert_eq!(audio_tags[1].1, 20);
}

/// Walk a brand-new writer with no payloads — just the header +
/// PreviousTagSize0 — and confirm the reader-side helper accepts it
/// as an empty stream.
#[test]
fn empty_flv_stream_parses_to_zero_tags() {
    let w = FlvWriter::new(
        Vec::new(),
        FlvHeaderFlags {
            audio: true,
            video: true,
        },
    )
    .expect("new");
    let buf = w.finish().expect("finish");
    let tags = read_flv_stream(&buf);
    assert!(tags.is_empty(), "expected no tags, got {tags:?}");
    // 13-byte file body: 9 header + 4 PreviousTagSize0.
    assert_eq!(buf.len(), 13);
    let header = build_flv_header(FlvHeaderFlags {
        audio: true,
        video: true,
    });
    assert_eq!(&buf[..9], &header);
}

/// End-to-end test for [`FlvReader`]: drive the same RTMP loopback
/// the writer test above uses, but read the recorded stream back
/// through the public reader API instead of the private byte-walker.
/// Proves the writer's output round-trips through the reader without
/// needing the integration-test-local `read_flv_stream` helper.
#[test]
fn record_rtmp_publish_then_read_back_through_flv_reader() {
    let server = RtmpServer::bind("127.0.0.1:0").expect("bind");
    let addr = server.local_addr().expect("local_addr");
    let (sink_tx, sink_rx) = mpsc::channel::<Vec<u8>>();

    let server_thread = thread::spawn(move || {
        let req = server.accept().expect("server accept");
        let mut session = req.accept().expect("session accept");
        session
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("set_read_timeout");
        let mut writer = FlvWriter::new(
            Cursor::new(Vec::<u8>::new()),
            FlvHeaderFlags {
                audio: true,
                video: true,
            },
        )
        .expect("flv new");
        writer
            .write_script_data(
                0,
                "onMetaData",
                &Amf0Value::EcmaArray(vec![(
                    "source".into(),
                    Amf0Value::String("loopback".into()),
                )]),
            )
            .expect("write meta");
        loop {
            match session.next_packet() {
                Ok(Some(StreamPacket::Audio { timestamp, tag })) => {
                    writer.write_audio_tag(timestamp, &tag).expect("audio")
                }
                Ok(Some(StreamPacket::Video { timestamp, tag })) => {
                    writer.write_video_tag(timestamp, &tag).expect("video")
                }
                Ok(Some(StreamPacket::Metadata(v))) => writer
                    .write_script_data(0, "onMetaData", &v)
                    .expect("meta rt"),
                Ok(Some(StreamPacket::Command(_))) => {}
                Ok(None) | Err(_) => break,
            }
        }
        sink_tx
            .send(writer.finish().expect("finish").into_inner())
            .expect("ship");
    });

    thread::sleep(Duration::from_millis(50));

    let url = format!(
        "rtmp://{}:{}/{APP}/{STREAM_KEY}-reader",
        addr.ip(),
        addr.port()
    );
    let mut client = RtmpClient::connect(&url).expect("client connect");
    let avc_c = b"\x01\x42\x80\x1e\x00".to_vec();
    client.send_video_sequence_header(&avc_c).expect("avc seq");
    let asc = vec![0x12, 0x10];
    client.send_audio_sequence_header(&asc).expect("aac seq");
    let nalu_k: Vec<u8> = (0..150).map(|i| (i ^ 0x5A) as u8).collect();
    client.send_video(0, true, &nalu_k).expect("keyframe");
    let aac_a: Vec<u8> = (0..80).map(|i| (i * 7) as u8).collect();
    client.send_audio(40, &aac_a).expect("audio frame");
    client.close().expect("client close");
    server_thread.join().expect("server thread");

    let flv = sink_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("flv bytes");
    let reader = FlvReader::new(Cursor::new(&flv[..])).expect("reader new");
    assert_eq!(
        reader.flags(),
        FlvHeaderFlags {
            audio: true,
            video: true
        }
    );
    let tags = reader.read_all().expect("read_all");

    // The reader walked every tag the writer emitted — script +
    // video sequence header + audio sequence header + keyframe +
    // audio frame.
    let videos: Vec<&FlvTag> = tags
        .iter()
        .filter(|t| t.tag_type() == FLV_TAG_TYPE_VIDEO)
        .collect();
    let audios: Vec<&FlvTag> = tags
        .iter()
        .filter(|t| t.tag_type() == FLV_TAG_TYPE_AUDIO)
        .collect();
    let scripts: Vec<&FlvTag> = tags
        .iter()
        .filter(|t| t.tag_type() == FLV_TAG_TYPE_SCRIPT_DATA)
        .collect();
    assert_eq!(videos.len(), 2, "expected seq-header + keyframe");
    assert_eq!(audios.len(), 2, "expected seq-header + raw");
    assert!(!scripts.is_empty(), "expected onMetaData");

    match videos[0] {
        FlvTag::Video { tag, .. } => {
            assert!(tag.is_avc_sequence_header());
            assert_eq!(tag.body, avc_c);
        }
        other => panic!("expected Video, got {other:?}"),
    }
    match videos[1] {
        FlvTag::Video { timestamp_ms, tag } => {
            assert_eq!(*timestamp_ms, 0);
            assert_eq!(tag.body, nalu_k);
        }
        other => panic!("expected Video keyframe, got {other:?}"),
    }
    match audios[0] {
        FlvTag::Audio { tag, .. } => assert_eq!(tag.body, asc),
        other => panic!("expected Audio seq header, got {other:?}"),
    }
    match audios[1] {
        FlvTag::Audio { timestamp_ms, tag } => {
            assert_eq!(*timestamp_ms, 40);
            assert_eq!(tag.body, aac_a);
        }
        other => panic!("expected Audio frame, got {other:?}"),
    }
    match scripts[0] {
        FlvTag::Script { name, .. } => assert_eq!(name, "onMetaData"),
        other => panic!("expected Script, got {other:?}"),
    }
}

/// A direct round-trip: hand-build a VideoTag + AudioTag, frame each
/// to FLV, then re-parse the FLVTAG payloads via `parse_video` /
/// `parse_audio` — no network involved. The crate's parser is the
/// same one the RTMP ingest path uses, so a successful re-parse
/// proves the bytes match what any spec-compliant `.flv` reader
/// expects per Annex E.
#[test]
fn local_videotag_audiotag_round_trip_through_writer_and_parser() {
    let video = VideoTag {
        mod_ex: Vec::new(),
        frame_type: VIDEO_FRAME_KEYFRAME,
        codec_id: VIDEO_CODEC_AVC,
        avc_packet_type: Some(AVC_PACKET_TYPE_SEQUENCE_HEADER),
        composition_time: 0,
        body: vec![0x01, 0x42, 0x80, 0x1E, 0xFF, 0xE1, 0x00, 0x05],
        ex_packet_type: None,
        fourcc: None,
        multitrack: None,
    };
    let audio = AudioTag {
        mod_ex: Vec::new(),
        sound_format: AUDIO_FORMAT_AAC,
        sound_rate: 3,
        sound_size_16bit: true,
        stereo: true,
        aac_packet_type: Some(AAC_PACKET_TYPE_SEQUENCE_HEADER),
        body: vec![0x12, 0x10],
        ex_packet_type: None,
        audio_fourcc: None,
        multitrack: None,
    };
    let mut w = FlvWriter::new(
        Vec::new(),
        FlvHeaderFlags {
            audio: true,
            video: true,
        },
    )
    .expect("new");
    w.write_video_tag(0, &video).expect("write video");
    w.write_audio_tag(0, &audio).expect("write audio");
    let buf = w.finish().expect("finish");
    let tags = read_flv_stream(&buf);
    assert_eq!(tags.len(), 2);
    let (vt_type, vt_ts, vt_payload) = &tags[0];
    let (at_type, at_ts, at_payload) = &tags[1];
    assert_eq!(*vt_type, FLV_TAG_TYPE_VIDEO);
    assert_eq!(*vt_ts, 0);
    assert_eq!(*at_type, FLV_TAG_TYPE_AUDIO);
    assert_eq!(*at_ts, 0);
    let pv = flv::parse_video(vt_payload).expect("parse video back");
    assert!(pv.is_avc_sequence_header());
    assert_eq!(pv.body, video.body);
    let pa = flv::parse_audio(at_payload).expect("parse audio back");
    assert_eq!(pa.sound_format, AUDIO_FORMAT_AAC);
    assert_eq!(pa.body, audio.body);
}
