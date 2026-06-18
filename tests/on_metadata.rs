//! Integration tests for the typed `onMetaData` view (Enhanced RTMP v2
//! §"Enhancing onMetaData").
//!
//! The spec mandates the `onMetaData` argument be an ECMA array of metadata
//! properties; its "Typical properties found in the onMetaData argument
//! object" table enumerates the well-known names. These tests confirm that
//! [`OnMetaData::from_amf0`] / [`OnMetaData::to_amf0`] lift and rebuild that
//! table losslessly, that the codec-id FourCC note ("Opus" == 0x4F707573)
//! decodes, and that the v2 per-track info maps round-trip verbatim.

use oxideav_rtmp::flv::OnMetaData;
use oxideav_rtmp::Amf0Value;

fn ecma(pairs: Vec<(&str, Amf0Value)>) -> Amf0Value {
    Amf0Value::EcmaArray(pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect())
}

#[test]
fn lifts_typical_properties_table() {
    let arg = ecma(vec![
        ("duration", Amf0Value::Number(12.5)),
        ("width", Amf0Value::Number(1920.0)),
        ("height", Amf0Value::Number(1080.0)),
        ("videodatarate", Amf0Value::Number(4500.0)),
        ("framerate", Amf0Value::Number(30.0)),
        ("videocodecid", Amf0Value::Number(7.0)), // legacy AVC CodecID
        ("audiodatarate", Amf0Value::Number(128.0)),
        ("audiosamplerate", Amf0Value::Number(48000.0)),
        ("audiosamplesize", Amf0Value::Number(16.0)),
        ("stereo", Amf0Value::Boolean(true)),
        ("audiocodecid", Amf0Value::Number(10.0)), // legacy AAC CodecID
        ("filesize", Amf0Value::Number(123456.0)),
        ("audiodelay", Amf0Value::Number(0.05)),
        ("canSeekToEnd", Amf0Value::Boolean(true)),
        ("creationdate", Amf0Value::String("Wed Jun 18 2026".into())),
    ]);

    let m = OnMetaData::from_amf0(&arg).unwrap();
    assert_eq!(m.duration, Some(12.5));
    assert_eq!(m.width, Some(1920.0));
    assert_eq!(m.height, Some(1080.0));
    assert_eq!(m.videodatarate, Some(4500.0));
    assert_eq!(m.framerate, Some(30.0));
    assert_eq!(m.videocodecid, Some(7.0));
    assert_eq!(m.audiodatarate, Some(128.0));
    assert_eq!(m.audiosamplerate, Some(48000.0));
    assert_eq!(m.audiosamplesize, Some(16.0));
    assert_eq!(m.stereo, Some(true));
    assert_eq!(m.audiocodecid, Some(10.0));
    assert_eq!(m.filesize, Some(123456.0));
    assert_eq!(m.audiodelay, Some(0.05));
    assert_eq!(m.can_seek_to_end, Some(true));
    assert_eq!(m.creationdate.as_deref(), Some("Wed Jun 18 2026"));
    assert!(m.extra.is_empty());

    // Legacy single-byte CodecIDs are not FourCCs.
    assert_eq!(m.audio_fourcc(), None);
    assert_eq!(m.video_fourcc(), None);
}

#[test]
fn plain_object_argument_is_accepted() {
    // Commodity peers sometimes emit an anonymous Object instead of the
    // mandated ECMA array; the decoder accepts both.
    let arg = Amf0Value::Object(vec![("duration".into(), Amf0Value::Number(3.0))]);
    let m = OnMetaData::from_amf0(&arg).unwrap();
    assert_eq!(m.duration, Some(3.0));
}

#[test]
fn rejects_non_object_argument() {
    assert!(OnMetaData::from_amf0(&Amf0Value::Number(1.0)).is_err());
    assert!(OnMetaData::from_amf0(&Amf0Value::Null).is_err());
}

#[test]
fn fourcc_codec_ids_decode_per_spec_note() {
    // Enhanced RTMP v2: a FourCC value is big-endian relative to the
    // underlying ASCII sequence. "Opus" == 0x4F707573 == 1332770163.0 and
    // "av01" == 0x61763031 == 1635135537.0.
    let arg = ecma(vec![
        ("audiocodecid", Amf0Value::Number(1332770163.0)),
        ("videocodecid", Amf0Value::Number(1635135537.0)),
    ]);
    let m = OnMetaData::from_amf0(&arg).unwrap();
    assert_eq!(m.audio_fourcc(), Some(*b"Opus"));
    assert_eq!(m.video_fourcc(), Some(*b"av01"));
}

