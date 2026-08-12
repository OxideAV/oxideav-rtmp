//! Integration tests for Enhanced RTMP v2 `Multitrack` packet bodies.
//!
//! Exercises the three `AvMultitrackType` modes — `OneTrack`,
//! `ManyTracks`, `ManyTracksManyCodecs` — across both audio and video,
//! confirming that each round-trips through
//! `build_video` / `parse_video` and `build_audio` / `parse_audio`,
//! that the wire bytes match the layout in
//! `docs/streaming/rtmp/enhanced-rtmp-v2.pdf` §"ExVideoTagBody" /
//! §"ExAudioTagBody", and that the strongly-typed [`Multitrack`] /
//! [`MultitrackTrack`] accessors lift the same data.

use oxideav_rtmp::flv::{
    build_audio, build_video, parse_audio, parse_video, AudioTag, Multitrack, MultitrackTrack,
    VideoTag, AUDIO_PACKET_TYPE_CODED_FRAMES, AUDIO_PACKET_TYPE_SEQUENCE_START,
    AV_MULTITRACK_TYPE_MANY_TRACKS, AV_MULTITRACK_TYPE_MANY_TRACKS_MANY_CODECS,
    AV_MULTITRACK_TYPE_ONE_TRACK, EX_PACKET_TYPE_CODED_FRAMES, EX_PACKET_TYPE_SEQUENCE_START,
    FOURCC_AAC, FOURCC_AV1, FOURCC_AVC, FOURCC_FLAC, FOURCC_HEVC, FOURCC_OPUS, FOURCC_VVC,
    VIDEO_FRAME_INTER, VIDEO_FRAME_KEYFRAME,
};

// -----------------------------------------------------------------
// Video
// -----------------------------------------------------------------

#[test]
fn video_one_track_codedframes_avc_matches_spec_wire_layout() {
    // Pseudocode from §"ExVideoTagHeader":
    //   header byte 0x96 = IsExHeader(1) | KeyFrame(001) | Multitrack(0110)
    //   nibble byte 0x01 = OneTrack(0000) | CodedFrames(0001)
    //   shared FourCC 'avc1'
    //   trackId 0
    //   body bytes (no UI24 size in OneTrack mode)
    let mt = Multitrack {
        multitrack_type: AV_MULTITRACK_TYPE_ONE_TRACK,
        tracks: vec![MultitrackTrack {
            fourcc: None,
            track_id: 0,
            body: b"\x00\x00\x00\x04NALU".to_vec(),
        }],
    };
    let tag = VideoTag::multitrack_tag(
        VIDEO_FRAME_KEYFRAME,
        EX_PACKET_TYPE_CODED_FRAMES,
        Some(FOURCC_AVC),
        mt,
    );
    let wire = build_video(&tag);
    assert_eq!(
        wire,
        vec![
            0x96, 0x01, b'a', b'v', b'c', b'1', 0x00, 0x00, 0x00, 0x00, 0x04, b'N', b'A', b'L',
            b'U',
        ]
    );
    let back = parse_video(&wire).expect("parse");
    assert!(back.is_multitrack());
    let mt_back = back.multitrack.as_ref().unwrap();
    assert_eq!(mt_back.multitrack_type, AV_MULTITRACK_TYPE_ONE_TRACK);
    assert_eq!(mt_back.tracks.len(), 1);
    assert_eq!(mt_back.tracks[0].track_id, 0);
    assert_eq!(mt_back.tracks[0].fourcc, None);
    assert_eq!(mt_back.tracks[0].body, b"\x00\x00\x00\x04NALU".to_vec());
    assert_eq!(back.fourcc, Some(FOURCC_AVC));
    assert_eq!(back.ex_packet_type, Some(EX_PACKET_TYPE_CODED_FRAMES));
}

