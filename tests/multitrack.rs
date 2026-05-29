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
