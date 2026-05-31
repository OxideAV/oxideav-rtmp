# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- **FLV file / byte-stream writer** (`src/flv_file.rs`,
  `tests/flv_file_record.rs`). New `flv_file` module exposing
  `FlvWriter<W: Write>`, `FlvHeaderFlags`, `build_flv_header`, and
  `build_flv_tag` per `docs/container/flv/flv_v10_1.pdf` Annex E.
  The writer emits the §E.2 9-byte file header (signature `F` `L`
  `V`, version, `TypeFlagsAudio` / `TypeFlagsVideo`, `DataOffset =
  9`) and the §E.3 alternating `PreviousTagSize` / `FLVTAG` body,
  framing each `VideoTag` / `AudioTag` via the existing
  `flv::build_video` / `flv::build_audio` paths and tracking the
  `PreviousTagSize` back-pointer (`11 + DataSize`) automatically.
  `write_script_data(timestamp_ms, name, &Amf0Value)` emits an
  AMF0 `name + value` pair as a §E.4.4 script-data tag (type 18) —
  the canonical use is an `onMetaData` tag emitted right after the
  header. The §E.4.1 24-bit `Timestamp` + 8-bit `TimestampExtended`
  splitting is handled transparently so callers pass a single
  `u32` timestamp. `write_raw_tag` is the escape hatch for callers
  who build their own payload (e.g. an Annex F encrypted body).
  Composes with `RtmpSession` so an RTMP ingest can be recorded to
  an `.flv` file or re-served over HTTP-FLV (an HTTP-FLV response
  body is exactly this byte stream with `Content-Type:
  video/x-flv`) without re-parsing any payload. 19 new tests (16
  unit in `src/flv_file.rs`, 3 integration in
  `tests/flv_file_record.rs`): header signature + flags + offset
  bytes match §E.2 exactly; 9-byte UI24 `DataSize` + UI8
  `TimestampExtended` layout matches §E.4.1 byte-for-byte (both
  the timestamp-under-24-bit and timestamp-over-24-bit-needing-
  TimestampExtended paths); `PreviousTagSize0` always 0 (§E.3);
  multi-tag back-pointer tracking across an interleaved video +
  audio + video sequence (20 / 16 / 23 byte tags); over-UI24
  payload rejected as `InvalidInput` (16 MiB+ would forge the size
  field otherwise); legacy AVC sequence-header round-trip through
  `parse_video`; AAC sequence-header round-trip through
  `parse_audio`; Enhanced-RTMP v2 HEVC `CodedFrames` ExHeader
  round-trip preserves FourCC + composition_time; AMF0 onMetaData
  script-tag name-then-value layout round-trips; `finish()`
  idempotency; post-`finish` writes return `BrokenPipe`; the
  escape-hatch `write_raw_tag` lets a caller pass an opaque
  payload; empty FLV stream (header + `PreviousTagSize0`-only)
  parses to zero tags; end-to-end RTMP loopback → `FlvWriter` →
  byte-by-byte FLV walker → `parse_video` / `parse_audio` proves
  every recorded payload re-parses unchanged. Total integration
  tests: 61 → 64 (+3); total tests: 202 → 222 (+20 including the
  new doc-test).

## [0.0.5](https://github.com/OxideAV/oxideav-rtmp/compare/v0.0.4...v0.0.5) - 2026-05-29

### Other

- Enhanced RTMP v2 Multitrack body parser + builder (audio + video)
- decode Enhanced-RTMP v2 MultichannelConfig audio body
- injection-robust property tests + AMF nesting depth guards
- poll_event surfaces server-originated UserControl + onStatus
- emit UserControl StreamEOF before Unpublish.Success on close

### Added