#[test]
fn video_many_tracks_hevc_two_tracks_round_trip() {
    // ManyTracks: shared 'hvc1' FourCC, two tracks (default + alt
    // resolution), each with its own UI24 sizeOfVideoTrack.
    let mt = Multitrack {
        multitrack_type: AV_MULTITRACK_TYPE_MANY_TRACKS,
        tracks: vec![
            MultitrackTrack {
                fourcc: None,
                track_id: 0,
                body: vec![0x10; 32],
            },
            MultitrackTrack {
                fourcc: None,
                track_id: 1,
                body: vec![0x20; 64],
            },
        ],
    };
    let tag = VideoTag::multitrack_tag(
        VIDEO_FRAME_INTER,
        EX_PACKET_TYPE_CODED_FRAMES,
        Some(FOURCC_HEVC),
        mt,
    );
    let wire = build_video(&tag);
    let back = parse_video(&wire).expect("parse");
    let mt_back = back.multitrack.as_ref().unwrap();
    assert_eq!(mt_back.tracks.len(), 2);
    assert_eq!(mt_back.tracks[0].body.len(), 32);
    assert_eq!(mt_back.tracks[1].body.len(), 64);
    assert_eq!(back, tag);
}

#[test]
fn video_many_tracks_many_codecs_round_trip_with_hevc_and_av1() {
    // ManyTracksManyCodecs: per-track FourCC, per-track UI24 size.
    // Two tracks: HEVC + AV1, distinct payloads.
    let mt = Multitrack {
        multitrack_type: AV_MULTITRACK_TYPE_MANY_TRACKS_MANY_CODECS,
        tracks: vec![
            MultitrackTrack {
                fourcc: Some(FOURCC_HEVC),
                track_id: 0,
                body: b"hevc-coded-bytes".to_vec(),
            },
            MultitrackTrack {
                fourcc: Some(FOURCC_AV1),
                track_id: 1,
                body: b"av1-obu-bytes".to_vec(),
            },
        ],
    };
    let tag = VideoTag::multitrack_tag(
        VIDEO_FRAME_KEYFRAME,
        EX_PACKET_TYPE_CODED_FRAMES,
        None,
        mt.clone(),
    );
    assert_eq!(tag.fourcc, None);
    let wire = build_video(&tag);
    // Multitrack nibble byte: (2 << 4) | 1 = 0x21.
    assert_eq!(wire[1], 0x21);
    let back = parse_video(&wire).expect("parse");
    assert_eq!(back.fourcc, None);
    let mt_back = back.multitrack.as_ref().unwrap();
    assert_eq!(mt_back, &mt);
}

#[test]
fn video_sequence_start_many_tracks_vvc_round_trip() {
    // Multitrack can wrap any inner PacketType (the spec says the
    // inner fetch "MUST not result in Multitrack" but otherwise any
    // PacketType is fair game). This covers SequenceStart with the
    // VVCDecoderConfigurationRecord payload for two VVC tracks.
    let mt = Multitrack {
        multitrack_type: AV_MULTITRACK_TYPE_MANY_TRACKS,
        tracks: vec![
            MultitrackTrack {
                fourcc: None,
                track_id: 0,
                body: b"vvcc-default".to_vec(),
            },
            MultitrackTrack {
                fourcc: None,
                track_id: 1,
                body: b"vvcc-alt".to_vec(),
            },
        ],
    };
    let tag = VideoTag::multitrack_tag(
        VIDEO_FRAME_KEYFRAME,
        EX_PACKET_TYPE_SEQUENCE_START,
        Some(FOURCC_VVC),
        mt,
    );
    let wire = build_video(&tag);
    // Multitrack nibble byte: (1 << 4) | 0 (SequenceStart) = 0x10.
    assert_eq!(wire[1], 0x10);
    let back = parse_video(&wire).expect("parse");
    assert_eq!(back, tag);
    assert_eq!(back.ex_packet_type, Some(EX_PACKET_TYPE_SEQUENCE_START));
}

#[test]
fn video_parse_rejects_inner_packet_type_multitrack() {
    // Spec invariant: the inner real PacketType MUST NOT itself be
    // Multitrack. A forged tag with the inner nibble = 6 must fail
    // with a controlled error rather than recurse / panic.
    let wire = [0x96u8, 0x06, b'h', b'v', b'c', b'1', 0x00];
    let err = parse_video(&wire).unwrap_err();
    assert!(format!("{err:?}").contains("MUST NOT"));
}

// -----------------------------------------------------------------
// Audio
// -----------------------------------------------------------------

