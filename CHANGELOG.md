# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.0.3](https://github.com/OxideAV/oxideav-rtmp/compare/v0.0.2...v0.0.3) - 2026-05-03

### Other

- replace never-match regex with semver_check = false
- migrate to centralized OxideAV/.github reusable workflows
- SourceRegistry PacketSource for rtmp:// URIs
- pin release-plz to patch-only bumps

### Added

- **`SourceRegistry` integration via `register(registry)`.** New
  `RtmpPacketSource` adapter implements
  `oxideav_core::PacketSource`, wrapping an `RtmpSession` and
  emitting `oxideav_core::Packet`s on stream 0 (audio) and
  stream 1 (video) with `TimeBase 1/1000` (RTMP's native
  millisecond unit). Codec ids resolved from the first audio +
  video tags via `audio_codec_id` / `video_codec_id`
  (`aac`/`mp3`/`pcm_*`/`speex`/`nellymoser` for audio,
  `h264`/`h263`/`vp6f`/`vp6a`/`flashsv`/`flashsv2` for video).
  AVC composition-time offsets are applied to PTS;
  sequence-header packets carry the `header` flag and
  AVCDecoderConfigurationRecord / AudioSpecificConfig in
  `CodecParameters::extradata`. Listen-style opener:
  `rtmp://host:port/app/stream-name` URIs bind a one-shot
  listener that accepts one publisher and validates the
  announced app + stream-name against the URL path. The
  historical `RtmpServer::accept` / `RtmpClient::connect` API is
  unchanged — registry support is purely additive.
- New integration test (`tests/packet_source.rs`) round-trips a
  synthetic publisher → registry opener → `PacketSource` flow,
  asserting stream descriptors, packet ordering, pts/dts, and
  the `header` / `keyframe` flags.

## [0.0.2](https://github.com/OxideAV/oxideav-rtmp/compare/v0.0.1...v0.0.2) - 2026-04-25

### Other

- release v0.0.1 ([#1](https://github.com/OxideAV/oxideav-rtmp/pull/1))

## [0.0.1](https://github.com/OxideAV/oxideav-rtmp/releases/tag/v0.0.1) - 2026-04-19

### Other

- Initial commit: pure-Rust RTMP ingest + push

### Added

- Initial release: pure-Rust RTMP with zero external dependencies.
- **Server (source):** `RtmpServer::bind` + `accept()` yields a
  `PublishRequest` that carries `app`, `stream_name`, `tc_url`, and
  `peer_addr`. Consumers verify the stream key / auth however they
  like, then `.accept()` (returns an `RtmpSession`) or `.reject()`
  (sends `NetStream.Publish.BadName` and closes). `serve(handler)`
  wraps the above in a thread-per-connection loop for multi-client
  use.
- **Client (sink):** `RtmpClient::connect("rtmp://…/app/stream")`
  runs the full publish handshake, then `send_video` /
  `send_audio` / `send_metadata` push H.264 / AAC payloads upstream.
- Reusable protocol building blocks: `amf` (AMF0 encode/decode),
  `handshake` (plain version-3 handshake), `chunk` (reader + writer
  with full fmt 0/1/2/3 + extended timestamp support), `message`
  (builders for every command / protocol-control message the publish
  flow emits), `flv` (video + audio FLV-tag parse / build for H.264
  + AAC).
- Loopback integration test — server and client running in one
  process confirm the full round-trip round-trips every frame.

### Not implemented

- RTMPS / TLS. Wrap the crate's `Read + Write` with rustls if
  needed; an `rtmps` feature may land later.
- RTMP play direction (subscribing / pulling).
- AMF3, shared objects, RTMFP, the Adobe digest-verified handshake.