#[test]
fn re_encodes_as_ecma_array_in_spec_order() {
    let mut m = OnMetaData {
        duration: Some(10.0),
        width: Some(640.0),
        videocodecid: Some(1635135537.0), // av01
        ..Default::default()
    };
    m.stereo = Some(false);

    let v = m.to_amf0();
    match &v {
        Amf0Value::EcmaArray(pairs) => {
            // Known fields appear in the spec table's order: audiosamplesize-
            // style audio props come before duration/filesize/framerate/
            // height/stereo, which come before video* fields.
            let keys: Vec<&str> = pairs.iter().map(|(k, _)| k.as_str()).collect();
            assert_eq!(keys, vec!["duration", "stereo", "videocodecid", "width"]);
        }
        other => panic!("expected EcmaArray, got {other:?}"),
    }

    // Decoding what we encoded recovers the same struct.
    let back = OnMetaData::from_amf0(&v).unwrap();
    assert_eq!(back, m);
}

#[test]
fn unknown_properties_preserved_in_extra() {
    let arg = ecma(vec![
        ("duration", Amf0Value::Number(1.0)),
        ("encoder", Amf0Value::String("oxideav".into())),
        ("metadatacreator", Amf0Value::String("test".into())),
    ]);
    let m = OnMetaData::from_amf0(&arg).unwrap();
    assert_eq!(
        m.extra,
        vec![
            ("encoder".to_string(), Amf0Value::String("oxideav".into())),
            (
                "metadatacreator".to_string(),
                Amf0Value::String("test".into())
            ),
        ]
    );
    // Round-trip preserves the unknowns in order, after the known fields.
    let back = OnMetaData::from_amf0(&m.to_amf0()).unwrap();
    assert_eq!(back, m);
}

#[test]
fn per_track_info_maps_round_trip_verbatim() {
    // v2 audioTrackIdInfoMap / videoTrackIdInfoMap: keyed by trackId, each
    // value an object of per-track attributes. Preserved verbatim.
    let video_map = Amf0Value::Object(vec![(
        "1".into(),
        Amf0Value::Object(vec![
            ("width".into(), Amf0Value::Number(1280.0)),
            ("height".into(), Amf0Value::Number(720.0)),
        ]),
    )]);
    let audio_map = Amf0Value::Object(vec![(
        "1".into(),
        Amf0Value::Object(vec![("audiodatarate".into(), Amf0Value::Number(96.0))]),
    )]);
    let arg = ecma(vec![
        ("videoTrackIdInfoMap", video_map.clone()),
        ("audioTrackIdInfoMap", audio_map.clone()),
    ]);

    let m = OnMetaData::from_amf0(&arg).unwrap();
    assert_eq!(m.video_track_id_info_map.as_ref(), Some(&video_map));
    assert_eq!(m.audio_track_id_info_map.as_ref(), Some(&audio_map));

    let back = OnMetaData::from_amf0(&m.to_amf0()).unwrap();
    assert_eq!(back, m);
}

#[test]
fn round_trips_through_amf0_wire_codec() {
    // Encode to AMF0 bytes and decode back — the typed view survives the
    // real on-wire serialization, not just the in-memory Amf0Value.
    let m = OnMetaData {
        duration: Some(7.25),
        width: Some(854.0),
        height: Some(480.0),
        framerate: Some(25.0),
        videocodecid: Some(1635135537.0), // av01
        audiocodecid: Some(1332770163.0), // Opus
        stereo: Some(true),
        ..Default::default()
    };

    let mut bytes = Vec::new();
    oxideav_rtmp::amf::encode(&mut bytes, &m.to_amf0());
    let mut pos = 0usize;
    let decoded = oxideav_rtmp::amf::decode(&bytes, &mut pos).unwrap();
    assert_eq!(pos, bytes.len());

    let back = OnMetaData::from_amf0(&decoded).unwrap();
    assert_eq!(back, m);
    assert_eq!(back.video_fourcc(), Some(*b"av01"));
    assert_eq!(back.audio_fourcc(), Some(*b"Opus"));
}
