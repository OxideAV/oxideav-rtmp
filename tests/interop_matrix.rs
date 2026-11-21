//! Live black-box interop matrix.
//!
//! Each fixture is an FLV stream produced by the black-box tool
//! (executable name in `common::TOOL`), then carried over a real
//! loopback RTMP connection in eight directions:
//!
//! * `ingest` — the tool publishes to this crate's server;
//! * `pull/simple`, `pull/digest` — this crate's play client pulls
//!   from the tool's server, offering either handshake;
//! * `push/simple`, `push/digest`, `push/chunk-sizes` — this crate's
//!   publish client pushes to the tool's server, offering either
//!   handshake, or changing its chunk size every five messages;
//! * `serve/aggregate` — this crate's server serves the tool's play
//!   client Aggregate Messages;
//! * `relay` — the tool publishes to this crate's server, which fans
//!   the stream out to the tool playing from the same server.
//!
//! The received elementary streams are compared packet-for-packet
//! (bytes, timestamps, order) against the tool's own FLV rendition of
//! the same stream. The whole file skips at runtime when the tool is
//! not installed.

mod common;

use std::path::Path;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use common::{
    KIND_AUDIO, KIND_SCRIPT, KIND_VIDEO, Proc, Scratch, Tag, args, diff, free_port, media,
    read_flv, run_tool, tool_available,
};
use oxideav_rtmp::chunk::Message;
use oxideav_rtmp::flv::{build_audio, build_video, parse_audio, parse_video};
use oxideav_rtmp::relay::{KeyframeGate, LateJoinCache};
use oxideav_rtmp::{
    Amf0Value, HandshakeKind, PlayerPacket, RtmpClient, RtmpPlayer, RtmpServer, SessionRequest,
    StreamEnd, StreamPacket,
};

const STEP: Duration = Duration::from_secs(40);

const VIDEO_SRC: &str = "-f lavfi -i testsrc2=size=160x120:rate=25";
const AUDIO_SRC: &str = "-f lavfi -i sine=frequency=440:sample_rate=44100";

/// One black-box-generated stream of the matrix.
struct Fixture {
    name: &'static str,
    /// Inputs + encoder options handed to the tool.
    encode: String,
    /// Extra muxer options applied to every FLV / RTMP output.
    mux: &'static str,
    /// The tool's own server can serve this stream (see
    /// `h264_big_keyframes`).
    tool_can_serve: bool,
}

fn fx(name: &'static str, encode: String) -> Fixture {
    Fixture {
        name,
        encode,
        mux: "",
        tool_can_serve: true,
    }
}

