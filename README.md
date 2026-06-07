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
        StreamPacket::Metadata(meta)           => { /* onMetaData object (AMF0 or AMF3, bridged to AMF0) */ }
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
- **AMF0 command flow** is what every commodity ingest endpoint
  negotiates by default. The
  [`amf3`] module ships a complete AMF3 wire-format encoder +
  decoder — all thirteen markers plus the three reference tables —
  and AMF3 data / command messages are now **routed end-to-end**:
  the server decodes `onMetaData` carried as a type-15 AMF3 data
  message (per AMF 3 spec §4.1 / AMF 0 spec §3.1, an AMF0 frame
  switching to AMF3 via the `avmplus-object-marker` `0x11`),
  bridges the AMF3 value graph onto `Amf0Value`, and surfaces it
  through the same `StreamPacket::Metadata` path as AMF0; type-17
  AMF3 commands feed the same stream-teardown detection.
  `RtmpClient::send_metadata_amf3` emits the AMF3-encoded form for
  peers on an AMF3 channel. Shared objects, RTMFP, and the Adobe
  digest-verified handshake remain unimplemented.
- **H.264 + AAC** are the canonical legacy payloads, plus
  **Enhanced RTMP v1** (Veovera 2023) FourCC video codecs —
  `hvc1` (HEVC / H.265), `av01` (AV1), `vp09` (VP9) — and the
  **Enhanced RTMP v2** (Veovera 2026) additions: `vp08` (VP8),
  `avc1` (FourCC-mode AVC / H.264), `vvc1` (VVC / H.266).
  Sequence-start (HEVC / VVC / AVC `DecoderConfigurationRecord`,
  `AV1CodecConfigurationRecord`, `VPCodecConfigurationRecord` for
  VP8 + VP9), `CodedFrames`, `CodedFramesX` (CTS=0 omitted),
  `SequenceEnd`, and the HDR `PacketTypeMetadata` (`colorInfo`)
  frames all round-trip via `flv::parse_video` / `flv::build_video`.
  SI24 `compositionTimeOffset` is emitted on the wire for the three
  NALU-based FourCCs (`hvc1` / `avc1` / `vvc1`) with `CodedFrames`
  and stripped for `CodedFramesX` per the v2 spec. The crate passes
  through FLV tag bytes verbatim, so additional codecs (MP3, H.263,
  Speex, …) keep working too.
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
  SequenceStart). The `MultichannelConfig` AudioPacketType is also
  decoded end-to-end: `AudioTag::multichannel_config()` lifts the body
  into the strongly-typed `MultichannelConfig` view (per `enhanced-rtmp-v2.pdf`
  §"ExAudioTagBody" — `audioChannelOrder(UI8) | channelCount(UI8) |
  (audioChannelMapping[UI8] | audioChannelFlags(UI32))`), and
  `AudioTag::multichannel_config_tag` rebuilds the matching outbound tag.
  `audio_channel` / `audio_channel_mask` public submodules name all 24
  spec-defined positions (including the 22.2 surround extras from SMPTE
  ST 2036-2-2008). The `Multitrack` AudioPacketType / VideoPacketType
  is also wired end-to-end: the `multitrackType (UB[4]) | realPacketType
  (UB[4])` byte plus the optional shared FourCC (per spec, omitted in
  `ManyTracksManyCodecs` mode) are consumed inline by `parse_audio` /
  `parse_video`, and the per-track list
  (`(trackFourCc if ManyTracksManyCodecs) | trackId(UI8) |
  (sizeOfTrack(UI24) if not OneTrack) | body`) lifts to a typed
  [`Multitrack { multitrack_type, tracks: Vec<MultitrackTrack> }`] on
  `VideoTag::multitrack` / `AudioTag::multitrack`. `OneTrack` /
  `ManyTracks` / `ManyTracksManyCodecs` all round-trip — including the
  inner-PacketType-MUST-NOT-be-Multitrack invariant from the spec — via
  the new `VideoTag::multitrack_tag` / `AudioTag::multitrack_tag`
  builders. Reserved `multitrackType` values (3..=15) pass through
  verbatim so a forwarding ingest preserves future modes.