#[test]
fn audio_one_track_opus_codedframes_round_trip() {
    // OneTrack audio Multitrack carrying Opus CodedFrames data.
    let mt = Multitrack {
        multitrack_type: AV_MULTITRACK_TYPE_ONE_TRACK,
        tracks: vec![MultitrackTrack {
            fourcc: None,
            track_id: 0,
            body: b"opus-self-delimited-frames".to_vec(),
        }],
    };
    let tag = AudioTag::multitrack_tag(
        AUDIO_PACKET_TYPE_CODED_FRAMES,
        Some(FOURCC_OPUS),
        mt.clone(),
    );
    let wire = build_audio(&tag);
    let back = parse_audio(&wire).expect("parse");
    assert!(back.is_multitrack());
    assert_eq!(back.audio_fourcc, Some(FOURCC_OPUS));
    assert_eq!(back.ex_packet_type, Some(AUDIO_PACKET_TYPE_CODED_FRAMES));
    assert_eq!(back.multitrack.as_ref().unwrap(), &mt);
}

#[test]
fn audio_many_tracks_aac_two_tracks_round_trip() {
    // ManyTracks AAC: shared 'mp4a' FourCC, two tracks with distinct
    // raw AAC frame payloads.
    let mt = Multitrack {
        multitrack_type: AV_MULTITRACK_TYPE_MANY_TRACKS,
        tracks: vec![
            MultitrackTrack {
                fourcc: None,
                track_id: 0,
                body: vec![0xAA; 96],
            },
            MultitrackTrack {
                fourcc: None,
                track_id: 1,
                body: vec![0xBB; 128],
            },
        ],
    };
    let tag =
        AudioTag::multitrack_tag(AUDIO_PACKET_TYPE_CODED_FRAMES, Some(FOURCC_AAC), mt.clone());
    let wire = build_audio(&tag);
    // Multitrack nibble: (1 << 4) | 1 = 0x11.
    assert_eq!(wire[1], 0x11);
    let back = parse_audio(&wire).expect("parse");
    assert_eq!(back.multitrack.as_ref().unwrap(), &mt);
}

#[test]
fn audio_many_tracks_many_codecs_opus_flac_round_trip() {
    // Mixed-codec audio multitrack (use case: a localization track in
    // FLAC alongside the primary Opus mix). Per-track FourCC, per-track
    // UI24 size.
    let mt = Multitrack {
        multitrack_type: AV_MULTITRACK_TYPE_MANY_TRACKS_MANY_CODECS,
        tracks: vec![
            MultitrackTrack {
                fourcc: Some(FOURCC_OPUS),
                track_id: 0,
                body: b"primary-opus".to_vec(),
            },
            MultitrackTrack {
                fourcc: Some(FOURCC_FLAC),
                track_id: 1,
                body: b"localization-flac".to_vec(),
            },
        ],
    };
    let tag = AudioTag::multitrack_tag(AUDIO_PACKET_TYPE_CODED_FRAMES, None, mt.clone());
    assert_eq!(tag.audio_fourcc, None);
    let wire = build_audio(&tag);
    // Multitrack nibble: (2 << 4) | 1 = 0x21.
    assert_eq!(wire[1], 0x21);
    let back = parse_audio(&wire).expect("parse");
    assert_eq!(back.audio_fourcc, None);
    assert_eq!(back.multitrack.as_ref().unwrap(), &mt);
}

#[test]
fn audio_sequence_start_many_tracks_aac_round_trip() {
    // SequenceStart inside Multitrack: per-track AudioSpecificConfig
    // payloads for two AAC tracks.
    let mt = Multitrack {
        multitrack_type: AV_MULTITRACK_TYPE_MANY_TRACKS,
        tracks: vec![
            MultitrackTrack {
                fourcc: None,
                track_id: 0,
                body: vec![0x12, 0x10], // 2-byte AAC-LC 44.1k stereo ASC
            },
            MultitrackTrack {
                fourcc: None,
                track_id: 1,
                body: vec![0x12, 0x08], // 2-byte AAC-LC mono ASC
            },
        ],
    };
    let tag = AudioTag::multitrack_tag(
        AUDIO_PACKET_TYPE_SEQUENCE_START,
        Some(FOURCC_AAC),
        mt.clone(),
    );
    let wire = build_audio(&tag);
    // Header 0x95 = ExHeader(9) | Multitrack(5). Nibble (1 << 4) | 0 = 0x10.
    assert_eq!(wire[0], 0x95);
    assert_eq!(wire[1], 0x10);
    let back = parse_audio(&wire).expect("parse");
    assert_eq!(back.multitrack.as_ref().unwrap(), &mt);
    assert_eq!(back.ex_packet_type, Some(AUDIO_PACKET_TYPE_SEQUENCE_START));
}

