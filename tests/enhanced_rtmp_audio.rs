//! Integration tests for Enhanced RTMP v2 (Veovera 2026) audio framing.
//!
//! These exercise the parse → `audio_to_packet` → `CodecId` path so a
//! downstream consumer that wires `RtmpSession` into the registry sees
//! the right `CodecParameters` + `Packet` flags for Opus / FLAC / AC-3
//! / E-AC-3 / MP3 / AAC publishers using the FourCC variant defined in
//! `enhanced-rtmp-v2.pdf` §"Enhanced Audio".

use oxideav_rtmp::flv::{
    build_audio, parse_audio, AudioTag, AUDIO_FORMAT_EX_HEADER, AUDIO_PACKET_TYPE_CODED_FRAMES,
    AUDIO_PACKET_TYPE_SEQUENCE_END, AUDIO_PACKET_TYPE_SEQUENCE_START, FOURCC_AC3, FOURCC_EAC3,
    FOURCC_FLAC, FOURCC_MP3, FOURCC_OPUS,
};
use oxideav_rtmp::{audio_codec_id_for_tag, audio_to_packet};

/// Exact wire bytes for an Opus `SequenceStart` per
/// `enhanced-rtmp-v2.pdf` §"ExAudioTagBody":
///
/// * header byte `0x90` — `SoundFormat = ExHeader(9) << 4 |
///   AudioPacketType = SequenceStart(0)`
/// * four-byte `Opus` FourCC
/// * body = `OpusSequenceHeader` (the Opus ID header per RFC 7845 §5.1;
///   starts with the 8-byte `OpusHead` magic).
#[test]
fn opus_sequence_start_wire_bytes_round_trip_to_packet() {
    let payload: Vec<u8> = vec![
        0x90, // ExHeader=9 | SequenceStart=0
        b'O', b'p', b'u', b's', // FourCC
        b'O', b'p', b'u', b's', b'H', b'e', b'a', b'd', // RFC 7845 magic
        0x01, // Version=1
        0x02, // ChannelCount=2
        0x38, 0x01, // PreSkip=0x0138 (LE)
        0x80, 0xbb, 0x00, 0x00, // InputSampleRate=48000 (LE)
        0x00, 0x00, // OutputGain=0
        0x00, // ChannelMappingFamily=0
    ];
    let tag = parse_audio(&payload).expect("parse");
    assert_eq!(tag.sound_format, AUDIO_FORMAT_EX_HEADER);
    assert_eq!(tag.audio_fourcc, Some(FOURCC_OPUS));
    assert_eq!(tag.ex_packet_type, Some(AUDIO_PACKET_TYPE_SEQUENCE_START));
    assert_eq!(tag.body.len(), 19);
    assert!(tag.is_ex_sequence_header());

    let pkt = audio_to_packet(0, &tag);
    assert_eq!(pkt.dts, Some(0));
    assert_eq!(pkt.pts, Some(0));
    assert!(pkt.flags.header);
    // The OpusHead bytes pass through unmodified — no legacy AAC
    // marker is prepended in Enhanced mode.
    assert_eq!(pkt.data, tag.body);
    assert_eq!(audio_codec_id_for_tag(&tag).as_str(), "opus");
}

/// AC-3 / E-AC-3: `CodedFrames` only — no `SequenceStart` shape is
/// defined in v2 (AC-3 is self-describing through its own
/// synchronization-frame header). The body is "audio data as defined
/// by the bitstream syntax in the ATSC standard for Digital Audio
/// Compression". This test exercises the (very common) flat-body case.
#[test]
fn ac3_coded_frames_wire_bytes_recover_raw_frame_body() {
    let payload: Vec<u8> = vec![
        0x91, // ExHeader=9 | CodedFrames=1
        b'a', b'c', b'-', b'3', // FourCC
        0x0B, 0x77, // AC-3 sync word
        0x12, 0x34, 0x56, 0x78, // (stub CRC + bsid + bytes)
    ];
    let tag = parse_audio(&payload).expect("parse");
    assert_eq!(tag.audio_fourcc, Some(FOURCC_AC3));
    assert_eq!(tag.ex_packet_type, Some(AUDIO_PACKET_TYPE_CODED_FRAMES));
    assert_eq!(tag.body, vec![0x0B, 0x77, 0x12, 0x34, 0x56, 0x78]);

    let pkt = audio_to_packet(42, &tag);
    assert_eq!(pkt.dts, Some(42));
    assert_eq!(pkt.pts, Some(42));
    assert!(!pkt.flags.header);
    assert_eq!(pkt.data, tag.body);
    assert_eq!(audio_codec_id_for_tag(&tag).as_str(), "ac3");
}

