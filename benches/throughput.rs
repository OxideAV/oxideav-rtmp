//! Loopback throughput + per-message allocation measurements.
//!
//! Run with `cargo bench --bench throughput`. Plain `std::time`
//! timing (no harness dependency); every figure is the median of
//! several runs. Sections:
//!
//! * `chunk-write` / `chunk-read` — the chunk-stream codec alone, on
//!   in-memory buffers (CPU cost per message, no sockets);
//! * `client-push → server-ingest` — a real [`RtmpClient`] publishing
//!   to a real [`RtmpServer`] over 127.0.0.1;
//! * `server-play → player-pull` — a [`PlaySession`] feeding an
//!   [`RtmpPlayer`] over 127.0.0.1;
//! * heap allocations per message on the ingest and push paths,
//!   counted by a wrapping global allocator.

use std::alloc::{GlobalAlloc, Layout, System};
use std::io::Cursor;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use oxideav_rtmp::chunk::{ChunkReader, ChunkWriter, Message};
use oxideav_rtmp::flv::{build_video, parse_video};
use oxideav_rtmp::{
    PlayerPacket, RtmpClient, RtmpPlayer, RtmpServer, SessionRequest, StreamPacket, VideoTag,
};

struct Counting;

static ALLOCS: AtomicU64 = AtomicU64::new(0);

// SAFETY: defers to the system allocator; only adds a counter.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
        // SAFETY: same contract as the caller's.
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: same contract as the caller's.
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
        // SAFETY: same contract as the caller's.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

const RUNS: usize = 5;

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).expect("finite"));
    v[v.len() / 2]
}

/// A coded-frame video tag whose FLV body is `size` bytes long.
fn video_tag(size: usize, key: bool) -> VideoTag {
    let body: Vec<u8> = (0..size.saturating_sub(5))
        .map(|i| (i * 31 + 7) as u8)
        .collect();
    VideoTag::default()
        .with_frame_type(if key { 1 } else { 2 })
        .with_codec_id(7)
        .with_avc_packet_type(Some(1))
        .with_body(body)
}

fn video_messages(count: usize, size: usize) -> Vec<Message> {
    (0..count)
        .map(|i| Message {
            msg_type_id: 9,
            msg_stream_id: 1,
            timestamp: (i * 40) as u32,
            payload: build_video(&video_tag(size, i % 50 == 0)),
        })
        .collect()
}

fn report(label: &str, count: usize, size: usize, secs: f64) {
    let mb = (count * size) as f64 / (1024.0 * 1024.0);
    println!(
        "{label:<44} {:>9.1} MB/s {:>11.0} msg/s",
        mb / secs,
        count as f64 / secs
    );
}

fn bench_chunk_codec(count: usize, size: usize, chunk_size: usize) {
    let msgs = video_messages(count, size);
    let mut wire = Vec::new();
    let mut w_times = Vec::new();
    for _ in 0..RUNS {
        let mut out = Vec::with_capacity(count * (size + size / chunk_size + 16));
        let t = Instant::now();
        {
            let mut w = ChunkWriter::new(&mut out);
            w.set_chunk_size(chunk_size);
            for m in &msgs {
                w.write_message(5, m).expect("write");
            }
        }
        w_times.push(t.elapsed().as_secs_f64());
        wire = out;
    }
    report(
        &format!("chunk-write {size} B msgs, chunk {chunk_size}"),
        count,
        size,
        median(w_times),
    );
    let mut r_times = Vec::new();
    for _ in 0..RUNS {
        let mut r = ChunkReader::new(Cursor::new(&wire[..]));
        r.set_chunk_size(chunk_size);
        let t = Instant::now();
        for _ in 0..count {
            let m = r.read_message().expect("read");
            std::hint::black_box(&m);
        }
        r_times.push(t.elapsed().as_secs_f64());
    }
    report(
        &format!("chunk-read  {size} B msgs, chunk {chunk_size}"),
        count,
        size,
        median(r_times),
    );

    // Allocations per message, ingest path: reassemble + parse.
    let mut r = ChunkReader::new(Cursor::new(&wire[..]));
    r.set_chunk_size(chunk_size);
    // Warm the per-csid state with the first message.
    let _ = r.read_message().expect("read");
    let before = ALLOCS.load(Ordering::Relaxed);
    for _ in 1..count {
        let m = r.read_message().expect("read");
        let tag = parse_video(&m.payload).expect("parse");
        std::hint::black_box(&tag);
    }
    let ingest = ALLOCS.load(Ordering::Relaxed) - before;
    // Allocations per message, push path: build + chunk.
    let tag = video_tag(size, false);
    let mut sink = Vec::with_capacity(count * (size + size / chunk_size + 16));
    let mut w = ChunkWriter::new(&mut sink);
    w.set_chunk_size(chunk_size);
    w.write_message(5, &msgs[0]).expect("write");
    let before = ALLOCS.load(Ordering::Relaxed);
    for (i, _) in msgs.iter().enumerate().skip(1) {
        let m = Message {
            msg_type_id: 9,
            msg_stream_id: 1,
            timestamp: (i * 40) as u32,
            payload: build_video(&tag),
        };
        w.write_message(5, &m).expect("write");
    }
    let push = ALLOCS.load(Ordering::Relaxed) - before;
    println!(
        "{:<44} ingest {:.2} allocs/msg, push {:.2} allocs/msg",
        format!("allocations {size} B msgs, chunk {chunk_size}"),
        ingest as f64 / (count - 1) as f64,
        push as f64 / (count - 1) as f64
    );
}

