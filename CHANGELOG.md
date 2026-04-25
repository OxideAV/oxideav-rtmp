# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