/// E-AC-3 dispatch sanity check — same shape as AC-3 but a different
/// FourCC. Both map to ATSC-defined bitstream syntax, but downstream
/// decoders need to know which one because E-AC-3 has different frame
/// structure.
#[test]
fn eac3_dispatch_yields_eac3_codec_id() {
    let payload: Vec<u8> = vec![
        0x91, // ExHeader | CodedFrames
        b'e', b'c', b'-', b'3', 0x0B, 0x77, 0xAA, 0xBB,
    ];
    let tag = parse_audio(&payload).expect("parse");
    assert_eq!(tag.audio_fourcc, Some(FOURCC_EAC3));
    assert_eq!(audio_codec_id_for_tag(&tag).as_str(), "eac3");
}

/// FLAC `SequenceStart` body per spec: "0x66 0x4C 0x61 0x43 ('fLaC' in
/// ASCII) signature followed by a metadata block (called the STREAMINFO
/// block)". The signature is part of the body — not a separate
/// FourCC. (The Enhanced RTMP FourCC `fLaC` happens to share the same
/// four bytes, but it's a distinct field.) Verify that the parser
/// treats the in-body fLaC bytes as opaque content and the FourCC as
/// the framing discriminator.
#[test]
fn flac_sequence_start_body_includes_native_signature() {
    let body = b"fLaC\x80\x00\x00\x22\
                 \x10\x00\x10\x00\
                 \x00\x00\x00\x00\x00\x00\x00\
                 \x0a\xc4\x42\xf0\x00\x00\x00\x00\
                 \xa5\xb6\xc7\xd8\xe9\xfa\x0b\x1c\
                 \x2d\x3e\x4f\x50\x61\x72\x83\x94"
        .to_vec();
    let tag = AudioTag {
        mod_ex: Vec::new(),
        sound_format: AUDIO_FORMAT_EX_HEADER,
        sound_rate: 0,
        sound_size_16bit: false,
        stereo: false,
        aac_packet_type: None,
        ex_packet_type: Some(AUDIO_PACKET_TYPE_SEQUENCE_START),
        audio_fourcc: Some(FOURCC_FLAC),
        body: body.clone(),

        multitrack: None,
    };
    let payload = build_audio(&tag);
    assert_eq!(payload[0], 0x90);
    // Framing FourCC.
    assert_eq!(&payload[1..5], b"fLaC");
    // Body FLAC signature + STREAMINFO bytes — also start with fLaC
    // by FLAC's own spec, but at a separate offset on the wire.
    assert_eq!(&payload[5..9], b"fLaC");
    assert_eq!(payload[9], 0x80); // last-metadata-block flag + block type=STREAMINFO

    let back = parse_audio(&payload).expect("parse");
    assert_eq!(back, tag);
    let pkt = audio_to_packet(0, &back);
    assert!(pkt.flags.header);
    assert_eq!(pkt.data, body);
    assert_eq!(audio_codec_id_for_tag(&back).as_str(), "flac");
}

/// MP3 with FourCC signalling: the v2 spec adds the `.mp3` FourCC so
/// MP3 streams can be carried alongside the other FourCC codecs
/// without falling back to the legacy `SoundFormat = 2` path. Body is
/// a sequence of MPEG-1/2 Layer III frames; the framing layer treats
/// them as opaque.
#[test]
fn mp3_fourcc_coded_frames_dispatches_to_mp3_codec_id() {
    let payload: Vec<u8> = vec![
        0x91, // ExHeader | CodedFrames
        b'.', b'm', b'p', b'3', // FourCC
        0xFF, 0xFB, 0x90, 0x00, // MP3 frame sync header (stub)
        0xAA, 0xBB, 0xCC, 0xDD, // frame body (stub)
    ];
    let tag = parse_audio(&payload).expect("parse");
    assert_eq!(tag.audio_fourcc, Some(FOURCC_MP3));
    assert_eq!(audio_codec_id_for_tag(&tag).as_str(), "mp3");

    let pkt = audio_to_packet(33, &tag);
    assert_eq!(pkt.dts, Some(33));
    assert_eq!(
        pkt.data,
        vec![0xFF, 0xFB, 0x90, 0x00, 0xAA, 0xBB, 0xCC, 0xDD]
    );
    assert!(!pkt.flags.header);
}

