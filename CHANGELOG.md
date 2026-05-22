# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- **AMF3 data / command message routing** (`src/amf3.rs`,
  `src/server.rs`, `src/client.rs`). Wires the r93 AMF3 parser into the
  RTMP message-dispatch path so AMF3-encoded `onMetaData` /
  data-messages (message type id 15) and AMF3 commands (type 17) decode
  end-to-end. Per AMF 3 spec §4.1 + AMF 0 spec §3.1, the outer
  NetConnection message structure is AMF0 and a value switches to AMF3
  via the `avmplus-object-marker` (`0x11`); new
  `amf3::decode_data_message` parses a type-15/17 body that is either
  `0x11`-prefixed (the spec-mandated switch) or already-AMF3
  (no-prefix, for channels negotiated to AMF3 from the start), sharing
  one reference-table context across the whole body. New
  `Amf3Value::to_amf0()` bridges the decoded AMF3 value graph onto the
  `Amf0Value` enum so `server::RtmpSession::next_packet` surfaces AMF3
  metadata through the same `StreamPacket::Metadata(Amf0Value)` path as
  AMF0 — `Integer`/`Date` collapse to `Number`/`Date`, sealed +
  dynamic object members concatenate into one ordered `Object`, the
  AMF3 `Array` dense slot becomes an ECMA-array under stringified
  ordinal keys, and `ByteArray`/`Vector`/`Dictionary`/`Xml*` map to
  their nearest AMF0 shape. The server's `MSG_DATA_AMF3` /
  `MSG_COMMAND_AMF3` arms now route (the AMF3 command path detects the
  same `closeStream` / `deleteStream` / `FCUnpublish` teardown as
  AMF0). New `RtmpClient::send_metadata_amf3` emits an AMF3-encoded
  `onMetaData` for peers on an AMF3 channel. `pub const
  amf3::AVMPLUS_OBJECT_MARKER`; `Amf0Value` / `Amf3Value` re-exported
  at the crate root. This resolves the r93 follow-up noted below.
- 9 new tests: 4 unit tests in `src/amf3.rs` cover `decode_data_message`
  framing (avmplus-wrapped sequence, unprefixed-AMF3, shared reference
  context, dangling-marker error); 5 cover the `to_amf0` bridge
  (scalars with Integer/Date collapse, sealed+dynamic merge ordering,
  Array→ECMA ordinal keys, Vector/ByteArray→StrictArray, and a full
  realistic `onMetaData` body bridged into an AMF0 object). A new
  `tests/amf3_metadata.rs` integration test drives a full
  client→server loopback publishing an AMF3 `onMetaData` and asserts
  the server surfaces every field through `StreamPacket::Metadata`.
- **AMF3 wire-format parser + builder** (`src/amf3.rs`). Implements the
  full Adobe "Action Message Format -- AMF 3" (January 2013)
  specification mirrored under `docs/streaming/rtmp/amf3-file-format-
  spec-adobe.pdf`: all thirteen value markers (Undefined / Null /
  False / True / Integer / Double / String / XMLDocument / Date /
  Array / Object / XML / ByteArray / Vector{Int,UInt,Double,Object} /
  Dictionary), U29 variable-length integers (§1.3.1) with explicit
  sign-extension for the Integer marker (§3.6), and the three
  reference tables (strings / objects / traits) maintained per
  `decode_all` invocation per §2.2. Object support distinguishes
  anonymous / typed / dynamic / externalizable shapes (§3.12);
  externalizable bodies surface as `Some(Vec<u8>)` on the
  `Amf3Value::Object::externalizable_body` field for round-tripping,
  with generic decode refusing externalizable inputs (no class
  handler registered) rather than silently corrupting `pos`.
  `Decoder::reset_tables()` provides the §4.1 packet-boundary reset.
  Encoder always emits literal (non-reference) values — the wire
  bytes remain valid per spec, and any literal can re-enter the
  decoder which will resolve references encountered later in the
  same payload. New helpers `anon_object` / `dynamic_object` /
  `anon_object_unordered` mirror the AMF0 builder ergonomics.
- New 26 unit tests in `src/amf3.rs` exercise: U29 length-class
  boundaries (1-byte, 2-byte, 3-byte, 4-byte) and a spec-Table-1
  canonical-bytes check for 0x7F / 0x80 / 0x4000 / 0x200000; all
  simple-marker (`Undefined` / `Null` / `Boolean`) round-trips;
  Integer sign extension at the negative boundary plus the
  out-of-range fallback to Double; literal-then-reference for both
  string and object tables; empty-string-never-in-table per §1.3.2;
  Date / ByteArray round-trips; dense + associative Array shapes;
  anonymous, dynamic, and typed-with-sealed-and-dynamic Object
  shapes; externalizable-without-handler refuses cleanly;
  `Vector.<int>` / `<uint>` / `<Number>` / `<Object>` (mixed-type)
  round-trips; `Dictionary` with both String and Integer keys;
  `Xml` / `XmlDocument` round-trips; multi-value packet sharing the
  string table across values; dangling-reference rejected; unknown
  marker rejected; trait reference re-used between two consecutive
  typed-object encodings; and object reference resolving to the
  same Date value.