- **Enhanced RTMP v2 `Multitrack` audio + video body parser + builder**
  (`src/flv.rs`, `tests/multitrack.rs`). The `VideoPacketType.Multitrack = 6`
  and `AudioPacketType.Multitrack = 5` body shapes from
  `enhanced-rtmp-v2.pdf` §"ExVideoTagBody" / §"ExAudioTagBody" are now
  decoded end-to-end. The `multitrackType (UB[4]) | realPacketType
  (UB[4])` byte plus the optional shared FourCC (omitted in
  `ManyTracksManyCodecs` mode per spec) are consumed inline by
  `parse_video` / `parse_audio` ahead of the existing FourCC slot, and
  the per-track list (`(trackFourCc if ManyTracksManyCodecs) |
  trackId(UI8) | (sizeOfTrack(UI24) if not OneTrack) | body`) is lifted
  into a typed `Multitrack { multitrack_type, tracks }` struct on the
  new `VideoTag::multitrack` / `AudioTag::multitrack` fields. The
  outer tag's `ex_packet_type` now holds the *real* inner PacketType
  (e.g. `CodedFrames`, `SequenceStart`) so a downstream `ex_packet_type
  == SequenceStart` check still works for multitrack tags, and
  `fourcc` / `audio_fourcc` hold the shared codec for `OneTrack` /
  `ManyTracks` modes (and `None` for `ManyTracksManyCodecs`, where each
  `MultitrackTrack::fourcc` carries the per-track codec).
  `VideoTag::multitrack_tag` and `AudioTag::multitrack_tag` are the
  outbound builders; `VideoTag::is_multitrack()` / `AudioTag::is_multitrack()`
  are the discriminators. New constants `AV_MULTITRACK_TYPE_ONE_TRACK`
  / `AV_MULTITRACK_TYPE_MANY_TRACKS` / `AV_MULTITRACK_TYPE_MANY_TRACKS_MANY_CODECS`
  cover the spec's `enum AvMultitrackType`. Reserved discriminators
  (3..=15) round-trip verbatim through `Multitrack::parse` /
  `Multitrack::encode` so a forwarding ingest preserves future modes.
  The spec invariant that the inner real PacketType MUST NOT itself be
  `Multitrack` is enforced — a forged inner nibble of `6` (video) or
  `5` (audio) surfaces a clean `Error::Other("…MUST NOT…")` rather
  than recursing, and a truncated `sizeOfTrack` overrun yields a clean
  `…overruns remaining N bytes` error. 23 new tests (11 unit in
  `src/flv.rs`, 12 integration in `tests/multitrack.rs`) cover: video
  `OneTrack` AVC CodedFrames byte-exact wire layout; video `ManyTracks`
  HEVC two-track byte-exact UI24 sizes; video `ManyTracksManyCodecs`
  HEVC+AV1; video `SequenceStart` `ManyTracks` VVC; audio `OneTrack`
  Opus CodedFrames; audio `ManyTracksManyCodecs` Opus+FLAC mixed-codec;
  audio `ManyTracks` AAC; audio `SequenceStart` `ManyTracks` AAC with
  per-track ASC; the inner-PacketType-MUST-NOT-be-Multitrack invariant
  for both audio and video; size-overrun-error and three other
  truncation paths; track ordering preserved verbatim through round-trip
  (trackIds `[7, 0, 3]` stay `[7, 0, 3]`); empty per-track body
  (`sizeOfTrack = 0`) round-trips; `ManyTracks` ModEx-prelude
  composition with `TimestampOffsetNano = 123_456` (proves the
  ModEx + Multitrack preludes compose on the wire); and a reserved
  `multitrack_type = 4` direct `Multitrack::encode`+`parse` symmetry.
  Resolves the `Multitrack` portion of the r177 / r186 README notes
  "`Multitrack` still parses to an opaque body pending a follow-up
  round." Total integration-test count: 49 → 61 (+12); total tests:
  191 → 202.