fn fixtures() -> Vec<Fixture> {
    let v = VIDEO_SRC;
    let a = AUDIO_SRC;
    let x264 = "-c:v libx264 -preset ultrafast -pix_fmt yuv420p";
    vec![
        // --- legacy H.264 + AAC family ---------------------------------
        fx(
            "h264_aac",
            format!("{v} {a} -t 2 {x264} -g 25 -c:a aac -b:a 64k"),
        ),
        fx(
            "h264_bframes_aac",
            format!(
                "{v} {a} -t 2 -c:v libx264 -preset veryfast -bf 3 -g 50 -pix_fmt yuv420p -c:a aac"
            ),
        ),
        fx("h264_only", format!("{v} -t 2 {x264} -g 10")),
        fx("aac_only", format!("{a} -t 2 -c:a aac")),
        fx(
            "aac_stereo_48k",
            format!("{a} -t 2 -c:a aac -ac 2 -ar 48000"),
        ),
        fx("aac_5_1", format!("{a} -t 1 -c:a aac -ac 6")),
        fx(
            "h264_high10",
            format!("{v} -t 1 -c:v libx264 -preset ultrafast -pix_fmt yuv420p10le"),
        ),
        fx(
            "h264_444",
            format!("{v} -t 1 -c:v libx264 -preset ultrafast -pix_fmt yuv444p"),
        ),
        fx("h264_intra_only", format!("{v} -t 1 {x264} -g 1")),
        fx("h264_1_frame", format!("{v} -frames:v 1 {x264}")),
        fx(
            "h264_odd_size",
            format!("-f lavfi -i testsrc2=size=98x54:rate=30000/1001 -t 1 {x264}"),
        ),
        fx(
            "h264_multi_slice",
            format!("{v} {a} -t 1 {x264} -slices 4 -c:a aac"),
        ),
        // --- large key frames: many chunks per message -----------------
        fx(
            "h264_large_frames",
            format!(
                "-f lavfi -i testsrc2=size=640x360:rate=10,noise=alls=60:allf=t -t 1 {x264} -qp 8 -g 2"
            ),
        ),
        // ~1.8 MB per frame, 18 MB in one burst. The tool's *server*
        // cannot serve this one: it stops sending near the end and
        // holds the connection open, and the tool's own play client
        // hangs on it the same way — so the `pull` cells are left out
        // (ingest, push and relay still run).
        Fixture {
            name: "h264_big_keyframes",
            encode: format!(
                "-f lavfi -i testsrc2=size=1280x720:rate=10,noise=alls=60:allf=t -t 1 {x264} -qp 4 -g 2"
            ),
            mux: "",
            tool_can_serve: false,
        },
        // --- legacy single-byte codecs ---------------------------------
        fx("flv1", format!("{v} -t 1 -c:v flv")),
        fx("flashsv", format!("{v} -t 1 -c:v flashsv")),
        fx("flashsv2", format!("{v} -t 1 -c:v flashsv2")),
        fx("mp3", format!("{a} -t 2 -c:a libmp3lame")),
        fx(
            "mp3_22k_stereo",
            format!("{a} -t 1 -c:a libmp3lame -ar 22050 -ac 2"),
        ),
        fx("pcm_s16le", format!("{a} -t 1 -c:a pcm_s16le")),
        fx("pcm_u8", format!("{a} -t 1 -c:a pcm_u8 -ar 11025")),
        fx(
            "pcm_mulaw",
            format!("{a} -t 1 -c:a pcm_mulaw -ar 8000 -ac 1"),
        ),
        fx("pcm_alaw", format!("{a} -t 1 -c:a pcm_alaw -ar 8000 -ac 1")),
        fx(
            "nellymoser",
            format!("{a} -t 1 -c:a nellymoser -ar 22050 -ac 1"),
        ),
        fx("adpcm_swf", format!("{a} -t 1 -c:a adpcm_swf -ar 22050")),
        fx("flv1_mp3", format!("{v} {a} -t 2 -c:v flv -c:a libmp3lame")),
        // --- Enhanced RTMP FourCC codecs --------------------------------
        fx(
            "hevc",
            format!("{v} -t 1 -c:v libx265 -preset ultrafast -x265-params log-level=none"),
        ),
        fx(
            "hevc_aac",
            format!(
                "{v} {a} -t 1 -c:v libx265 -preset ultrafast -x265-params log-level=none -c:a aac"
            ),
        ),
        fx("av1", format!("{v} -t 1 -c:v libsvtav1 -preset 12")),
        fx(
            "vp9",
            format!("{v} -t 1 -c:v libvpx-vp9 -deadline realtime -cpu-used 8"),
        ),
        fx("opus", format!("{a} -t 1 -c:a libopus -ar 48000")),
        fx("opus_5_1", format!("{a} -t 1 -c:a libopus -ar 48000 -ac 6")),
        fx("flac", format!("{a} -t 1 -c:a flac")),
        fx("ac3", format!("{a} -t 1 -c:a ac3")),
        fx("eac3", format!("{a} -t 1 -c:a eac3")),
        fx(
            "av1_opus",
            format!("{v} {a} -t 1 -c:v libsvtav1 -preset 12 -c:a libopus -ar 48000"),
        ),
        // --- Enhanced RTMP v2 multitrack --------------------------------
        fx(
            "two_audio_tracks",
            format!("{v} {a} {a} -map 0 -map 1 -map 2 -t 1 {x264} -c:a aac"),
        ),
        fx(
            "two_video_tracks",
            format!("{v} {v} {a} -map 0 -map 1 -map 2 -t 1 {x264} -c:a aac"),
        ),
        fx(
            "mixed_codec_tracks",
            format!(
                "{v} {a} {a} -map 0 -map 1 -map 2 -t 1 {x264} -c:a:0 aac -c:a:1 libopus -ar:a:1 48000"
            ),
        ),
        // --- long run crossing the 24-bit timestamp boundary ------------
        Fixture {
            name: "ext_timestamp_crossing",
            encode: format!("{v} {a} -t 4 {x264} -g 25 -c:a aac"),
            mux: "-output_ts_offset 16775.5",
            tool_can_serve: true,
        },
        Fixture {
            name: "ext_timestamp_far",
            encode: format!("{v} {a} -t 1 {x264} -c:a aac"),
            mux: "-output_ts_offset 2000000",
            tool_can_serve: true,
        },
        fx("long_run_aac", format!("{a} -t 120 -c:a aac -b:a 32k")),
    ]
}

/// Encode `f` once and derive the canonical FLV rendition the tool
/// itself produces when it copies that stream into an FLV output.
/// `None` when the tool on this machine cannot produce the fixture
/// (missing encoder, older writer) — the cell is then skipped.
fn make_fixture(dir: &Scratch, f: &Fixture) -> Option<std::path::PathBuf> {
    // Encode once into a neutral container; a few legacy codecs only
    // fit FLV itself.
    let mut src = None;
    let mut last_err = String::new();
    for container in ["nut", "flv"] {
        let path = dir.file(&format!("src.{container}"));
        let mut a = args(&f.encode);
        a.extend(args(&format!("-f {container}")));
        a.push(path.display().to_string());
        match run_tool(&a, &dir.file("encode.log"), STEP) {
            Ok(_) => {
                src = Some(path);
                break;
            }
            Err(e) => last_err = e,
        }
    }
    let Some(src) = src else {
        eprintln!(
            "  [skip {}] cannot encode: {}",
            f.name,
            first_line(&last_err)
        );
        return None;
    };
    let reference = dir.file("ref.flv");
    let mut a = vec!["-i".to_string(), src.display().to_string()];
    a.extend(args("-map 0 -c copy"));
    a.extend(args(f.mux));
    a.extend(args("-f flv"));
    a.push(reference.display().to_string());
    if let Err(e) = run_tool(&a, &dir.file("ref.log"), STEP) {
        eprintln!(
            "  [skip {}] tool cannot write FLV: {}",
            f.name,
            first_line(&e)
        );
        return None;
    }
    Some(src)
}

