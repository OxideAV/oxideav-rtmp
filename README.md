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
- **H.264 + AAC** are the expected payloads. The crate passes
  through FLV tag bytes verbatim, so other codecs (MP3, H.263,
  Speex, …) work end-to-end but the framing helpers focus on
  `VIDEO_CODEC_AVC` + `AUDIO_FORMAT_AAC`.

## Reusable building blocks

The lower-level modules are public so callers can compose something
non-standard:

- `amf::{encode, decode, encode_command, Amf0Value}`
- `chunk::{ChunkReader, ChunkWriter, Message}`
- `handshake::{client_handshake, server_handshake}`
- `flv::{parse_video, build_video, parse_audio, build_audio}`
- `message::build_*` — builders for every protocol-control /
  command message we emit