- **Enhanced RTMP v2 `MultichannelConfig` audio body parser + builder**
  (`src/flv.rs`). The `AudioPacketType.MultichannelConfig = 4` body
  shape from `enhanced-rtmp-v2.pdf` §"ExAudioTagBody" is now decoded
  end-to-end via the new `MultichannelConfig` struct + the
  `MultichannelConfigOrder` discriminated union (`Unspecified` /
  `Native { flags: u32 }` / `Custom { mapping: Vec<u8> }` /
  `Reserved(u8)`). On the wire the body is
  `audioChannelOrder(UI8) | channelCount(UI8) | (mapping[UI8] |
  flags(UI32) | nothing)`; lengths line up at 2 bytes for `Unspecified`,
  6 bytes for `Native`, and `2 + channelCount` for `Custom`. Truncated
  bodies, stray trailing bytes on `Unspecified`, and short `Custom`
  mappings all return `Err(Error::Other)` cleanly; an unrecognised
  `audioChannelOrder` value is preserved as
  `MultichannelConfigOrder::Reserved` with the trailing bytes in
  `extra` so a forwarding tag never silently loses data.
  `AudioTag::is_multichannel_config()`,
  `AudioTag::multichannel_config()`, and
  `AudioTag::multichannel_config_tag(fourcc, &cfg)` provide the
  lift / round-trip helpers; the existing `parse_audio` / `build_audio`
  bytes path is unchanged (the body is still carried verbatim through
  `AudioTag::body`). New `audio_channel` and `audio_channel_mask`
  public submodules expose the 24 spec-defined channel positions and
  their corresponding UI32 mask bits (including the 22.2 surround
  extensions per SMPTE ST 2036-2-2008), plus `audio_channel::UNUSED`
  (0xFE) and `UNKNOWN` (0xFF) sentinels. New
  `AUDIO_CHANNEL_ORDER_UNSPECIFIED` / `_NATIVE` / `_CUSTOM` constants
  cover the order discriminator. 10 new unit tests cover: 2-byte
  `Unspecified` round-trip; 6-byte `Native` 5.1 mask round-trip with
  byte-exact wire bytes; `Custom` stereo round-trip; full 22.2
  `Custom` (24-byte mapping) round-trip exercising every
  `audio_channel::*` constant; `UNUSED` / `UNKNOWN` sentinel
  preservation; six truncation paths (empty body, partial header,
  partial flags, short mapping, stray bytes on `Unspecified`);
  `Reserved` order verbatim round-trip; end-to-end build → parse →
  lift through `build_audio` + `parse_audio` on an Opus FourCC tag;
  accessor returns `None` for non-MultichannelConfig packet types and
  for legacy tags whose body happens to start with valid-looking
  bytes; and a bit-position check confirming `1 << channel_index`
  equals the matching `audio_channel_mask` entry for every one of the
  24 channels. Resolves the `MultichannelConfig` portion of the r177
  README note "`Multitrack` and `MultichannelConfig` AudioPacketTypes
  parse to opaque bodies pending follow-up rounds." `Multitrack`
  remains deferred — its `AvMultitrackType + per-track FourCc + track
  id + sizeOfAudioTrack` framing needs a richer follow-up.

- **Injection-robustness property tests + AMF0/AMF3 nested-container
  depth guards** (`src/amf.rs`, `src/amf3.rs`,
  `tests/injection_robustness.rs`). Every public parser surface — AMF0
  (`decode` / `decode_all`), AMF3 (`decode` / `decode_all` /
  `decode_data_message`), FLV (`parse_video` / `parse_audio`), the
  chunk-stream reader (`ChunkReader::read_message`), and both
  handshake directions (`client_handshake` / `server_handshake`) — is
  now fuzzed with a deterministic xorshift PRNG (no `rand` dep): 1024
  random-byte iterations for each AMF surface, 2048 for each FLV
  surface, 512 for `ChunkReader`, plus a 1024-iteration "valid frame
  with 1..=4 random byte flips" mutation pass on a built
  `onMetaData` payload. Adversarial structural inputs are also covered:
  truncated handshakes from both directions and at every truncation
  boundary, wrong RTMP version bytes (`0x00` / `0x01` / `0x06` /
  `0xFF`), AMF0 `M_STRICT_ARRAY` with a `u32::MAX` length, AMF0
  `M_STRING` claiming 65535 bytes from a 3-byte buffer, a fmt-0 chunk
  with an oversize 24-bit `msg_length` and a forged fmt-1 chunk
  arriving with no prior fmt-0 state. The runtime guarantee: every
  call either returns `Ok` or `Err`, never panics, never spins, and
  never over-allocates (`amf0_strict_array_with_huge_count_errors_fast`
  asserts the error path is under 100 ms even with `u32::MAX`).
  Stack-overflow protection: `amf::MAX_DECODE_DEPTH = 64` and
  `amf3::MAX_DECODE_DEPTH = 64` cap nested-container recursion before
  the call stack runs out; AMF0 routes through a new
  `decode_at_depth(buf, pos, depth)` and AMF3 tracks `depth` as a
  field on `Decoder` (incremented on entry to `decode`, decremented on
  return). Tests build 2_000-level-deep forged Object frames in both
  formats and assert the guard surfaces a clean `Error::InvalidAmf0`
  before the default 8 MiB stack overflows. Integration-test count:
  28 → 49 (+21).