/// Lines the tool prints on a healthy run: its FLV writer cannot seek
/// back on a network output to patch the duration, and its reader
/// reports the peer's end-of-stream socket close as an I/O error (it
/// prints the same line when both ends are the tool itself).
const BENIGN: [&str; 6] = [
    "Failed to update header with correct duration",
    "Failed to update header with correct filesize",
    "Error during demuxing: Input/output error",
    // Advisory notes about the fixture itself, not about the session.
    "Guessed Channel Layout",
    "more than one audio stream is not supported by most flv demuxers",
    "more than one video stream is not supported by most flv demuxers",
];

/// The tool's stderr minus the benign end-of-stream lines.
fn warnings(log: &str) -> String {
    log.lines()
        .filter(|l| !l.trim().is_empty() && !BENIGN.iter().any(|b| l.contains(b)))
        .collect::<Vec<_>>()
        .join(" | ")
}

fn first_line(s: &str) -> String {
    s.lines().next().unwrap_or("").chars().take(160).collect()
}

/// Arguments that make the tool copy `src` to `dest` as FLV.
fn copy_args(src: &Path, f: &Fixture, dest: &str) -> Vec<String> {
    let mut a = vec!["-i".to_string(), src.display().to_string()];
    a.extend(args("-map 0 -c copy"));
    a.extend(args(f.mux));
    a.extend(args("-f flv"));
    a.push(dest.to_string());
    a
}

/// What our server saw from one publisher.
struct Ingest {
    tags: Vec<Tag>,
    metadata: Vec<Amf0Value>,
    end: Option<StreamEnd>,
    handshake: HandshakeKind,
}

fn packet_to_tag(pkt: &StreamPacket) -> Option<Tag> {
    match pkt {
        StreamPacket::Video { timestamp, tag } => Some(Tag {
            kind: KIND_VIDEO,
            ts: *timestamp,
            data: build_video(tag),
        }),
        StreamPacket::Audio { timestamp, tag } => Some(Tag {
            kind: KIND_AUDIO,
            ts: *timestamp,
            data: build_audio(tag),
        }),
        _ => None,
    }
}

/// Accept one publisher on `server` and collect everything it sends.
fn ingest_thread(server: RtmpServer) -> mpsc::Receiver<std::result::Result<Ingest, String>> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let run = || -> std::result::Result<Ingest, String> {
            let req = server.accept().map_err(|e| format!("accept: {e}"))?;
            let handshake = req.handshake_kind();
            let mut session = req.accept().map_err(|e| format!("accept publish: {e}"))?;
            session
                .set_read_timeout(Some(Duration::from_secs(20)))
                .map_err(|e| e.to_string())?;
            let mut out = Ingest {
                tags: Vec::new(),
                metadata: Vec::new(),
                end: None,
                handshake,
            };
            loop {
                match session.next_packet() {
                    Ok(Some(pkt)) => {
                        if let Some(t) = packet_to_tag(&pkt) {
                            out.tags.push(t);
                        } else if let StreamPacket::Metadata(m) = pkt {
                            out.metadata.push(m);
                        }
                    }
                    Ok(None) => {
                        out.end = session.end_reason();
                        break;
                    }
                    Err(e) => {
                        return Err(format!("next_packet after {} tags: {e}", out.tags.len()));
                    }
                }
            }
            let _ = session.close();
            Ok(out)
        };
        let _ = tx.send(run());
    });
    rx
}

/// `ingest`: the tool publishes the fixture to our server.
fn cell_ingest(dir: &Scratch, f: &Fixture, src: &Path) -> std::result::Result<(), String> {
    let expected = media(&read_flv(&dir.file("ref.flv")));
    let server = RtmpServer::bind("127.0.0.1:0").map_err(|e| e.to_string())?;
    let port = server.local_addr().map_err(|e| e.to_string())?.port();
    let rx = ingest_thread(server);
    let url = format!("rtmp://127.0.0.1:{port}/live/matrix-key");
    let mut publisher = Proc::spawn(&copy_args(src, f, &url), &dir.file("ingest.log"));
    let status = publisher.wait(STEP);
    let got = rx
        .recv_timeout(STEP)
        .map_err(|_| format!("server never finished; tool log: {}", publisher.log()))??;
    match status {
        Some(s) if s.success() => {}
        other => return Err(format!("tool exit {other:?}: {}", publisher.log())),
    }
    let log = warnings(&publisher.log());
    if !log.is_empty() {
        return Err(format!("tool warned: {}", first_line(&log)));
    }
    if got.end != Some(StreamEnd::Signalled) {
        return Err(format!("stream end not signalled: {:?}", got.end));
    }
    // The tool's client opens with a digested C1, so the server must
    // have taken the digest path (digested S1 + chained S2). The tool
    // answers with a plain echo C2, hence no claim about
    // `peer_response_verified` here.
    if !matches!(got.handshake, HandshakeKind::Digest { .. }) {
        return Err(format!("digest C1 not detected: {:?}", got.handshake));
    }
    if got.metadata.is_empty() {
        return Err("no onMetaData surfaced".into());
    }
    if let Some(d) = diff(&expected, &got.tags) {
        return Err(d);
    }
    Ok(())
}

