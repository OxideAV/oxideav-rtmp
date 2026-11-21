//! The pipeline entry point against a live black-box publisher: the
//! tool (executable name in `common::TOOL`) publishes to an `rtmp://`
//! listen-mode `PacketSource`, which must detect the codecs from the
//! stream itself and hand out every coded frame on a monotonic
//! decode timeline. Skips at runtime when the tool is not installed.

mod common;

use std::collections::HashMap;
use std::thread;
use std::time::Duration;

use common::{
    KIND_AUDIO, KIND_VIDEO, Proc, Scratch, args, free_port, read_flv, run_tool, tool_available,
};
use oxideav_core::Error as CoreError;
use oxideav_rtmp::open_rtmp;

const STEP: Duration = Duration::from_secs(40);
const VIDEO_SRC: &str = "-f lavfi -i testsrc2=size=160x120:rate=25";
const AUDIO_SRC: &str = "-f lavfi -i sine=frequency=440:sample_rate=44100";

/// (name, encoder arguments, expected codec id per stream in the
/// source's audio-first layout).
fn cases() -> Vec<(&'static str, String, Vec<&'static str>)> {
    let v = VIDEO_SRC;
    let a = AUDIO_SRC;
    vec![
        (
            "h264_aac",
            format!("{v} {a} -t 1 -c:v libx264 -preset ultrafast -pix_fmt yuv420p -c:a aac"),
            vec!["aac", "h264"],
        ),
        (
            "hevc_opus",
            format!(
                "{v} {a} -t 1 -c:v libx265 -preset ultrafast -x265-params log-level=none -c:a libopus -ar 48000"
            ),
            vec!["opus", "hevc"],
        ),
        (
            "av1_flac",
            format!("{v} {a} -t 1 -c:v libsvtav1 -preset 12 -c:a flac"),
            vec!["flac", "av1"],
        ),
        (
            "flv1_mp3",
            format!("{v} {a} -t 1 -c:v flv -c:a libmp3lame"),
            vec!["mp3", "h263"],
        ),
        (
            "vp9_ac3",
            format!("{v} {a} -t 1 -c:v libvpx-vp9 -deadline realtime -cpu-used 8 -c:a ac3"),
            vec!["ac3", "vp9"],
        ),
    ]
}

/// Tags of `kind` in the tool's FLV rendition that carry coded frames
/// (not sequence headers, configuration or end-of-sequence markers):
/// FLV v10.1 §E.4.2.1 / §E.4.3.1 for the legacy AAC / AVC framing,
/// the Enhanced RTMP packet-type nibble otherwise.
fn coded_count(tags: &[common::Tag], kind: u8) -> usize {
    tags.iter()
        .filter(|t| t.kind == kind && !t.data.is_empty())
        .filter(|t| {
            let b = t.data[0];
            if kind == KIND_VIDEO {
                if b & 0x80 != 0 {
                    matches!(b & 0x0F, 1 | 3)
                } else if b & 0x0F == 7 {
                    t.data.get(1) == Some(&1)
                } else {
                    true
                }
            } else if b >> 4 == 9 {
                b & 0x0F == 1
            } else if b >> 4 == 10 {
                t.data.get(1) == Some(&1)
            } else {
                true
            }
        })
        .count()
}

#[test]
fn listen_source_detects_codecs_and_delivers_every_frame() {
    if !tool_available() {
        eprintln!("skipping: black-box tool `{}` not available", common::TOOL);
        return;
    }
    let mut ran = 0usize;
    for (name, encode, want_codecs) in cases() {
        let dir = Scratch::new(name);
        let src = dir.file("src.nut");
        let mut a = args(&encode);
        a.extend(args("-f nut"));
        a.push(src.display().to_string());
        if run_tool(&a, &dir.file("encode.log"), STEP).is_err() {
            eprintln!("  [skip {name}] the tool cannot encode this stream here");
            continue;
        }
        let reference = dir.file("ref.flv");
        let mut a = vec!["-i".to_string(), src.display().to_string()];
        a.extend(args("-map 0 -c copy -f flv"));
        a.push(reference.display().to_string());
        if run_tool(&a, &dir.file("ref.log"), STEP).is_err() {
            eprintln!("  [skip {name}] the tool cannot write this stream as FLV here");
            continue;
        }
        let tags = read_flv(&reference);

        let port = free_port();
        let url = format!("rtmp://127.0.0.1:{port}/live/source-key");
        let listen_url = url.clone();
        let opener = thread::spawn(move || {
            let mut source = open_rtmp(&listen_url).map_err(|e| format!("open: {e}"))?;
            let streams = source.streams().to_vec();
            let mut counts: HashMap<u32, usize> = HashMap::new();
            let mut last_dts: HashMap<u32, i64> = HashMap::new();
            loop {
                match source.next_packet() {
                    Ok(p) => {
                        // Configuration and end-of-sequence markers
                        // travel as header-flagged packets.
                        if !p.flags.header {
                            *counts.entry(p.stream_index).or_default() += 1;
                        }
                        if let Some(dts) = p.dts {
                            let prev = last_dts.insert(p.stream_index, dts);
                            if prev.is_some_and(|prev| dts < prev) {
                                return Err(format!(
                                    "stream {} decode time went backwards",
                                    p.stream_index
                                ));
                            }
                        }
                    }
                    Err(CoreError::Eof) => break,
                    Err(e) => return Err(format!("next_packet: {e}")),
                }
            }
            Ok((streams, counts))
        });
        // The opener binds first, then blocks in accept.
        thread::sleep(Duration::from_millis(300));
        let mut a = vec!["-i".to_string(), src.display().to_string()];
        a.extend(args("-map 0 -c copy -f flv"));
        a.push(url);
        let mut publisher = Proc::spawn(&a, &dir.file("publish.log"));
        let status = publisher.wait(STEP);
        assert!(
            matches!(status, Some(s) if s.success()),
            "{name}: publisher failed: {}",
            publisher.log()
        );
        let (streams, counts) = opener
            .join()
            .expect("opener thread")
            .unwrap_or_else(|e| panic!("{name}: {e}"));

        let got_codecs: Vec<String> = streams
            .iter()
            .map(|s| s.params.codec_id.as_str().to_string())
            .collect();
        assert_eq!(got_codecs, want_codecs, "{name}: detected codecs");
        for s in &streams {
            let kind = if s.index == oxideav_rtmp::AUDIO_STREAM_INDEX {
                KIND_AUDIO
            } else {
                KIND_VIDEO
            };
            assert_eq!(
                counts.get(&s.index).copied().unwrap_or(0),
                coded_count(&tags, kind),
                "{name}: packets on stream {} ({})",
                s.index,
                s.params.codec_id.as_str()
            );
        }
        ran += 1;
    }
    eprintln!("packet-source interop: {ran} streams checked");
}