- **`RtmpClient::poll_event` surfaces server-originated events; symmetric
  `UserControl StreamEOF` recognition on the client side** (`src/client.rs`,
  `src/lib.rs`). Round 154 added the server-side teardown that emits a
  `UserControl StreamEOF(stream_id)` (RTMP 1.0 §7.1.7) before
  `onStatus(NetStream.Unpublish.Success)` and the write-half FIN. The
  pre-r158 `RtmpClient` swallowed those server-originated bytes — the
  `reader` field was `#[allow(dead_code)]`-flagged and the only
  post-publish reads happened opportunistically when the underlying
  `TcpStream` dropped, so a publisher couldn't tell "server cleanly
  closed our publish" from "TCP connection died." This round wires up a
  `poll_event(&mut self) -> Result<Option<ClientEvent>>` surface: each
  call reads one inbound RTMP message, handles protocol-control
  housekeeping internally (Set Chunk Size, Window Ack Size, Set Peer
  Bandwidth, Ping Request → Ping Response auto-reply), and returns the
  externally-visible notifications as a new `ClientEvent` enum:
  `StreamBegin { stream_id }`, `StreamEof { stream_id }`,
  `OnStatus { level, code, description }`,
  `Result { transaction_id, values }`,
  `ErrorReply { transaction_id, values }`, and `Other`. `StreamEof` is
  not itself terminal — the server's close path emits onStatus *after*
  StreamEOF, so `poll_event` keeps reading until the TCP read half
  observes EOF / connection-reset, at which point a `read_eof` latch
  makes subsequent calls return `Ok(None)` immediately without
  re-entering the chunk reader on a dead socket. New
  `tests/client_stream_eof.rs` covers end-to-end against our own
  `RtmpServer::close`: the client observes
  `ClientEvent::StreamEof { stream_id: 1 }` followed by
  `ClientEvent::OnStatus { code: "NetStream.Unpublish.Success", .. }`,
  and a separate test verifies the post-EOF latch returns `Ok(None)` in
  under 50 ms. Four new unit tests in `src/client.rs` cover the UCM
  payload parser (`parse_user_control` + `ucm_stream_id`) and the AMF0
  command classifier (`classify_command` for `onStatus` / `_result` /
  `_error`).

- **Server session close emits `UserControl StreamEOF` before
  `onStatus(NetStream.Unpublish.Success)`** (`src/server.rs`,
  `src/message.rs`). The publish-side end-of-stream signal is now an
  explicit RTMP wire event rather than a bare TCP FIN. `RtmpSession::close`
  emits, in order: a `UserControl StreamEOF(stream_id)` event (RTMP 1.0
  §7.1.7 — the `the stream is dry` event the spec assigns to the
  server-to-client direction, re-used symmetrically here for an
  end-of-publish notification), the existing
  `onStatus("NetStream.Unpublish.Success")` command, a chunk-writer
  `flush()` so every buffered chunk reaches the kernel, then a write-half
  `Shutdown::Write` — mirroring the client-side r152 fix so the peer drains
  every buffered frame and command before observing EOF. New
  `message::build_user_control_stream_eof(u32)` builder exposes the event
  for callers that want to emit it on a non-publish stream. New
  `tests/session_close.rs` integration test drains raw bytes off the
  client socket and asserts the StreamEOF six-byte body precedes the
  literal AMF0 `NetStream.Unpublish.Success` string.

## [0.0.4](https://github.com/OxideAV/oxideav-rtmp/compare/v0.0.3...v0.0.4) - 2026-05-24

### Other