/// Connect a publish client to a tool server that may still be
/// starting up: retry only while the connection is refused.
fn connect_retry<T>(
    mut connect: impl FnMut() -> oxideav_rtmp::Result<T>,
) -> std::result::Result<T, String> {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        match connect() {
            Ok(c) => return Ok(c),
            Err(oxideav_rtmp::Error::Io(e))
                if e.kind() == std::io::ErrorKind::ConnectionRefused
                    && Instant::now() < deadline =>
            {
                thread::sleep(Duration::from_millis(40));
            }
            Err(e) => return Err(format!("connect: {e}")),
        }
    }
}

/// `pull`: the tool serves the fixture, our play client pulls it.
fn cell_pull(
    dir: &Scratch,
    f: &Fixture,
    src: &Path,
    digest: bool,
) -> std::result::Result<(), String> {
    let expected = media(&read_flv(&dir.file("ref.flv")));
    let port = free_port();
    let url = format!("rtmp://127.0.0.1:{port}/live/matrix-key");
    let mut a = vec!["-i".to_string(), src.display().to_string()];
    a.extend(args("-map 0 -c copy"));
    a.extend(args(f.mux));
    a.extend(args("-f flv -listen 1"));
    a.push(url.clone());
    let mut server = Proc::spawn(&a, &dir.file("pull.log"));
    let opts = oxideav_rtmp::PlayOptions::default().with_digest_handshake(digest);
    let mut player = connect_retry(|| RtmpPlayer::connect_with_options(&url, &opts))
        .map_err(|e| format!("{e}; tool log: {}", server.log()))?;
    // The tool's server pushes the whole stream without pacing, so a
    // few silent seconds mean it has stopped sending.
    player
        .set_read_timeout(Some(Duration::from_secs(4)))
        .map_err(|e| e.to_string())?;
    let mut got = Vec::new();
    let mut metadata = 0usize;
    loop {
        match player.next_packet() {
            Ok(Some(PlayerPacket::Video { timestamp, tag })) => got.push(Tag {
                kind: KIND_VIDEO,
                ts: timestamp,
                data: build_video(&tag),
            }),
            Ok(Some(PlayerPacket::Audio { timestamp, tag })) => got.push(Tag {
                kind: KIND_AUDIO,
                ts: timestamp,
                data: build_audio(&tag),
            }),
            Ok(Some(PlayerPacket::Metadata(_))) => metadata += 1,
            Ok(Some(_)) => {}
            Ok(None) => break,
            Err(e) => return Err(format!("next_packet after {} tags: {e}", got.len())),
        }
    }
    // The tool's server ends a stream by closing its socket. When it
    // does so with our last Acknowledgement still unread, the close
    // turns into a reset, and a reset may discard the stream's
    // closing 5-byte end-of-sequence marker in flight. Every coded
    // frame must still have arrived.
    let reset = player.end_reason() == Some(StreamEnd::ConnectionReset);
    let _ = player.close();
    let mut expected = expected;
    if reset
        && got.len() + 1 == expected.len()
        && expected.last().is_some_and(|t| t.data.len() <= 5)
    {
        expected.pop();
    }
    let _ = server.wait(Duration::from_secs(10));
    if metadata == 0 {
        return Err("no onMetaData surfaced".into());
    }
    if let Some(d) = diff(&expected, &got) {
        return Err(d);
    }
    Ok(())
}

/// `push`: our publish client pushes the fixture's tags to the tool's
/// server, which records them.
/// How the `push` cell frames the stream on the wire.
#[derive(Clone, Copy, PartialEq, Eq)]
enum PushMode {
    /// One message per tag, simple handshake.
    Simple,
    /// One message per tag, digest handshake offered.
    Digest,
    /// Set Chunk Size changed every few messages.
    ChunkSizes,
}