### Notes

- AMF3 message routing via the AMF0 `avmplus-object-marker` (0x11) is
  now wired into the server's message dispatch (see the r96 entry
  above) through `amf3::decode_data_message` + `Amf3Value::to_amf0`,
  rather than by extending `amf::Amf0Value` with a wrapping variant.
  The standalone `amf::decode` path still consumes pure AMF0 only;
  AMF3-channel callers use the `amf3` module (directly or via the
  server / client routing) — the cleaner split given AMF3's
  per-message reference-table context.

- **Enhanced RTMP v2 video FourCC additions** (Veovera 2026).
  `flv::parse_video` / `flv::build_video` now recognise the three
  new `VideoFourCc` values from `enhanced-rtmp-v2.pdf`
  §"Enhanced Video": `vp08` (VP8 — `VPCodecConfigurationRecord`
  for SequenceStart, no SI24 on the wire), `avc1` (FourCC-mode
  AVC/H.264 — `AVCDecoderConfigurationRecord` for SequenceStart,
  SI24 `compositionTimeOffset` on the wire for `CodedFrames` and
  implied-zero for `CodedFramesX`, mirroring the legacy AVC path),
  and `vvc1` (VVC/H.266 — `VVCDecoderConfigurationRecord` for
  SequenceStart, SI24 on the wire for `CodedFrames` and
  implied-zero for `CodedFramesX`, parallel to HEVC's row). The
  parse-side `needs_cts` rule and the build-side `cts_on_wire`
  rule are widened symmetrically: the three NALU-based FourCCs
  (`hvc1` / `avc1` / `vvc1`) all carry the SI24 with
  `CodedFrames`; the non-NALU FourCCs (`av01` / `vp09` / `vp08`)
  and the SequenceStart / SequenceEnd / Metadata / CodedFramesX
  shapes never do.
- **FourCC → `CodecId` mapping for the v2 video additions.**
  `adapter::video_fourcc_codec_id` now resolves `vp08` →
  `"vp8"`, `avc1` → `"h264"` (the same codec id legacy AVC
  reports, so a downstream `oxideav-h264` decoder picks both up
  unchanged), `vvc1` → `"vvc"`. The dispatcher
  `video_codec_id_for_tag` already routes FourCC-mode tags
  through this mapper, so the new codecs surface end-to-end on
  the `PacketSource` adapter without any other change.
- New `FOURCC_VP8` / `FOURCC_AVC` / `FOURCC_VVC` public
  constants in `flv` so callers composing `VideoTag` literals
  for the v2 set don't have to repeat the spec's ASCII bytes.
- New tests (10 in `src/flv.rs`, 5 in `tests/enhanced_rtmp_video.rs`)
  exercise VP8 SequenceStart + CodedFrames, AVC FourCC
  SequenceStart + CodedFrames-with-SI24 + CodedFramesX, VVC
  SequenceStart + negative-CTS CodedFrames + CodedFramesX,
  truncated-SI24 controlled-failure, v2-FourCC disjointness from
  the v1 set, and the full v1+v2 build → parse → build
  idempotence sweep extended with eight new cases. The
  AVC-FourCC keyframe test additionally confirms the resulting
  `Packet` resolves to `CodecId("h264")` and applies the SI24
  to `pts` correctly.

### Notes

- The `connect` command's `videoFourCcInfoMap` advertisement
  (`enhanced-rtmp-v2.pdf` §"Enhancing NetConnection connect
  Command") still does not list the new v2 codecs — a publisher
  using `RtmpClient::connect` continues to negotiate as a legacy
  AVC-only client. Manually-composed `VideoTag` literals with
  `fourcc = Some(FOURCC_VP8 / FOURCC_AVC / FOURCC_VVC)` going
  through `flv::build_video` produce correct wire bytes; the
  high-level publish helper opts in once the configurable
  codec-list follow-up lands.

- **Enhanced RTMP v2 audio framing** (Veovera 2026). `flv::parse_audio`
  / `flv::build_audio` now recognise the `ExHeader = 9` value in the
  `SoundFormat` nibble of the audio-tag header byte and handle the
  `FourCC`-based extended header (`Opus` / `fLaC` / `ac-3` / `ec-3` /
  `.mp3` / `mp4a`) per `enhanced-rtmp-v2.pdf` §"Enhanced Audio" /
  "Extended AudioTagHeader" / "ExAudioTagBody". The three core
  `AudioPacketType` values round-trip: `SequenceStart` (per-codec
  sequence header — `OpusHead` / `fLaC + STREAMINFO` /
  `AudioSpecificConfig` for FourCC-AAC), `CodedFrames` (codec
  bitstream — AC-3 / E-AC-3 sync frames, Opus self-delimited packets
  per RFC 6716 App. B, MP3 frames, raw AAC frames), and `SequenceEnd`
  (empty body, "no less than the same meaning as a silence message"
  per spec). New `AudioTag` fields `ex_packet_type: Option<u8>` and
  `audio_fourcc: Option<[u8; 4]>` are the discriminators; legacy
  publishers leave both `None` and the parser / builder follow the
  pre-2023 SoundFormat / SoundRate / SoundSize / SoundType single-byte
  path unchanged.
- **FourCC → `CodecId` mapping for audio.** New
  `adapter::audio_fourcc_codec_id([u8; 4]) -> CodecId` resolves
  `Opus`/`fLaC`/`ac-3`/`ec-3`/`.mp3`/`mp4a` to
  `"opus"`/`"flac"`/`"ac3"`/`"eac3"`/`"mp3"`/`"aac"`, and the new
  dispatcher `adapter::audio_codec_id_for_tag(&AudioTag) -> CodecId`
  selects legacy vs FourCC off `tag.audio_fourcc.is_some()`.
  `audio_codec_params` now copies the body of any Enhanced-RTMP
  `PacketTypeSequenceStart` audio tag into `CodecParameters.extradata`
  (matching the existing AVC / HEVC behaviour), so downstream Opus /
  FLAC / AAC decoders pick up their initialisation header without
  re-parsing the packet stream.
- **Packet flags propagated for Enhanced RTMP audio**.
  `audio_to_packet` now sets `flags.header = true` for both legacy
  AAC sequence-headers (unchanged) and Enhanced-RTMP
  `PacketTypeSequenceStart`, and also flags `SequenceEnd` as a header
  packet (empty body) so consumers can route it to an end-of-sequence
  / flush boundary without trying to decode an empty payload. The
  legacy AAC packet-type marker byte is **not** prepended in Enhanced
  mode — the body is the raw codec data per `ExAudioTagBody`.
- New `AUDIO_FORMAT_EX_HEADER` / `AUDIO_PACKET_TYPE_*` / `FOURCC_AC3`
  / `FOURCC_EAC3` / `FOURCC_OPUS` / `FOURCC_MP3` / `FOURCC_FLAC` /
  `FOURCC_AAC` public constants in `flv` so callers composing
  `AudioTag` literals (e.g. an Enhanced-RTMP-aware push client) don't
  have to repeat the spec's magic numbers.
- New integration test (`tests/enhanced_rtmp_audio.rs`, 9 cases)
  exercises wire-byte → `Packet` flow for Opus `SequenceStart`
  (`OpusHead` ID-header round-trip), AC-3 / E-AC-3 / MP3
  `CodedFrames`, FLAC `SequenceStart` (with the in-body `fLaC`
  signature distinguished from the framing FourCC), `SequenceEnd`,
  build-parse-build idempotence across the 5-FourCC × 3-PacketType
  matrix, and legacy/Enhanced disjointness.

### Notes

- AudioPacketType `Multitrack`, `MultichannelConfig`, and `ModEx`
  (with the only-defined `TimestampOffsetNano = 0` subtype) parse
  paths are **not** implemented yet. Their nested layouts
  (per-track FourCC + size-prefixed track chunks; AudioChannelOrder
  + channel-count + channel-map / 32-bit AudioChannelFlags mask;
  size-prefixed ModEx data + ModExType nibble chain) are spec'd in
  `enhanced-rtmp-v2.pdf` §"ExAudioTagBody" but warrant a dedicated
  follow-up round so we can wire them through `audio_to_packet` /
  `CodecParameters` properly. A tag whose `AudioPacketType` decodes
  to `Multitrack`, `MultichannelConfig`, or `ModEx` is currently
  preserved verbatim (FourCC + raw body) — the parser does not
  fail, but the caller is expected to skip the message rather than
  interpret the body as a normal `CodedFrames` payload.
- The `connect` command's `audioFourCcInfoMap` / `capsEx` /
  `videoFourCcInfoMap` advertisements (`enhanced-rtmp-v2.pdf`
  §"Enhancing NetConnection connect Command") are still not
  populated by `RtmpClient`. A publisher using `RtmpClient::connect`
  will negotiate as a legacy AVC + AAC client. Manually-composed
  `AudioTag` literals with `audio_fourcc = Some(..)` going through
  `flv::build_audio` still produce correct wire bytes; only the
  high-level publish helper declines to opt in until a future round
  adds a configurable codec list to `RtmpClient`.

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
- AMF3 message bodies (TagType 15 / Command type 17 / Data
  type 15 / Shared-Object type 16) are now decodable via the
  new `amf3` module — see the entry above for the wire-format
  parser landing.

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
