# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- **Enhanced RTMP v1 video framing** (Veovera 2023). `flv::parse_video`
  / `flv::build_video` now recognise the `IsExHeader` flag in the
  high bit of the video-tag header byte and handle the
  `FourCC`-based extended header (`hvc1` / `av01` / `vp09`) per
  `enhanced-rtmp-v1.pdf` §"Defining Additional Video Codecs",
  Table 4. All five `PacketType` values round-trip:
  `SequenceStart` (codec configuration record — `HEVCDecoder
  ConfigurationRecord` / `AV1CodecConfigurationRecord` /
  `VPCodecConfigurationRecord`), `CodedFrames`, `CodedFramesX`
  (the SI24=0 wire-size optimisation), `SequenceEnd`, and
  `PacketTypeMetadata` (HDR `colorInfo`). The SI24
  `CompositionTime` is emitted only for the one shape that
  carries it — HEVC × `CodedFrames` — matching the spec's
  "CompositionTime Offset is implied to equal zero" exception
  for the non-HEVC FourCCs and `CodedFramesX`. New `VideoTag`
  fields `ex_packet_type: Option<u8>` and `fourcc: Option<[u8;
  4]>` are the discriminators; legacy publishers leave both
  `None` and the parser/builder follow the pre-2023 single-byte
  `CodecID` path unchanged.
- **FourCC → `CodecId` mapping.** New
  `adapter::video_fourcc_codec_id([u8; 4]) -> CodecId` resolves
  `hvc1`/`av01`/`vp09` to `"hevc"`/`"av1"`/`"vp9"`, and the new
  dispatcher `adapter::video_codec_id_for_tag(&VideoTag) ->
  CodecId` selects legacy vs FourCC off `tag.fourcc.is_some()`.
  `video_codec_params` now copies the body of any Enhanced-RTMP
  `PacketTypeSequenceStart` tag into `CodecParameters.extradata`
  (matching the existing AVC behaviour), so downstream HEVC /
  AV1 / VP9 decoders pick up their configuration record without
  re-parsing the packet stream.
- **Packet flags propagated for Enhanced RTMP**. `video_to_packet`
  now sets `flags.header = true` for both legacy AVC
  sequence-headers and Enhanced-RTMP `PacketTypeSequenceStart`,
  preserves the keyframe bit for `CodedFrames(X)`, and
  suppresses `keyframe` while setting `header` for
  `PacketTypeMetadata` (per spec: "presence of
  PacketTypeMetadata means that FrameType flags at the top of
  this table should be ignored"). The HEVC × `CodedFrames`
  SI24 CTS is applied to `pts` the same way AVC's CTS is, so a
  B-frame publisher with a non-zero composition-time offset
  gets the correct `dts != pts` split on the consumer side.
- New `EX_PACKET_TYPE_*` / `FOURCC_*` / `VIDEO_IS_EX_HEADER`
  public constants in `flv` so callers composing
  `VideoTag` literals (e.g. an Enhanced-RTMP-aware push
  client) don't have to repeat the spec's magic numbers.
- New integration test (`tests/enhanced_rtmp_video.rs`)
  exercises wire-byte → `Packet` flow for HEVC keyframes, HEVC
  negative-CTS, AV1 CodedFrames, VP9 SequenceStart, and a
  build-parse-build idempotence sweep across all six
  FourCC × PacketType combinations the spec defines.

### Notes

- The `connect` command's `fourCcList` advertisement (Enhanced
  RTMP v1 Table 5) is **not** populated by the client yet — a
  publisher using `RtmpClient::connect` will negotiate as a
  legacy AVC-only client. Manually-composed `VideoTag` literals
  with `fourcc = Some(..)` going through `flv::build_video` still
  produce correct wire bytes; only the high-level publish helper
  declines to opt in until a future round adds a configurable
  codec list to `RtmpClient`.
- AMF3 message bodies are still not parsed (TagType 15 /
  Command type 17 / Data type 15 / Shared-Object type 16). The
  legacy RTMP 1.0 AMF0 flow is what every commodity ingest
  endpoint negotiates, but a follow-up round can lift the
  `amf` module to AMF3 once the `docs/streaming/rtmp/amf3-*.pdf`
  spec is transcribed.

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