fn cell_push(dir: &Scratch, mode: PushMode) -> std::result::Result<(), String> {
    let digest = mode == PushMode::Digest;
    let reference = read_flv(&dir.file("ref.flv"));
    let port = free_port();
    let url = format!("rtmp://127.0.0.1:{port}/live/matrix-key");
    let recorded = dir.file("push_out.flv");
    let mut a = args("-listen 1 -i");
    a.push(url.clone());
    a.extend(args("-map 0 -c copy -copyts -f flv"));
    a.push(recorded.display().to_string());
    let mut server = Proc::spawn(&a, &dir.file("push.log"));
    let mut client = connect_retry(|| {
        if digest {
            RtmpClient::connect_with_digest_handshake(
                &url,
                "live",
                &oxideav_rtmp::ConnectCapabilities::default(),
            )
        } else {
            RtmpClient::connect(&url)
        }
    })
    .map_err(|e| format!("{e}; tool log: {}", server.log()))?;
    for (i, t) in reference.iter().enumerate() {
        if mode == PushMode::ChunkSizes && i % 5 == 0 {
            let sizes = [128u32, 65_536, 1_000, 4_096, 1, 300_000];
            client
                .set_chunk_size(sizes[(i / 5) % sizes.len()])
                .map_err(|e| format!("set_chunk_size: {e}"))?;
        }
        let r = match t.kind {
            KIND_VIDEO => parse_video(&t.data).and_then(|tag| client.send_video_tag(t.ts, &tag)),
            KIND_AUDIO => parse_audio(&t.data).and_then(|tag| client.send_audio_tag(t.ts, &tag)),
            KIND_SCRIPT => {
                let vals = oxideav_rtmp::amf::decode_all(&t.data).map_err(|e| e.to_string())?;
                match vals.into_iter().nth(1) {
                    Some(v) => client.send_metadata(v),
                    None => Ok(()),
                }
            }
            _ => Ok(()),
        };
        r.map_err(|e| format!("send: {e}; tool log: {}", server.log()))?;
    }
    client.close().map_err(|e| format!("close: {e}"))?;
    match server.wait(STEP) {
        Some(s) if s.success() => {}
        other => return Err(format!("tool exit {other:?}: {}", server.log())),
    }
    let log = warnings(&server.log());
    if !log.is_empty() {
        return Err(format!("tool warned: {}", first_line(&log)));
    }
    let expected = canonical_copyts(dir, &dir.file("ref.flv"), "ref_canon.flv")?;
    let got = media(&read_flv(&recorded));
    if let Some(d) = diff(&expected, &got) {
        return Err(d);
    }
    Ok(())
}

fn canonical_copyts(
    dir: &Scratch,
    input: &Path,
    name: &str,
) -> std::result::Result<Vec<Tag>, String> {
    let out = dir.file(name);
    let mut a = vec!["-i".to_string(), input.display().to_string()];
    a.extend(args("-map 0 -c copy -copyts -f flv"));
    a.push(out.display().to_string());
    run_tool(&a, &dir.file("canonical.log"), STEP)?;
    Ok(media(&read_flv(&out)))
}

/// `serve/aggregate`: our server serves the fixture to the tool's
/// play client with every media message carried in Aggregate
/// Messages (§7.1.6) — single sub-messages for the stream's first
/// dozen frames, then four per aggregate.
fn cell_serve_aggregate(dir: &Scratch) -> std::result::Result<(), String> {
    let reference = read_flv(&dir.file("ref.flv"));
    let server = RtmpServer::bind("127.0.0.1:0").map_err(|e| e.to_string())?;
    let port = server.local_addr().map_err(|e| e.to_string())?.port();
    let url = format!("rtmp://127.0.0.1:{port}/live/matrix-key");
    let (tx, rx) = mpsc::channel::<std::result::Result<(), String>>();
    thread::spawn(move || {
        let run = || -> std::result::Result<(), String> {
            let mut play = match server.accept_any().map_err(|e| e.to_string())? {
                SessionRequest::Play(req) => req.accept().map_err(|e| e.to_string())?,
                SessionRequest::Publish(_) => return Err("expected a subscriber".into()),
            };
            play.set_chunk_size(60_000).map_err(|e| e.to_string())?;
            let mut pending: Vec<Message> = Vec::new();
            let mut sent_media = 0usize;
            for t in &reference {
                if t.kind == KIND_SCRIPT {
                    let vals = oxideav_rtmp::amf::decode_all(&t.data).map_err(|e| e.to_string())?;
                    if let Some(v) = vals.into_iter().nth(1) {
                        play.send_metadata(&v).map_err(|e| e.to_string())?;
                    }
                    continue;
                }
                pending.push(Message {
                    msg_type_id: t.kind,
                    msg_stream_id: 0,
                    timestamp: t.ts,
                    payload: t.data.clone(),
                });
                sent_media += 1;
                // The first few go out one per aggregate: while the
                // tool is still discovering the stream it stamps the
                // leading frames of a multi-message aggregate with
                // earlier timestamps (bytes intact; never seen once
                // the stream is running, nor with one sub-message per
                // aggregate), which would only blur the comparison.
                if sent_media <= 12 || pending.len() == 4 {
                    play.send_aggregate(&pending).map_err(|e| e.to_string())?;
                    pending.clear();
                }
            }
            play.send_aggregate(&pending).map_err(|e| e.to_string())?;
            play.close().map_err(|e| e.to_string())
        };
        let _ = tx.send(run());
    });
    let recorded = dir.file("aggregate_out.flv");
    let mut a = args("-i");
    a.push(url);
    a.extend(args("-map 0 -c copy -copyts -f flv"));
    a.push(recorded.display().to_string());
    let mut subscriber = Proc::spawn(&a, &dir.file("aggregate.log"));
    rx.recv_timeout(STEP)
        .map_err(|_| format!("server never finished; tool log: {}", subscriber.log()))??;
    match subscriber.wait(STEP) {
        Some(s) if s.success() => {}
        other => return Err(format!("subscriber exit {other:?}: {}", subscriber.log())),
    }
    let log = warnings(&subscriber.log());
    if !log.is_empty() {
        return Err(format!("subscriber warned: {}", first_line(&log)));
    }
    let expected = canonical_copyts(dir, &dir.file("ref.flv"), "ref_canon.flv")?;
    let got = media(&read_flv(&recorded));
    // Four sub-messages reach the tool at once, and its recorder
    // orders equal-timestamp audio and video by arrival — so the two
    // elementary streams are compared separately (each exact).
    for kind in [KIND_VIDEO, KIND_AUDIO] {
        let pick = |tags: &[Tag]| -> Vec<Tag> {
            tags.iter().filter(|t| t.kind == kind).cloned().collect()
        };
        if let Some(d) = diff(&pick(&expected), &pick(&got)) {
            return Err(format!("kind {kind}: {d}"));
        }
    }
    Ok(())
}

