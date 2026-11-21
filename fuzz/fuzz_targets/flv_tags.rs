#![no_main]

//! FLV tag bodies and the FLV file reader.
//!
//! `parse_video` / `parse_audio` over arbitrary bytes (legacy and
//! Enhanced RTMP framings, ModEx preludes, Multitrack bodies). An
//! accepted body must survive parse → build → parse → build with
//! identical bytes on the second and third leg, and every typed
//! accessor must be callable on it. The same input is also read as an
//! FLV byte stream under a small per-tag cap.

use std::io::Cursor;

use libfuzzer_sys::fuzz_target;
use oxideav_rtmp::flv::{build_audio, build_video, parse_audio, parse_video};
use oxideav_rtmp::{FlvReader, parse_audio_message};

fuzz_target!(|data: &[u8]| {
    if let Ok(tag) = parse_video(data) {
        let _ = tag.video_command();
        let _ = tag.is_command();
        let _ = tag.color_info();
        let _ = tag.timestamp_offset_nano();
        let _ = tag.is_ex_sequence_end();
        let _ = tag.is_ex_mpeg2ts_sequence_start();
        let _ = tag.mpeg2ts_video_descriptor();
        let _ = tag.demux_tracks();
        let first = build_video(&tag);
        let again = parse_video(&first).expect("own video tag must parse");
        assert_eq!(build_video(&again), first, "video tag is not a fixed point");
    }
    if let Ok(tag) = parse_audio(data) {
        let _ = tag.multichannel_config();
        let _ = tag.timestamp_offset_nano();
        let _ = tag.is_ex_sequence_end();
        let _ = tag.demux_tracks();
        let first = build_audio(&tag);
        let again = parse_audio(&first).expect("own audio tag must parse");
        assert_eq!(build_audio(&again), first, "audio tag is not a fixed point");
    }
    let _ = parse_audio_message(data);

    if let Ok(mut reader) = FlvReader::with_max_tag_size(Cursor::new(data), 1 << 16) {
        for _ in 0..=data.len() / 11 {
            match reader.read_tag() {
                Ok(Some(_)) => {}
                Ok(None) | Err(_) => break,
            }
        }
    }
});
