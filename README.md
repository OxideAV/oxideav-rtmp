# oxideav-rtmp

Pure-Rust **RTMP** for the
[`oxideav`](https://github.com/OxideAV/oxideav) framework — accept an
incoming publisher (server / source) or push your own stream to a
remote RTMP server (client / sink). Zero external dependencies,
blocking-thread-per-connection.

## Server (accept a publisher)

```rust
use oxideav_rtmp::{RtmpServer, StreamPacket};

let server = RtmpServer::bind("0.0.0.0:1935")?;
let req = server.accept()?;                         // blocks until one publisher connects

if req.stream_name != "my-secret-key" {
    req.reject("unauthorized")?;
    return Ok(());
}

let mut session = req.accept()?;                    // sends NetStream.Publish.Start
while let Some(pkt) = session.next_packet()? {
    match pkt {
        StreamPacket::Video { timestamp, tag } => { /* AVC bytes in `tag.body` */ }
        StreamPacket::Audio { timestamp, tag } => { /* AAC bytes in `tag.body` */ }
        StreamPacket::Metadata(meta)           => { /* onMetaData AMF0 object */ }
    }
}
```

Multi-client variant — one thread per connection:

```rust
server.serve(|req| {
    if auth_ok(&req.app, &req.stream_name) {
        let session = req.accept().expect("accept");
        route(session);
    } else {
        let _ = req.reject("forbidden");
    }
})?;
```

## Client (push to a remote RTMP server)

```rust
use oxideav_rtmp::RtmpClient;

let mut client = RtmpClient::connect("rtmp://origin.example.com:1935/live/stream-key-abc")?;

client.send_video_sequence_header(&avcc_bytes)?;    // AVCDecoderConfigurationRecord
client.send_audio_sequence_header(&aac_asc)?;       // 2-byte AudioSpecificConfig

loop {
    client.send_video(ts_ms, is_keyframe, &length_prefixed_nalus)?;
    client.send_audio(ts_ms, &raw_aac_frame)?;
}

client.close()?;
```

## Scope

- **RTMP** (`rtmp://`, plain TCP port 1935). No RTMPS yet — wrap our
  `Read + Write` with rustls if you need it, or request an `rtmps`
  feature.
- **Publish direction only.** The server accepts incoming
  publishers; the client pushes to remote servers. RTMP play
  (subscribe / pull) is a follow-up.
- **AMF0 command flow.** AMF3, shared objects, RTMFP, and the
  Adobe digest-verified handshake are not implemented; they're
  essentially unused in modern ingest workflows.
- **H.264 + AAC** are the canonical legacy payloads, plus
  **Enhanced RTMP v1** (Veovera 2023) FourCC video codecs:
  `hvc1` (HEVC / H.265), `av01` (AV1), `vp09` (VP9). Sequence-start
  (HEVCDecoderConfigurationRecord / `AV1CodecConfigurationRecord` /
  `VPCodecConfigurationRecord`), `CodedFrames`, `CodedFramesX`
  (CTS=0 omitted), `SequenceEnd`, and the HDR `PacketTypeMetadata`
  (`colorInfo`) frames all round-trip via `flv::parse_video` /
  `flv::build_video`. The crate passes through FLV tag bytes
  verbatim, so additional codecs (MP3, H.263, Speex, …) keep
  working too.
- **Enhanced RTMP v2** (Veovera 2026) FourCC audio codecs:
  `Opus`, `fLaC` (FLAC), `ac-3` (AC-3), `ec-3` (E-AC-3), `.mp3`,
  `mp4a` (AAC, added FourCC signalling). `SequenceStart`,
  `CodedFrames`, and `SequenceEnd` AudioPacketTypes round-trip via
  `flv::parse_audio` / `flv::build_audio`; bodies are the per-codec
  shapes called out in `enhanced-rtmp-v2.pdf` §"ExAudioTagBody"
  (`OpusHead` ID-header per RFC 7845 §5.1 for Opus SequenceStart,
  `fLaC + STREAMINFO` per Xiph FLAC §7 for FLAC SequenceStart, ATSC
  sync frames for AC-3 / E-AC-3 CodedFrames, MPEG Layer III frames
  for MP3, ISO/IEC 14496-3 `AudioSpecificConfig` for FourCC-AAC
  SequenceStart). `Multitrack`, `MultichannelConfig`, and `ModEx`
  AudioPacketTypes parse to opaque bodies pending follow-up rounds.

## Pipeline integration (`SourceRegistry`)

Wire `rtmp://` URIs into the workspace's
[`oxideav_core::SourceRegistry`] so the pipeline executor reads
RTMP streams via the same dispatch as `file://` and `http(s)://`:

```rust
use oxideav_core::SourceRegistry;
let mut reg = SourceRegistry::new();
oxideav_rtmp::register(&mut reg);
// `rtmp://host:port/app/stream-name` opens a one-shot listener
// that accepts a single publisher and surfaces it as a
// `PacketSource` (audio = stream 0, video = stream 1, both with
// time_base 1/1000). Codec ids are auto-detected from the
// publisher's first audio + video tags (h264, hevc, av1, vp9 in
// Enhanced-RTMP-v1 FourCC mode; h264, h263, vp6f / vp6a, flashsv /
// flashsv2 for legacy single-byte codec ids; opus, flac, ac3,
// eac3, mp3, aac in Enhanced-RTMP-v2 FourCC mode; aac, mp3, pcm_*,
// speex, nellymoser for legacy single-byte audio sound-format).
let _src = reg.open("rtmp://0.0.0.0:1935/live/secret-key")?;
```

The opener is **listen-style**: each `open()` binds the URL's
`host:port`, accepts one publisher, validates the announced
`app` + `stream_name` against the URL path (rejects on
mismatch), then hands packets to the registry. For multi-client
service, keep using [`RtmpServer::serve`] directly.

## Reusable building blocks

The lower-level modules are public so callers can compose something
non-standard:

- `amf::{encode, decode, encode_command, Amf0Value}`
- `chunk::{ChunkReader, ChunkWriter, Message}`
- `handshake::{client_handshake, server_handshake}`
- `flv::{parse_video, build_video, parse_audio, build_audio}`
- `message::build_*` — builders for every protocol-control /
  command message we emit