/// `relay`: the tool publishes to our server; our server forwards
/// every packet to the tool playing from the same server.
fn cell_relay(dir: &Scratch, f: &Fixture, src: &Path) -> std::result::Result<(), String> {
    let server = RtmpServer::bind("127.0.0.1:0").map_err(|e| e.to_string())?;
    let port = server.local_addr().map_err(|e| e.to_string())?.port();
    let url = format!("rtmp://127.0.0.1:{port}/live/matrix-key");
    let (tx, rx) = mpsc::channel::<std::result::Result<usize, String>>();
    thread::spawn(move || {
        let run = || -> std::result::Result<usize, String> {
            // The subscriber connects first and waits for the stream.
            let mut play = match server.accept_any().map_err(|e| e.to_string())? {
                SessionRequest::Play(req) => req.accept().map_err(|e| e.to_string())?,
                SessionRequest::Publish(_) => return Err("expected the subscriber first".into()),
            };
            let mut publish = match server.accept_any().map_err(|e| e.to_string())? {
                SessionRequest::Publish(req) => req.accept().map_err(|e| e.to_string())?,
                SessionRequest::Play(_) => return Err("expected the publisher second".into()),
            };
            publish
                .set_read_timeout(Some(Duration::from_secs(20)))
                .map_err(|e| e.to_string())?;
            let mut n = 0usize;
            while let Some(pkt) = publish.next_packet().map_err(|e| format!("ingest: {e}"))? {
                play.forward(&pkt).map_err(|e| format!("forward: {e}"))?;
                n += 1;
            }
            play.close().map_err(|e| format!("close play: {e}"))?;
            let _ = publish.close();
            Ok(n)
        };
        let _ = tx.send(run());
    });

    let recorded = dir.file("relay_out.flv");
    let mut a = args("-i");
    a.push(url.clone());
    a.extend(args("-map 0 -c copy -copyts -f flv"));
    a.push(recorded.display().to_string());
    let mut subscriber = Proc::spawn(&a, &dir.file("relay_play.log"));
    // Give the subscriber time to reach `play` before the publisher
    // dials in (the relay thread accepts them in that order).
    thread::sleep(Duration::from_millis(400));
    let mut publisher = Proc::spawn(&copy_args(src, f, &url), &dir.file("relay_pub.log"));
    let pub_status = publisher.wait(STEP);
    let forwarded = rx.recv_timeout(STEP).map_err(|_| {
        format!(
            "relay never finished; publisher log: {} subscriber log: {}",
            publisher.log(),
            subscriber.log()
        )
    })??;
    if !matches!(pub_status, Some(s) if s.success()) {
        return Err(format!(
            "publisher exit {pub_status:?}: {}",
            publisher.log()
        ));
    }
    match subscriber.wait(STEP) {
        Some(s) if s.success() => {}
        other => return Err(format!("subscriber exit {other:?}: {}", subscriber.log())),
    }
    let log = warnings(&subscriber.log());
    if !log.is_empty() {
        return Err(format!("subscriber warned: {}", first_line(&log)));
    }
    if forwarded == 0 {
        return Err("nothing forwarded".into());
    }
    let expected = canonical_copyts(dir, &dir.file("ref.flv"), "ref_canon.flv")?;
    let got = media(&read_flv(&recorded));
    if let Some(d) = diff(&expected, &got) {
        return Err(d);
    }
    Ok(())
}