/// Real sockets: publish client → ingest server.
fn bench_publish_loopback(count: usize, size: usize) {
    let mut times = Vec::new();
    for _ in 0..RUNS {
        let server = RtmpServer::bind("127.0.0.1:0").expect("bind");
        let addr = server.local_addr().expect("addr");
        let ingest = thread::spawn(move || {
            let mut s = server.accept().expect("accept").accept().expect("publish");
            s.set_read_timeout(Some(Duration::from_secs(30)))
                .expect("timeout");
            let mut n = 0usize;
            while let Some(p) = s.next_packet().expect("packet") {
                if matches!(p, StreamPacket::Video { .. }) {
                    n += 1;
                }
            }
            n
        });
        let mut c = RtmpClient::connect(&format!("rtmp://{addr}/live/bench")).expect("connect");
        let key = video_tag(size, true);
        let inter = video_tag(size, false);
        let t = Instant::now();
        for i in 0..count {
            let tag = if i % 50 == 0 { &key } else { &inter };
            c.send_video_tag((i * 40) as u32, tag).expect("send");
        }
        c.close().expect("close");
        let n = ingest.join().expect("join");
        times.push(t.elapsed().as_secs_f64());
        assert_eq!(n, count, "every message must arrive");
    }
    report(
        &format!("client-push → server-ingest {size} B msgs"),
        count,
        size,
        median(times),
    );
}

/// Real sockets: play session → play client.
fn bench_play_loopback(count: usize, size: usize) {
    let mut times = Vec::new();
    for _ in 0..RUNS {
        let server = RtmpServer::bind("127.0.0.1:0").expect("bind");
        let addr = server.local_addr().expect("addr");
        let serve = thread::spawn(move || {
            let mut play = match server.accept_any().expect("accept") {
                SessionRequest::Play(r) => r.accept().expect("play"),
                SessionRequest::Publish(_) => panic!("expected a subscriber"),
            };
            let key = video_tag(size, true);
            let inter = video_tag(size, false);
            for i in 0..count {
                let tag = if i % 50 == 0 { &key } else { &inter };
                play.send_video((i * 40) as u32, tag).expect("send");
            }
            play.close().expect("close");
        });
        let mut p = RtmpPlayer::connect(&format!("rtmp://{addr}/live/bench")).expect("connect");
        p.set_read_timeout(Some(Duration::from_secs(30)))
            .expect("timeout");
        let t = Instant::now();
        let mut n = 0usize;
        while let Some(pkt) = p.next_packet().expect("packet") {
            if matches!(pkt, PlayerPacket::Video { .. }) {
                n += 1;
            }
        }
        times.push(t.elapsed().as_secs_f64());
        assert_eq!(n, count, "every message must arrive");
        serve.join().expect("join");
        let _ = p.close();
    }
    report(
        &format!("server-play → player-pull {size} B msgs"),
        count,
        size,
        median(times),
    );
}

fn main() {
    println!("oxideav-rtmp throughput (median of {RUNS} runs)");
    bench_chunk_codec(40_000, 1_200, 128);
    bench_chunk_codec(40_000, 1_200, 4_096);
    bench_chunk_codec(1_000, 131_072, 4_096);
    bench_chunk_codec(1_000, 131_072, 65_536);
    bench_publish_loopback(20_000, 1_200);
    bench_publish_loopback(600, 131_072);
    bench_play_loopback(20_000, 1_200);
    bench_play_loopback(600, 131_072);
}