#[test]
fn audio_parse_rejects_inner_packet_type_multitrack() {
    // Spec invariant for audio: inner AudioPacketType MUST NOT be
    // Multitrack (= 5). Forged byte 0x05 in the nibble slot.
    let wire = [0x95u8, 0x05, b'O', b'p', b'u', b's', 0x00];
    let err = parse_audio(&wire).unwrap_err();
    assert!(format!("{err:?}").contains("MUST NOT"));
}

// -----------------------------------------------------------------
// Cross-mode + edge cases
// -----------------------------------------------------------------

#[test]
fn track_ordering_preserved_across_round_trip() {
    // Spec §"ExVideoTagBody" Track Ordering: trackId 0 is the default
    // track; positive ids are variants. We don't assign any semantics
    // here, only verify the order of tracks is preserved verbatim
    // through build → parse.
    let mt = Multitrack {
        multitrack_type: AV_MULTITRACK_TYPE_MANY_TRACKS,
        tracks: vec![
            MultitrackTrack {
                fourcc: None,
                track_id: 7,
                body: b"first".to_vec(),
            },
            MultitrackTrack {
                fourcc: None,
                track_id: 0,
                body: b"second".to_vec(),
            },
            MultitrackTrack {
                fourcc: None,
                track_id: 3,
                body: b"third".to_vec(),
            },
        ],
    };
    let tag = VideoTag::multitrack_tag(
        VIDEO_FRAME_KEYFRAME,
        EX_PACKET_TYPE_CODED_FRAMES,
        Some(FOURCC_HEVC),
        mt.clone(),
    );
    let wire = build_video(&tag);
    let back = parse_video(&wire).expect("parse");
    let tracks = &back.multitrack.unwrap().tracks;
    assert_eq!(tracks.len(), 3);
    assert_eq!(tracks[0].track_id, 7);
    assert_eq!(tracks[1].track_id, 0);
    assert_eq!(tracks[2].track_id, 3);
    assert_eq!(tracks[0].body, b"first");
    assert_eq!(tracks[1].body, b"second");
    assert_eq!(tracks[2].body, b"third");
}

#[test]
fn empty_track_body_round_trips() {
    // A zero-length per-track body (sizeOfTrack = 0) is legal —
    // SequenceEnd track or similar. The UI24 size field encodes 0 and
    // the next track follows immediately.
    let mt = Multitrack {
        multitrack_type: AV_MULTITRACK_TYPE_MANY_TRACKS,
        tracks: vec![
            MultitrackTrack {
                fourcc: None,
                track_id: 0,
                body: Vec::new(),
            },
            MultitrackTrack {
                fourcc: None,
                track_id: 1,
                body: b"payload".to_vec(),
            },
        ],
    };
    let tag = VideoTag::multitrack_tag(
        VIDEO_FRAME_INTER,
        EX_PACKET_TYPE_CODED_FRAMES,
        Some(FOURCC_HEVC),
        mt.clone(),
    );
    let wire = build_video(&tag);
    let back = parse_video(&wire).expect("parse");
    assert_eq!(back.multitrack.as_ref().unwrap(), &mt);
}

// -----------------------------------------------------------------
// Per-track demux / mux (§"Multitrack Streaming via Enhanced RTMP")
// -----------------------------------------------------------------