/// SequenceEnd has an empty body and signals end of audio sequence
/// for the track. Per spec: "AudioPacketType.SequenceEnd is to have
/// no less than the same meaning as a silence message". Surface it
/// with `flags.header = true` so consumers can route it to a flush
/// boundary without trying to decode the empty payload.
#[test]
fn sequence_end_wire_bytes_round_trip_with_empty_body() {
    let payload: Vec<u8> = vec![
        0x92, // ExHeader=9 | SequenceEnd=2
        b'O', b'p', b'u', b's',
    ];
    assert_eq!(payload.len(), 5);
    let tag = parse_audio(&payload).expect("parse");
    assert_eq!(tag.ex_packet_type, Some(AUDIO_PACKET_TYPE_SEQUENCE_END));
    assert!(tag.body.is_empty());

    let pkt = audio_to_packet(60_000, &tag);
    assert!(pkt.flags.header);
    assert!(pkt.data.is_empty());
    assert_eq!(pkt.dts, Some(60_000));
}

/// Build → parse idempotence across every (FourCC, PacketType) pair the
/// framing layer recognises today. Catches accidental drift between
/// the encode and decode paths.
#[test]
fn build_parse_idempotence_across_all_known_pairs() {
    for fcc in [
        FOURCC_OPUS,
        FOURCC_FLAC,
        FOURCC_AC3,
        FOURCC_EAC3,
        FOURCC_MP3,
    ] {
        for &pt in &[
            AUDIO_PACKET_TYPE_SEQUENCE_START,
            AUDIO_PACKET_TYPE_CODED_FRAMES,
            AUDIO_PACKET_TYPE_SEQUENCE_END,
        ] {
            let body = if pt == AUDIO_PACKET_TYPE_SEQUENCE_END {
                vec![]
            } else {
                vec![0xAA, 0xBB, 0xCC, 0xDD]
            };
            let tag = AudioTag {
                mod_ex: Vec::new(),
                sound_format: AUDIO_FORMAT_EX_HEADER,
                sound_rate: 0,
                sound_size_16bit: false,
                stereo: false,
                aac_packet_type: None,
                ex_packet_type: Some(pt),
                audio_fourcc: Some(fcc),
                body,

                multitrack: None,
            };
            let bytes = build_audio(&tag);
            let parsed = parse_audio(&bytes).expect("parse");
            let rebuilt = build_audio(&parsed);
            assert_eq!(bytes, rebuilt, "fcc={fcc:?} pt={pt}");
            assert_eq!(parsed, tag, "fcc={fcc:?} pt={pt}");
        }
    }
}

/// A v2-mode publisher and a legacy publisher must never decode to
/// the same shape — the SoundFormat high nibble is the discriminator,
/// and `ExHeader = 9` is reserved in the legacy enum.
#[test]
fn legacy_and_enhanced_tags_are_disjoint() {
    // Legacy AAC raw frame: SoundFormat=10 | rate=3 | 16bit | stereo
    // = 0xAF, then AACPacketType=1 (raw), then frame body.
    let legacy: Vec<u8> = vec![0xAF, 0x01, 0xDE, 0xAD, 0xBE, 0xEF];
    let legacy_tag = parse_audio(&legacy).expect("legacy");
    assert!(!legacy_tag.is_enhanced());
    assert_eq!(audio_codec_id_for_tag(&legacy_tag).as_str(), "aac");

    // Enhanced Opus CodedFrames.
    let enhanced: Vec<u8> = vec![0x91, b'O', b'p', b'u', b's', 0xDE, 0xAD];
    let enh_tag = parse_audio(&enhanced).expect("enhanced");
    assert!(enh_tag.is_enhanced());
    assert_eq!(audio_codec_id_for_tag(&enh_tag).as_str(), "opus");

    // No accidental alias.
    assert_ne!(legacy_tag.audio_fourcc, enh_tag.audio_fourcc);
    assert_ne!(legacy_tag.sound_format, enh_tag.sound_format);
}