- graceful FIN on close to stop teardown truncating A/V frames
- Enhanced RTMP v2 ModEx packet-type prelude (audio + video)
- route AMF3 data/command messages into message dispatch
- AMF3 wire-format parser + builder (full §3.1 + §1.3.1 + §2.2)
- Enhanced RTMP v2 video FourCC additions (vp08 / avc1 / vvc1)
- Enhanced RTMP v2 audio framing (FourCC Opus / FLAC / AC-3 / E-AC-3 / MP3 / AAC)
- Enhanced RTMP v1 video framing (FourCC HEVC / AV1 / VP9)

### Fixed

- **Client teardown no longer truncates in-flight A/V frames**
  (`src/client.rs`). `RtmpClient::close` previously did
  `TcpStream::shutdown(Shutdown::Both)` immediately after writing the
  `closeStream` command. Closing the read half at the same instant lets
  the kernel answer the peer's still-unacked data with a RST on some
  platforms, which discards any audio/video messages the peer hasn't yet
  drained from its receive buffer — so the last frames plus `closeStream`
  could vanish mid-stream. `close` now shuts down only the write half
  (`Shutdown::Write`, a graceful FIN); the peer drains every buffered
  frame and our `closeStream` command before observing EOF. This
  resolves the intermittent `loopback_publish` failure (server saw 2 of
  4 video tags) that surfaced on fast Linux CI runners.

### Added

- **Enhanced RTMP v2 ModEx prelude** (`src/flv.rs`). `flv::parse_video`
  / `flv::build_video` and `flv::parse_audio` / `flv::build_audio` now
  decode and re-emit the `ModEx` packet-type prelude chain per
  `enhanced-rtmp-v2.pdf` §"ExVideoTagHeader" / §"ExAudioTagHeader" (the
  `while (packetType == ModEx)` loop). When the PacketType nibble of the
  header byte is `ModEx (7 for video, 7 for audio)`, a chain of
  size-prefixed entries precedes the FourCC: each entry is a
  `modExDataSize` (`UI8 + 1`, escaping to a `0xFF` sentinel + `UI16 + 1`
  for 256..=65536 bytes), the `modExData` bytes, and a single byte whose
  high nibble is the `modExType` (`UB[4]`) and whose low nibble is the
  *next* PacketType (`UB[4]`) — looping until a non-ModEx PacketType
  terminates the chain. New `flv::ModEx { mod_ex_type, data }` captures
  each entry; new `VideoTag::mod_ex` / `AudioTag::mod_ex` fields hold the
  ordered chain and round-trip it verbatim ahead of the real packet
  type. The only `mod_ex_type` defined today is
  `TimestampOffsetNano = 0` (a `bytesToUI24` 0..=999_999 ns
  sub-millisecond presentation offset); `ModEx::timestamp_offset_nano`,
  `ModEx::timestamp_offset_nano_entry`, and
  `VideoTag::timestamp_offset_nano` / `AudioTag::timestamp_offset_nano`
  expose it. Crucially, after parsing, `ex_packet_type` holds the real
  PacketType recovered from the chain (not `ModEx`), so the
  `video_to_packet` / `audio_to_packet` adapters route a ModEx-prefixed
  tag to the correct CodecId + packet flags transparently — previously
  the header's `7` nibble would have been mis-read as an unknown
  PacketType and the chain bytes mistaken for the FourCC. New public
  constants `EX_PACKET_TYPE_MOD_EX`, `EX_PACKET_TYPE_MULTITRACK`,
  `MOD_EX_TYPE_TIMESTAMP_OFFSET_NANO`; `flv::ModEx` re-exported at the
  crate root.
- 9 new tests (8 unit in `src/flv.rs`, 1 integration in
  `tests/enhanced_rtmp_video.rs`) cover: video + audio
  TimestampOffsetNano single-entry round-trips, a two-entry chain
  (TimestampOffsetNano + an unknown subtype preserved verbatim), the
  UI16 size escape (300-byte modExData), the accessor rejecting the
  wrong subtype / short data, controlled-failure on truncated chains
  (missing data / nibble / FourCC) for both audio and video, byte-exact
  no-prelude output for an empty `mod_ex`, and a full
  ModEx-wire-bytes → `parse_video` → `video_to_packet` → CodecId
  resolution + `build_video` round-trip proving the prelude is
  transparent to the adapter.
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