/// Single-track Enhanced video tag helper.
fn ex_video(fourcc: [u8; 4], packet_type: u8, cts: i32, body: &[u8]) -> VideoTag {
    VideoTag {
        frame_type: VIDEO_FRAME_KEYFRAME,
        codec_id: 0,
        avc_packet_type: None,
        composition_time: cts,
        body: body.to_vec(),
        ex_packet_type: Some(packet_type),
        fourcc: Some(fourcc),
        mod_ex: Vec::new(),
        multitrack: None,
    }
}

/// Single-track Enhanced audio tag helper.
fn ex_audio(fourcc: [u8; 4], packet_type: u8, body: &[u8]) -> AudioTag {
    AudioTag {
        sound_format: 9,
        sound_rate: 0,
        sound_size_16bit: false,
        stereo: false,
        aac_packet_type: None,
        ex_packet_type: Some(packet_type),
        audio_fourcc: Some(fourcc),
        body: body.to_vec(),
        mod_ex: Vec::new(),
        multitrack: None,
    }
}

#[test]
fn video_demux_lifts_per_track_cts_for_nalu_fourccs() {
    // §"ExVideoTagBody": in a multitrack message the per-track body is
    // itself an ExVideoTagBody, so hvc1 × CodedFrames carries an SI24
    // compositionTimeOffset per track. Two tracks, two distinct CTSes.
    let t0 = ex_video(FOURCC_HEVC, EX_PACKET_TYPE_CODED_FRAMES, 17, b"frame-zero");
    let t1 = ex_video(FOURCC_HEVC, EX_PACKET_TYPE_CODED_FRAMES, -4, b"frame-one");
    let outer =
        VideoTag::multitrack_from_tags(AV_MULTITRACK_TYPE_MANY_TRACKS, &[(0, &t0), (5, &t1)])
            .expect("mux");
    assert_eq!(outer.fourcc, Some(FOURCC_HEVC));
    assert_eq!(outer.ex_packet_type, Some(EX_PACKET_TYPE_CODED_FRAMES));

    // The wire round-trip preserves the whole structure…
    let back = parse_video(&build_video(&outer)).expect("parse");
    // …and demux gives back the standalone per-track tags bit-exactly.
    let tracks = back.demux_tracks().expect("demux");
    assert_eq!(tracks.len(), 2);
    assert_eq!(tracks[0].0, 0);
    assert_eq!(tracks[0].1.composition_time, 17);
    assert_eq!(tracks[0].1.body, b"frame-zero");
    assert_eq!(tracks[0].1.fourcc, Some(FOURCC_HEVC));
    assert_eq!(tracks[1].0, 5);
    assert_eq!(tracks[1].1.composition_time, -4);
    assert_eq!(tracks[1].1.body, b"frame-one");
}

#[test]
fn video_mux_demux_many_codecs_round_trip() {
    // ManyTracksManyCodecs: per-track FourCC on the wire, no shared one.
    // av01 carries no CTS; hvc1 does — demux must honour each track's
    // own FourCC × PacketType body shape.
    let hevc = ex_video(FOURCC_HEVC, EX_PACKET_TYPE_CODED_FRAMES, 33, b"hevc-au");
    let av1 = ex_video(FOURCC_AV1, EX_PACKET_TYPE_CODED_FRAMES, 0, b"av1-tu");
    let outer = VideoTag::multitrack_from_tags(
        AV_MULTITRACK_TYPE_MANY_TRACKS_MANY_CODECS,
        &[(0, &hevc), (1, &av1)],
    )
    .expect("mux");
    assert_eq!(outer.fourcc, None);
    let back = parse_video(&build_video(&outer)).expect("parse");
    let tracks = back.demux_tracks().expect("demux");
    assert_eq!(tracks[0].1.fourcc, Some(FOURCC_HEVC));
    assert_eq!(tracks[0].1.composition_time, 33);
    assert_eq!(tracks[0].1.body, b"hevc-au");
    assert_eq!(tracks[1].1.fourcc, Some(FOURCC_AV1));
    assert_eq!(tracks[1].1.composition_time, 0);
    assert_eq!(tracks[1].1.body, b"av1-tu");
    // Full inverse: re-muxing the demuxed tags reproduces the outer tag.
    let remux = VideoTag::multitrack_from_tags(
        AV_MULTITRACK_TYPE_MANY_TRACKS_MANY_CODECS,
        &[(0, &tracks[0].1), (1, &tracks[1].1)],
    )
    .expect("remux");
    assert_eq!(build_video(&remux), build_video(&outer));
}