- **Graceful session close**. `RtmpSession::close` emits a
  `UserControl StreamEOF(stream_id)` (RTMP 1.0 §7.1.7) before
  `onStatus("NetStream.Unpublish.Success")`, flushes the chunk writer,
  and half-closes the write side. The peer drains every buffered
  frame plus the EOF + status reply before observing FIN, matching the
  client-side teardown behaviour. Symmetrically, `RtmpClient::poll_event`
  surfaces every server-originated User Control Message defined in
  RTMP 1.0 §3.7 — `StreamBegin`, `StreamEOF`, `StreamDry`,
  `StreamIsRecorded`, `PingResponse` — as typed `ClientEvent` variants
  alongside `onStatus(...)`, `_result`, and `_error` replies, so a
  publisher can distinguish a clean server-initiated end-of-stream from
  a transient "no data" probe, an on-demand-recorded advertisement, or
  the round-trip echo of its own `RtmpClient::send_ping_request`.
  Server-originated `PingRequest` is auto-replied internally (per spec
  §3.7 — it's a liveness probe, not an application event), and the
  matching `RtmpSession::send_stream_dry` / `send_stream_is_recorded` /
  `send_ping_request` helpers let an ingest broadcast the same UCM
  events to its publishers. `SetBufferLength` (the only 8-byte UCM
  payload — 4-byte stream id + 4-byte buffer length in ms) is validated
  on the wire and surfaces a `ProtocolViolation` if truncated. The rest
  of the protocol-control plumbing (Set Chunk Size, Window Ack Size,
  Set Peer Bandwidth) is handled transparently inside `poll_event`.
- **Injection-robust parser surface**. Every public decode entry
  point — AMF0 (`decode` / `decode_all`), AMF3 (`decode` / `decode_all`
  / `decode_data_message`), FLV (`parse_video` / `parse_audio`), the
  chunk-stream reader (`ChunkReader::read_message`), both handshake
  directions — is fuzzed against deterministic random byte streams
  (`tests/injection_robustness.rs`) and against structurally-corrupted
  RTMP frames (oversize lengths, forged fmt-1 chunks without prior
  fmt-0 state, truncated handshakes, wrong RTMP version bytes,
  `u32::MAX` strict-array counts). Every call returns `Result`,
  never panics, never spins, and never over-allocates. Stack-overflow
  protection: `amf::MAX_DECODE_DEPTH` and `amf3::MAX_DECODE_DEPTH`
  (both 64) cap nested-container recursion in both AMF dialects before
  the call stack runs out, so a forged onMetaData with 2_000 nested
  Objects surfaces a clean error rather than crashing.

- **Enhanced RTMP v1+v2 NetConnection `connect` capability
  negotiation** (Veovera 2023+2026). `RtmpClient::connect_with_capabilities`
  advertises a [`ConnectCapabilities`] block in the publisher's `connect`
  Command Object per `enhanced-rtmp-v2.pdf` §"Enhancing NetConnection
  connect Command": the v1 `fourCcList` strict-array, the v2 object
  maps `videoFourCcInfoMap` / `audioFourCcInfoMap` with per-codec
  bitmask values (`FourCcInfoMask.CanDecode = 0x01` /
  `CanEncode = 0x02` / `CanForward = 0x04`, with the `"*"` wildcard
  honoured), and the v2 `capsEx` u32 bitfield (`Reconnect = 0x01` /
  `Multitrack = 0x02` / `ModEx = 0x04` / `TimestampNanoOffset = 0x08`).
  The block is appended *after* the historical `videoFunction` field so
  legacy peers keep parsing the message correctly. `RtmpServer::set_capabilities`
  symmetrically lets a server stamp its OWN capability block into the
  `_result(connect)` info object alongside the
  `NetConnection.Connect.Success` status; the publisher's advertised
  block surfaces on `PublishRequest::capabilities` and the server's
  advertised block surfaces on `RtmpClient::server_capabilities()`. The
  empty / default block produces byte-identical output to the legacy
  pre-2023 connect command. Resolves the previous "high-level publish
  helper opts in once the configurable codec-list follow-up lands" note
  for both audio and video FourCC advertisements.
- **Enhanced RTMP v2 `ModEx` prelude** (Veovera 2026). The `ModEx`
  packet-type signal (`enhanced-rtmp-v2.pdf` §"ExVideoTagHeader" /
  §"ExAudioTagHeader") is decoded for both audio and video: a chain
  of size-prefixed `modExData` entries (`UI8 + 1` length, escaping to
  `0xFF` + `UI16 + 1` for 256..=65536-byte payloads) preceding the
  FourCC, each terminated by a `modExType | next-PacketType` nibble
  byte. The chain round-trips through `flv::parse_*` / `flv::build_*`
  via the new `VideoTag::mod_ex` / `AudioTag::mod_ex` (`Vec<flv::ModEx>`)
  fields, and the real PacketType is recovered from the chain so the
  packet adapters route a ModEx-prefixed tag transparently. The only
  subtype defined today, `TimestampOffsetNano` (a `bytesToUI24`
  sub-millisecond presentation offset, 0..=999_999 ns), is exposed via
  `{Video,Audio}Tag::timestamp_offset_nano`. The
  [`SourceRegistry`]/[`audio_to_packet`]/[`video_to_packet`] adapter
  now **folds that nanosecond offset onto the `Packet` timeline** at
  source: the timeline switched to `RTMP_TIME_BASE = 1/1_000_000_000`
  (nanoseconds), so `pts` and `dts` are emitted as
  `ms * RTMP_MS_TO_NS` and the per-message `TimestampOffsetNano` sum
  is added to the *presentation* time per spec — for audio both
  `pts == dts` receive the offset; for video only `pts` (decode
  timestamp stays unmodified, matching the §"ExVideoTagHeader"
  contract "without altering the core RTMP timestamp"). The exposed
  `RTMP_MS_TO_NS` (= 1_000_000) constant lets a consumer recover the
  wire ms value when needed. Multi-entry chains and out-of-band
  ModEx subtypes are summed via the typed accessor so future
  subtypes never feed the timestamp sum by accident.

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
// time_base 1/1_000_000_000 — nanoseconds, so Enhanced-RTMP-v2
// `TimestampOffsetNano` ModEx entries fold onto the same uniform
// `Packet` timeline). Codec ids are auto-detected from the
// publisher's first audio + video tags (h264, hevc, av1, vp9 in
// Enhanced-RTMP-v1 FourCC mode; vp8, h264 (avc1), vvc added in
// Enhanced-RTMP-v2 FourCC mode; h264, h263, vp6f / vp6a, flashsv /
// flashsv2 for legacy single-byte codec ids; opus, flac, ac3,
// eac3, mp3, aac in Enhanced-RTMP-v2 audio FourCC mode; aac, mp3,
// pcm_*, speex, nellymoser for legacy single-byte audio
// sound-format).
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
- `amf3::{encode, decode, decode_all, decode_data_message, encode_all, Amf3Value, Decoder}`
  plus `Amf3Value::to_amf0()` and `amf3::AVMPLUS_OBJECT_MARKER`
- `caps::{ConnectCapabilities, FourCcInfoMap}` plus the spec-mirroring
  `FOURCC_INFO_CAN_DECODE` / `CAPS_EX_*` / `OBJECT_ENCODING_*` /
  `FOURCC_WILDCARD` constants — Enhanced RTMP v1+v2 NetConnection
  `connect` capability negotiation per `enhanced-rtmp-v2.pdf`
  §"Enhancing NetConnection connect Command". Compose with
  `RtmpClient::connect_with_capabilities` (publisher) and
  `RtmpServer::set_capabilities` (ingest) for high-level use, or call
  `ConnectCapabilities::encode_into` / `from_amf0` directly to bridge
  to a custom command pipeline.
- `chunk::{ChunkReader, ChunkWriter, Message, MessageStreamKind}` —
  `Message::stream_kind()` lifts the raw `msg_stream_id: u32` field
  into a typed `Control` / `NetStream(id)` / `Reserved(raw)` view per
  RTMP Message Formats spec §4.1 + §5, and
  `Message::validate_protocol_control_invariants()` returns
  `Err(ProtocolViolation)` whenever a protocol-control message
  (`msg_type_id` 1..=6) carries a non-zero `msg_stream_id` — the spec
  §5 mandate "Protocol control messages MUST have message stream ID 0
  (called as control stream)" — or whenever the §4.1 reserved
  high-byte rule is violated.
- `aggregate::{parse_aggregate, build_aggregate}` — Aggregate Message
  (RTMP 1.0 §7.1.6 type 22) parser + builder. Splits a single
  `Message` of type id 22 into its component FLV-shaped sub-messages
  (audio / video / data / command), applying the §7.1.6 timestamp
  re-normalisation rule (`t_i + (aggregate.timestamp - t_0)`) and
  honouring the spec's "stream id of the aggregate overrides the
  stream ids of the sub-messages" override. Symmetrically packs a
  `&[Message]` slice into an aggregate carrying the correct §E.3
  `PreviousTagSize` back-pointers — useful when a publisher wants to
  cut chunk-header overhead by bundling several frames into one
  message before they hit `ChunkWriter::write_message`. **Routed
  end-to-end:** `RtmpSession::next_packet` decomposes incoming
  aggregates into a per-session queue and surfaces each sub as the
  same `StreamPacket::{Audio,Video,Metadata}` the publisher would
  have produced individually, including the spec's teardown-command
  detection on aggregated `closeStream` / `deleteStream` /
  `FCUnpublish` subs. `RtmpClient::poll_event` does the symmetric
  decomposition for server-pushed aggregates, and
  `RtmpClient::send_aggregate(&[Message])` is the outbound helper —
  every sub's `msg_stream_id` is overridden to the active publish
  stream id per §7.1.6, the aggregate is framed on `CSID_DATA`, and
  an empty slice is a no-op.
- `handshake::{client_handshake, server_handshake}`
- `flv::{parse_video, build_video, parse_audio, build_audio, ModEx}`
- `flv_file::{FlvWriter, FlvReader, FlvTag, FlvHeaderFlags,
  build_flv_header, build_flv_tag, DEFAULT_MAX_TAG_SIZE}` — FLV file
  / byte-stream serializer + parser (Annex E of `flv_v10_1.pdf`).
  `FlvWriter<W>` frames `VideoTag` / `AudioTag` plus AMF0 script-data
  tags into the on-disk `.flv` layout (9-byte file header +
  alternating `PreviousTagSize` / `FLVTAG` body). `FlvReader<R>` is
  the inverse: walks the §E.2 header, the §E.3 alternating
  back-pointers, and each §E.4.1 `FLVTAG`, surfacing each tag as a
  strongly-typed [`FlvTag`] (`Audio` / `Video` / `Script` / opaque
  `Unknown` for reserved `TagType` values). Verifies the §E.3
  `PreviousTagSize == 11 + DataSize` invariant on every tag and
  refuses to advance past a mismatch; bounds the per-tag `DataSize`
  by [`DEFAULT_MAX_TAG_SIZE`] (UI24 ceiling = 16 MiB) or a
  caller-supplied cap via [`FlvReader::with_max_tag_size`]; surfaces
  a clean `Error::UnexpectedEof` on a truncated header / payload and
  `Error::Other` on a forward-incompatible `Filter = 1` (Annex F
  encrypted body). Useful as a recorder for an `RtmpSession` (write
  each `StreamPacket` to an `io::Write` sink) and as the foundation
  for an HTTP-FLV bridge — the body of an HTTP-FLV response is
  exactly this byte stream, served with `Content-Type: video/x-flv`,
  and a downstream consumer can re-decode it with `FlvReader`
  without re-implementing the §E.3 walk.
- `message::build_*` — builders for every protocol-control /
  command message we emit