#[test]
fn black_box_interop_matrix() {
    if !tool_available() {
        eprintln!("skipping: black-box tool `{}` not available", common::TOOL);
        return;
    }
    let only = std::env::var("OXIDEAV_RTMP_INTEROP_ONLY").ok();
    let mut cells = 0usize;
    let mut passed = 0usize;
    let mut skipped = 0usize;
    let mut failures: Vec<String> = Vec::new();
    for f in fixtures() {
        if let Some(o) = &only
            && !f.name.contains(o.as_str())
        {
            continue;
        }
        let dir = Scratch::new(f.name);
        let Some(src) = make_fixture(&dir, &f) else {
            skipped += 1;
            continue;
        };
        let mut runs: Vec<(&str, std::result::Result<(), String>)> =
            vec![("ingest", cell_ingest(&dir, &f, &src))];
        if f.tool_can_serve {
            runs.push(("pull/simple", cell_pull(&dir, &f, &src, false)));
            runs.push(("pull/digest", cell_pull(&dir, &f, &src, true)));
        }
        runs.push(("push/simple", cell_push(&dir, PushMode::Simple)));
        runs.push(("push/digest", cell_push(&dir, PushMode::Digest)));
        runs.push(("push/chunk-sizes", cell_push(&dir, PushMode::ChunkSizes)));
        runs.push(("serve/aggregate", cell_serve_aggregate(&dir)));
        runs.push(("relay", cell_relay(&dir, &f, &src)));
        for (dirn, r) in runs {
            cells += 1;
            match r {
                Ok(()) => passed += 1,
                Err(e) => {
                    eprintln!("  FAIL {}/{dirn}: {e}", f.name);
                    failures.push(format!("{}/{dirn}: {e}", f.name));
                }
            }
        }
    }
    eprintln!(
        "interop matrix: {passed}/{cells} cells passed, {skipped} fixtures skipped (tool cannot produce them)"
    );
    assert!(
        failures.is_empty(),
        "{} of {cells} interop cells failed:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

/// A paced ("live") H.264 + AAC stream: 3 s, a key frame every 0.4 s.
fn live_fixture(dir: &Scratch) -> Option<std::path::PathBuf> {
    make_fixture(
        dir,
        &fx(
            "live_h264_aac",
            format!(
                "{VIDEO_SRC} {AUDIO_SRC} -t 3 -c:v libx264 -preset ultrafast -pix_fmt yuv420p -g 10 -c:a aac"
            ),
        ),
    )
}

/// Arguments that make the tool publish `src` at its native frame
/// rate, like a live encoder.
fn live_publish_args(src: &Path, url: &str) -> Vec<String> {
    let mut a = args("-re -i");
    a.push(src.display().to_string());
    a.extend(args("-map 0 -c copy -f flv"));
    a.push(url.to_string());
    a
}

/// Coded frames only: drops sequence headers and end-of-sequence
/// markers of the legacy AVC / AAC framing (FLV v10.1 §E.4.3.1 /
/// §E.4.2.1), which a recorder re-creates on its own.
fn coded(tags: &[Tag], kind: u8) -> Vec<Tag> {
    tags.iter()
        .filter(|t| t.kind == kind && t.data.len() > 1 && t.data[1] == 1)
        .cloned()
        .collect()
}

/// A publisher that is killed mid-stream must end the ingest promptly,
/// with everything received so far intact and the end reported as a
/// lost connection rather than a signalled close.
#[test]
fn abrupt_publisher_disconnect_is_reported() {
    if !tool_available() {
        eprintln!("skipping: black-box tool `{}` not available", common::TOOL);
        return;
    }
    let dir = Scratch::new("abrupt");
    let Some(src) = live_fixture(&dir) else {
        return;
    };
    let expected = media(&read_flv(&dir.file("ref.flv")));
    let server = RtmpServer::bind("127.0.0.1:0").expect("bind");
    let port = server.local_addr().expect("addr").port();
    let rx = ingest_thread(server);
    let url = format!("rtmp://127.0.0.1:{port}/live/abrupt-key");
    let mut publisher = Proc::spawn(&live_publish_args(&src, &url), &dir.file("abrupt.log"));
    thread::sleep(Duration::from_millis(1200));
    publisher.kill();
    let killed = Instant::now();
    let got = rx
        .recv_timeout(Duration::from_secs(10))
        .expect("ingest must end after the publisher dies")
        .expect("ingest result");
    assert!(
        killed.elapsed() < Duration::from_secs(5),
        "the lost connection must surface promptly"
    );
    assert!(
        matches!(
            got.end,
            Some(StreamEnd::PeerClosed | StreamEnd::Truncated | StreamEnd::ConnectionReset)
        ),
        "a killed publisher is not a signalled end: {:?}",
        got.end
    );
    assert!(!got.tags.is_empty(), "something arrived before the kill");
    assert!(
        got.tags.len() < expected.len(),
        "the stream was cut short ({} of {})",
        got.tags.len(),
        expected.len()
    );
    assert!(
        diff(&expected[..got.tags.len()], &got.tags).is_none(),
        "what arrived is an intact prefix of the stream"
    );
}

/// The tool joins a live relay late and records it. `group_cache`
/// selects whether the relay replays the current group of pictures.
fn late_join(group_cache: bool) {
    if !tool_available() {
        eprintln!("skipping: black-box tool `{}` not available", common::TOOL);
        return;
    }
    let dir = Scratch::new(if group_cache { "late-gop" } else { "late" });
    let Some(src) = live_fixture(&dir) else {
        return;
    };
    let server = RtmpServer::bind("127.0.0.1:0").expect("bind");
    let port = server.local_addr().expect("addr").port();
    let url = format!("rtmp://127.0.0.1:{port}/live/late-key");

    // Acceptor: hands the publisher and any subscriber to the relay.
    let (pub_tx, pub_rx) = mpsc::channel();
    let (sub_tx, sub_rx) = mpsc::channel();
    thread::spawn(move || {
        while let Ok(req) = server.accept_any() {
            let sent = match req {
                SessionRequest::Publish(r) => r.accept().map(|s| pub_tx.send(s).is_ok()),
                SessionRequest::Play(r) => r.accept().map(|s| sub_tx.send(s).is_ok()),
            };
            if !matches!(sent, Ok(true)) {
                break;
            }
        }
    });
    // Relay: publisher → cache → every subscriber through its gate.
    let (done_tx, done_rx) = mpsc::channel::<std::result::Result<(usize, usize), String>>();
    thread::spawn(move || {
        let run = || -> std::result::Result<(usize, usize), String> {
            let mut publish = pub_rx
                .recv_timeout(STEP)
                .map_err(|_| "no publisher".to_string())?;
            publish
                .set_read_timeout(Some(Duration::from_secs(20)))
                .map_err(|e| e.to_string())?;
            let mut cache = LateJoinCache::new();
            if group_cache {
                cache = cache.with_group_cache(8 << 20);
            }
            let mut subs: Vec<(oxideav_rtmp::PlaySession, KeyframeGate)> = Vec::new();
            let mut seen = 0usize;
            let mut seen_at_join = 0usize;
            while let Some(pkt) = publish.next_packet().map_err(|e| format!("ingest: {e}"))? {
                while let Ok(mut play) = sub_rx.try_recv() {
                    let (start, gate) = cache.join();
                    for p in &start {
                        play.forward(p).map_err(|e| format!("prime: {e}"))?;
                    }
                    seen_at_join = seen;
                    subs.push((play, gate));
                }
                cache.observe(&pkt);
                seen += 1;
                for (play, gate) in &mut subs {
                    if gate.admit(&pkt) {
                        play.forward(&pkt).map_err(|e| format!("forward: {e}"))?;
                    }
                }
            }
            for (play, _) in subs {
                play.close().map_err(|e| format!("close: {e}"))?;
            }
            Ok((seen, seen_at_join))
        };
        let _ = done_tx.send(run());
    });

    let mut publisher = Proc::spawn(&live_publish_args(&src, &url), &dir.file("late_pub.log"));
    thread::sleep(Duration::from_millis(1300));
    let recorded = dir.file("late.flv");
    let mut a = args("-i");
    a.push(url);
    a.extend(args("-map 0 -c copy -copyts -f flv"));
    a.push(recorded.display().to_string());
    let mut subscriber = Proc::spawn(&a, &dir.file("late_sub.log"));

    let status = publisher.wait(STEP);
    assert!(
        matches!(status, Some(s) if s.success()),
        "publisher failed: {}",
        publisher.log()
    );
    let (seen, seen_at_join) = done_rx
        .recv_timeout(STEP)
        .expect("relay finished")
        .expect("relay result");
    assert!(
        seen_at_join > 10 && seen_at_join < seen,
        "the subscriber must have joined mid-stream ({seen_at_join} of {seen})"
    );
    let status = subscriber.wait(STEP);
    assert!(
        matches!(status, Some(s) if s.success()),
        "subscriber failed: {}",
        subscriber.log()
    );
    assert_eq!(warnings(&subscriber.log()), "", "subscriber warnings");

    // The recording must be a clean tail of the stream: for each
    // elementary stream a contiguous run of the original coded frames
    // up to the last one, the video starting on a key frame.
    let expected = canonical_copyts(&dir, &dir.file("ref.flv"), "ref_canon.flv").expect("canon");
    let got = read_flv(&recorded);
    for kind in [KIND_VIDEO, KIND_AUDIO] {
        let want = coded(&expected, kind);
        let have = coded(&got, kind);
        assert!(!have.is_empty(), "kind {kind}: nothing recorded");
        assert!(
            have.len() < want.len(),
            "kind {kind}: a late joiner cannot have the whole stream"
        );
        let tail = &want[want.len() - have.len()..];
        assert!(
            diff(tail, &have).is_none(),
            "kind {kind}: recording is not the stream's tail: {:?}",
            diff(tail, &have)
        );
    }
    let first_video = &coded(&got, KIND_VIDEO)[0];
    assert_eq!(
        first_video.data[0] >> 4,
        1,
        "playback starts on a key frame"
    );
    // And the tool decodes the whole recording without complaint.
    let mut a = args("-v error -i");
    a.push(recorded.display().to_string());
    a.extend(args("-f null -"));
    let log = run_tool(&a, &dir.file("decode.log"), STEP).expect("decode recording");
    assert_eq!(log.trim(), "", "decode errors in the late-join recording");
}

#[test]
fn late_join_starts_at_next_key_frame() {
    late_join(false);
}

#[test]
fn late_join_with_group_cache_starts_immediately() {
    late_join(true);
}