#[test]
fn video_mux_one_track_requires_exactly_one_track() {
    let t = ex_video(FOURCC_AVC, EX_PACKET_TYPE_CODED_FRAMES, 0, b"x");
    let err = VideoTag::multitrack_from_tags(AV_MULTITRACK_TYPE_ONE_TRACK, &[(0, &t), (1, &t)])
        .unwrap_err();
    assert!(err.to_string().contains("OneTrack"), "{err}");
    let ok = VideoTag::multitrack_from_tags(AV_MULTITRACK_TYPE_ONE_TRACK, &[(0, &t)]).expect("mux");
    let tracks = parse_video(&build_video(&ok))
        .unwrap()
        .demux_tracks()
        .unwrap();
    assert_eq!(tracks.len(), 1);
    assert_eq!(tracks[0].1.body, b"x");
}

#[test]
fn video_mux_rejects_mixed_packet_types_and_shared_fourcc_mismatch() {
    let cf = ex_video(FOURCC_HEVC, EX_PACKET_TYPE_CODED_FRAMES, 0, b"a");
    let ss = ex_video(FOURCC_HEVC, EX_PACKET_TYPE_SEQUENCE_START, 0, b"cfg");
    let err = VideoTag::multitrack_from_tags(AV_MULTITRACK_TYPE_MANY_TRACKS, &[(0, &cf), (1, &ss)])
        .unwrap_err();
    assert!(err.to_string().contains("PacketType"), "{err}");

    let other = ex_video(FOURCC_AV1, EX_PACKET_TYPE_CODED_FRAMES, 0, b"b");
    let err =
        VideoTag::multitrack_from_tags(AV_MULTITRACK_TYPE_MANY_TRACKS, &[(0, &cf), (1, &other)])
            .unwrap_err();
    assert!(err.to_string().contains("FourCC"), "{err}");
}

#[test]
fn video_mux_rejects_legacy_nested_and_modex_tracks() {
    // A legacy (non-Enhanced) tag has no FourCC / PacketType.
    let legacy = VideoTag {
        frame_type: VIDEO_FRAME_KEYFRAME,
        codec_id: 7,
        avc_packet_type: Some(1),
        composition_time: 0,
        body: b"nalu".to_vec(),
        ex_packet_type: None,
        fourcc: None,
        mod_ex: Vec::new(),
        multitrack: None,
    };
    let err =
        VideoTag::multitrack_from_tags(AV_MULTITRACK_TYPE_ONE_TRACK, &[(0, &legacy)]).unwrap_err();
    assert!(err.to_string().contains("Enhanced"), "{err}");

    // A multitrack tag cannot nest.
    let inner = ex_video(FOURCC_HEVC, EX_PACKET_TYPE_CODED_FRAMES, 0, b"a");
    let mt = VideoTag::multitrack_from_tags(AV_MULTITRACK_TYPE_ONE_TRACK, &[(0, &inner)]).unwrap();
    let err =
        VideoTag::multitrack_from_tags(AV_MULTITRACK_TYPE_ONE_TRACK, &[(0, &mt)]).unwrap_err();
    assert!(err.to_string().contains("Multitrack"), "{err}");
}

#[test]
fn video_demux_rejects_non_multitrack_and_truncated_track_body() {
    let plain = ex_video(FOURCC_HEVC, EX_PACKET_TYPE_CODED_FRAMES, 0, b"a");
    assert!(plain.demux_tracks().is_err());

    // hvc1 × CodedFrames needs >= 3 body bytes for the SI24 CTS; a
    // 2-byte track body must fail cleanly, not panic.
    let mt = Multitrack {
        multitrack_type: AV_MULTITRACK_TYPE_ONE_TRACK,
        tracks: vec![MultitrackTrack {
            fourcc: None,
            track_id: 0,
            body: vec![0x00, 0x01],
        }],
    };
    let outer = VideoTag::multitrack_tag(
        VIDEO_FRAME_KEYFRAME,
        EX_PACKET_TYPE_CODED_FRAMES,
        Some(FOURCC_HEVC),
        mt,
    );
    let err = outer.demux_tracks().unwrap_err();
    assert!(err.to_string().contains("track 0"), "{err}");
}

#[test]
fn audio_mux_demux_many_codecs_round_trip() {
    let opus = ex_audio(FOURCC_OPUS, AUDIO_PACKET_TYPE_CODED_FRAMES, b"opus-frame");
    let flac = ex_audio(FOURCC_FLAC, AUDIO_PACKET_TYPE_CODED_FRAMES, b"flac-frame");
    let outer = AudioTag::multitrack_from_tags(
        AV_MULTITRACK_TYPE_MANY_TRACKS_MANY_CODECS,
        &[(0, &opus), (3, &flac)],
    )
    .expect("mux");
    assert_eq!(outer.audio_fourcc, None);
    let back = parse_audio(&build_audio(&outer)).expect("parse");
    let tracks = back.demux_tracks().expect("demux");
    assert_eq!(tracks.len(), 2);
    assert_eq!(tracks[0].0, 0);
    assert_eq!(tracks[0].1.audio_fourcc, Some(FOURCC_OPUS));
    assert_eq!(tracks[0].1.body, b"opus-frame");
    assert_eq!(tracks[1].0, 3);
    assert_eq!(tracks[1].1.audio_fourcc, Some(FOURCC_FLAC));
    assert_eq!(tracks[1].1.body, b"flac-frame");
    let remux = AudioTag::multitrack_from_tags(
        AV_MULTITRACK_TYPE_MANY_TRACKS_MANY_CODECS,
        &[(0, &tracks[0].1), (3, &tracks[1].1)],
    )
    .expect("remux");
    assert_eq!(build_audio(&remux), build_audio(&outer));
}

#[test]
fn audio_mux_sequence_start_shared_fourcc_round_trip() {
    let a0 = ex_audio(FOURCC_AAC, AUDIO_PACKET_TYPE_SEQUENCE_START, b"\x12\x10");
    let a1 = ex_audio(FOURCC_AAC, AUDIO_PACKET_TYPE_SEQUENCE_START, b"\x11\x90");
    let outer =
        AudioTag::multitrack_from_tags(AV_MULTITRACK_TYPE_MANY_TRACKS, &[(0, &a0), (1, &a1)])
            .expect("mux");
    assert_eq!(outer.audio_fourcc, Some(FOURCC_AAC));
    assert_eq!(outer.ex_packet_type, Some(AUDIO_PACKET_TYPE_SEQUENCE_START));
    let tracks = parse_audio(&build_audio(&outer))
        .unwrap()
        .demux_tracks()
        .unwrap();
    assert_eq!(tracks[0].1.body, b"\x12\x10");
    assert_eq!(tracks[1].1.body, b"\x11\x90");
    assert_eq!(
        tracks[1].1.ex_packet_type,
        Some(AUDIO_PACKET_TYPE_SEQUENCE_START)
    );
}

#[test]
fn audio_mux_rejects_legacy_and_mixed_inner_types() {
    let legacy = AudioTag {
        sound_format: 10,
        sound_rate: 3,
        sound_size_16bit: true,
        stereo: true,
        aac_packet_type: Some(1),
        ex_packet_type: None,
        audio_fourcc: None,
        body: b"frame".to_vec(),
        mod_ex: Vec::new(),
        multitrack: None,
    };
    let err =
        AudioTag::multitrack_from_tags(AV_MULTITRACK_TYPE_ONE_TRACK, &[(0, &legacy)]).unwrap_err();
    assert!(err.to_string().contains("Enhanced"), "{err}");

    let cf = ex_audio(FOURCC_OPUS, AUDIO_PACKET_TYPE_CODED_FRAMES, b"x");
    let ss = ex_audio(FOURCC_OPUS, AUDIO_PACKET_TYPE_SEQUENCE_START, b"y");
    let err = AudioTag::multitrack_from_tags(AV_MULTITRACK_TYPE_MANY_TRACKS, &[(0, &cf), (1, &ss)])
        .unwrap_err();
    assert!(err.to_string().contains("PacketType"), "{err}");
}
