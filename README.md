# oxideav-rtmp

[![CI](https://github.com/OxideAV/oxideav-rtmp/actions/workflows/ci.yml/badge.svg)](https://github.com/OxideAV/oxideav-rtmp/actions/workflows/ci.yml) [![crates.io](https://img.shields.io/crates/v/oxideav-rtmp.svg)](https://crates.io/crates/oxideav-rtmp) [![docs.rs](https://docs.rs/oxideav-rtmp/badge.svg)](https://docs.rs/oxideav-rtmp) [![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

Pure-Rust **RTMP** for the
[`oxideav`](https://github.com/OxideAV/oxideav) framework — all four
network roles: accept an incoming publisher (ingest), push your own
stream to a remote server (publish client), serve subscribers
(§4.2.1 `play`), and pull a remote stream (play client). Zero
external dependencies, blocking-thread-per-connection.

## Server (accept a publisher)

```rust
use oxideav_rtmp::{RtmpServer, StreamPacket};

let server = RtmpServer::bind("0.0.0.0:1935")?;
let req = server.accept()?;                         // blocks until a publisher connects

if req.stream_name != "my-secret-key" {
    req.reject("unauthorized")?;
    return Ok(());
}

let mut session = req.accept()?;                    // sends NetStream.Publish.Start
while let Some(pkt) = session.next_packet()? {
    match pkt {
        StreamPacket::Video { timestamp, tag } => { /* AVC bytes in `tag.body` */ }
        StreamPacket::Audio { timestamp, tag } => { /* AAC bytes in `tag.body` */ }
        StreamPacket::Metadata(meta)           => { /* onMetaData (AMF0 or AMF3, bridged to AMF0) */ }
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

## Server (serve subscribers / relay)

The same listener accepts both directions —
`RtmpServer::accept_any` classifies each connection by the command
the peer issues after `createStream` (`publish` or `play`):

```rust
use oxideav_rtmp::{RtmpServer, SessionRequest};

let server = RtmpServer::bind("0.0.0.0:1935")?;
match server.accept_any()? {
    SessionRequest::Publish(req) => { /* ingest as before */ }
    SessionRequest::Play(req) => {
        // §4.2.1 play: stream_name + optional Start/Duration/Reset
        // (and any §3.7 SetBufferLength sent ahead of the play).
        let mut session = req.accept()?;   // StreamBegin → Play.Reset? → Play.Start
        session.send_metadata(&meta)?;
        session.send_video(ts, &video_tag)?;
        session.send_audio(ts, &audio_tag)?;
        // …or relay a whole publisher packet: session.forward(&pkt)?
        // Subscriber commands (pause / seek / receiveAudio /
        // receiveVideo / playlist play / play2) surface as events:
        while let Some(ev) = session.next_event()? { /* … */ }
        session.close()?;                  // UserControl StreamEOF
    }
}
```

`accept_recorded()` announces `StreamIsRecorded` first (recorded /
seekable content); `reject()` refuses with
`NetStream.Play.StreamNotFound`. `notify_pause` / `notify_seek` emit
the §4.2.7 / §4.2.8 status replies. `serve_sessions` is the
thread-per-connection loop over both directions; the publish-only
`accept` / `serve` politely refuse play connections.

## Client (pull / play a remote stream)

```rust
use oxideav_rtmp::{RtmpPlayer, PlayerPacket};

let mut player = RtmpPlayer::connect("rtmp://origin.example.com/vod/clip")?;
while let Some(pkt) = player.next_packet()? {
    match pkt {
        PlayerPacket::Video { timestamp, tag } => { /* coded frames */ }
        PlayerPacket::Audio { timestamp, tag } => { /* coded frames */ }
        PlayerPacket::Metadata(meta)           => { /* onMetaData */ }
        PlayerPacket::Status { code, .. }      => { /* NetStream.* */ }
        PlayerPacket::Control(_)               => { /* UCM events */ }
    }
}
player.close()?; // deleteStream
```

`connect_with_options` exposes the §4.2.1 Start / Duration / Reset
arguments, a pre-play §3.7 `SetBufferLength`, and the Enhanced-RTMP
capability block. Mid-stream: `pause(ms)` / `resume(ms)` (§4.2.8),
`seek(ms)` (§4.2.7), `set_receive_audio` / `set_receive_video`
(§4.2.4 / §4.2.5), dynamic-playlist `play(name, …)` (§4.2.1) and
`play2(params)` (§4.2.2). `Ok(None)` marks the server's
`StreamEOF`; a refused play surfaces as `Error::Rejected` carrying
`NetStream.Play.StreamNotFound`.

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

- **Transport.** RTMP (`rtmp://`, plain TCP port 1935). No RTMPS yet —
  wrap our `Read + Write` with rustls if you need it.
- **Direction.** Both. Publish: the server accepts incoming
  publishers (`RtmpSession`) and the client pushes to remote servers
  (`RtmpClient`). Play (RTMP 1.0 §4.2.1): the server serves
  subscribers (`PlaySession` — §4.2.1 Figure 5 acceptance sequence,
  typed pause / seek / receiveAudio / receiveVideo / playlist-play /
  play2 events, §4.2.7 / §4.2.8 notify replies) and the client pulls
  remote streams (`RtmpPlayer`, full §4.2 control surface). A
  publisher → subscriber relay is one `session.forward(&pkt)` per
  ingested packet. Graceful teardown on every path drains the socket
  until the peer's FIN (bounded) so the close-time RST can never
  discard in-flight final frames.
- **AMF0 + AMF3 command flow.** The AMF0 codec covers every
  serializable marker from the spec — Number, Boolean, String, Object,
  Null, Undefined, Reference (`0x07`), ECMA array, strict array, Date,
  long string, Unsupported (`0x0D`), XML Document (`0x0F`), and Typed
  Object (`0x10`) — round-tripping each. Object references are
  dereferenced transparently to a clone of their target (so a
  reference-deduplicated `onMetaData` decodes correctly), and typed
  objects are themselves reference-eligible; the two deliberately
  unserializable markers (Movieclip `0x04`, RecordSet `0x0E`) are
  rejected as unknown. The
  [`amf3`] module ships a complete AMF3 encoder + decoder (all thirteen
  markers plus the three reference tables), and AMF3 data / command
  messages route end-to-end: a type-15 AMF3 data message (an AMF0 frame
  switching to AMF3 via the `avmplus-object-marker` `0x11`) is decoded,
  bridged onto `Amf0Value`, and surfaced through the same
  `StreamPacket::Metadata` path; type-17 AMF3 commands feed the same
  stream-teardown detection. `RtmpClient::send_metadata_amf3` emits the
  AMF3-encoded form in the Enhanced RTMP v2 clarified framing — the
  type-15/16/17 payload starts with a one-byte *format selector* (only
  format 0 = AMF0 values is defined; each AMF3 value is introduced by
  the non-sticky `0x11` marker) — and `amf3::decode_message_to_amf0`
  decodes that framing plus both legacy selector-less shapes on every
  type-15/17 dispatch path, server and client. The server's
  negotiation driver also accepts type-17 commands, so an
  objectEncoding-3 peer can run its whole `connect` → `createStream` →
  `publish` / `play` sequence in AMF3. The AMF0 decoder also honours an inline
  `avmplus-object-marker` (`0x11`) mid-stream (§3.1): the following value
  is decoded as a self-contained AMF3 value and bridged back onto
  `Amf0Value`, so a mixed AMF0/AMF3 packet parses through the ordinary
  `amf::decode_all` path. Externalizable objects are decodable via
  `Decoder::register_externalizable` (the caller supplies a body-length
  resolver for a known class; an unregistered class is refused, not
  guessed). RTMFP and RTMP Encrypted (the RC4/DH transport cipher)
  remain unimplemented.
- **Digest (HMAC-SHA256) handshake.** Both RTMP handshakes are
  supported. The server auto-negotiates per connection
  (`handshake::server_handshake_negotiated`): a zero — or junk —
  version field takes the historical echo path, a validating digest
  C1 gets a digested S1 (same 764-byte block order the client picked)
  plus a chained-response S2, and the client's C2 response digest is
  verified. Clients opt in via
  `RtmpClient::connect_with_digest_handshake` /
  `PlayOptions::digest_handshake`, emitting a digested C1 (schema 1)
  and falling back to the echo exchange automatically when the server
  doesn't digest; the outcome — including whether the peer's response
  digest verified — surfaces as [`HandshakeKind`]. The low-level
  pieces (both block orders, the `mod 728` digest / `mod 632` DH-key
  offset arithmetic, install/verify/detect, response chaining) are
  public in [`handshake`], carried by a dependency-free FIPS 180-4
  SHA-256 + RFC 2104 HMAC validated against the published NIST /
  RFC 4231 vectors.
- **Shared Objects.** The [`shared_object`] module parses + builds the
  message-type-19 (AMF0) / 16 (AMF3) Shared Object bodies: the header
  (UI16 name length + bare UTF-8 name + UI32 version + UI32 flags with
  the persistent bit + 4 reserved bytes) and the back-to-back event
  stream (UI8 type + UI32 length + data), with all eleven documented
  event types typed as [`SoEvent`] — Use / Release / Request Change /
  Change / Success / Send Message / Status / Clear / Remove / Request
  Remove / Use Success. Property and handler names are bare
  UI16-prefixed strings; values are full AMF0 or AMF3 values (the
  struct is generic over the flavour, and `parse_shared_object`
  bridges either onto AMF0). Unknown event codes round-trip verbatim
  for relays; truncations and overruns are clean errors. SO traffic is
  live on all four connection surfaces: inbound messages surface as
  `StreamPacket::SharedObject` / `ClientEvent::SharedObject` /
  `PlayerPacket::SharedObject` / `PlaySessionEvent::SharedObject`, and
  every surface has a `send_shared_object` for the reply direction.
- **`@setDataFrame` / `@clearDataFrame`.** The reserved data-frame
  control names are wired on every surface: `RtmpClient::send_metadata`
  (the `onMetaData` special case), `send_data_frame(handler, value)`,
  `clear_data_frame(handler)` / `clear_metadata()` on the publish
  side; on the ingest side the session strips the control prefix,
  maintains the stored-frame state (`RtmpSession::data_frames`, upsert
  on set, removal on clear — the state a server replays to late
  subscribers via `PlaySession::send_data`), and surfaces
  `StreamPacket::Metadata` / `DataFrame` / `DataFrameCleared`;
  `parse_data_frame` / `DataFrameCommand` classify a decoded data
  message (type 18, or type 15 after the AMF3 bridge), and
  `PlaySession::forward` relays a live non-`onMetaData` frame in its
  bare subscriber shape.
- **NetConnection `call` (RPC).** §7.2.1.2 both ways on every
  surface: an RPC's procedure name rides the wire command-name field,
  so any non-built-in command is a call aimed at the application.
  `RtmpClient::call` / `RtmpPlayer::call` issue RPCs (transaction id
  allocated when a response is expected, the spec's 0 otherwise);
  inbound RPCs surface as `ClientEvent::Call` / `PlayerPacket::Call` /
  `StreamPacket::Call` / `PlaySessionEvent::Call` with
  `reply_call_result` / `reply_call_error` helpers; server-initiated
  `send_call` replies come back as the matching `CallReply` variants.
  The §7.2.1 NetConnection `close` command tears the session down like
  `closeStream` / `deleteStream`.
- **Timestamp rollover.** RTMP timestamps are 32-bit milliseconds and
  roll over every ~49.7 days; §4 mandates RFC 1982 serial-number
  arithmetic. The public [`TimestampUnwrapper`] folds each raw
  timestamp's wrapping signed step onto a monotonic 64-bit timeline,
  and both `PacketSource` adapters use it so `pts`/`dts` keep growing
  across rollovers.
- **Peer bandwidth limit types.** §5.4.5's Hard / Soft / Dynamic
  semantics run through [`PeerBandwidthLimiter`] on all four
  steady-state paths (Hard adopts, Soft takes the smaller of the
  indicated and in-effect windows, Dynamic acts as Hard only when the
  previous type was Hard); when the effective window changes the peer
  gets the SHOULD-mandated Window Acknowledgement Size reply.
- **Codecs.** H.264 + AAC are the canonical legacy payloads, plus the
  Enhanced RTMP FourCC video codecs — `hvc1` (HEVC), `av01` (AV1),
  `vp09` (VP9), `vp08` (VP8), `avc1` (FourCC-mode AVC), `vvc1` (VVC) —
  and FourCC audio codecs — `Opus`, `fLaC`, `ac-3`, `ec-3`, `.mp3`,
  `mp4a`. Sequence-start config records, `CodedFrames`, `CodedFramesX`
  (CTS=0 omitted), `SequenceEnd`, `MPEG2TSSequenceStart` (the MPEG-2 TS
  carriage variant; `av01` carries an `AV1VideoDescriptor`), and the HDR
  `colorInfo` metadata frame round-trip via `flv::parse_video` /
  `build_video` / `parse_audio` / `build_audio`. `SequenceEnd` has a
  typed surface on both pipelines — `VideoTag` / `AudioTag`
  `is_ex_sequence_end()` + `sequence_end_tag()` — alongside
  `VideoTag::is_ex_mpeg2ts_sequence_start()` /
  `mpeg2ts_video_descriptor()` / `mpeg2ts_sequence_start_tag()`. The
  crate passes through FLV tag bytes verbatim, so additional codecs
  (MP3, H.263, Speex, …) keep working too.
- **Audio silence message.** A zero-length audio message is the
  Enhanced RTMP v2 §"ExAudioTagHeader" *silence* signal (an empty
  message with no AudioTagHeader). `flv::parse_audio_message` lifts it
  to `AudioMessage::Silence` (a non-empty payload becomes
  `AudioMessage::Tag`), `is_silence_payload` classifies a raw payload,
  and `build_silence_audio` / `build_audio_message` emit the inverse.
  Per spec `AudioPacketType.SequenceEnd` carries "no less than the same
  meaning" as silence.
- **Seek command frames.** A `VideoFrameType.Command` tag (FrameType
  `5`) carries no coded video — just a single `videoCommand` byte
  signalling the bounds of a client-side seeking sequence
  (`StartSeek` / `EndSeek`). `VideoTag::video_command()` lifts it (for
  both legacy FLV §E.4.3.1 framing and the Enhanced-RTMP v2
  FourCC framing), `VideoTag::is_command()` classifies it, and
  `VideoTag::command_tag` / `command_tag_ex` build the inverse. The
  command path skips the AVC packet-type / SI24 composition-time prefix
  that a coded frame would carry, and an unknown command value passes
  through verbatim.
- **HDR `colorInfo`.** `VideoTag::color_info()` lifts the `colorInfo`
  object into a strongly-typed [`ColorInfo`] view — `colorConfig`
  (bit depth + ITU-T H.273 primaries / transfer / matrix indices),
  `hdrCll` content light level, and `hdrMdcv` (SMPTE ST 2086 mastering
  display) — and `color_info_tag` rebuilds the outbound tag. Every
  property is `Option<f64>` so a partial object round-trips; the
  reset signal (`Undefined` or empty `{}`) surfaces as
  [`ColorInfo::is_reset`].
- **Typed `onMetaData`.** [`OnMetaData::from_amf0`] lifts the Enhanced
  RTMP v2 §"Enhancing onMetaData" typical-properties table —
  `duration`, `width` / `height`, `framerate`, `videodatarate`,
  `audiosamplerate`, `stereo`, `audiocodecid` / `videocodecid`, … —
  into named `Option` fields; any property outside the table is kept
  verbatim in `extra`, so `OnMetaData::to_amf0` (which emits the
  spec-mandated ECMA array) round-trips losslessly. When a codec id is
  a FourCC encoded as a number ("Opus" == `0x4F707573`),
  `audio_fourcc()` / `video_fourcc()` reconstruct the four ASCII bytes
  while leaving legacy single-byte CodecIDs as `None`. The v2
  `audioTrackIdInfoMap` / `videoTrackIdInfoMap` per-track maps are
  preserved as raw AMF for callers doing multitrack selection.
- **Multichannel + multitrack audio.** `AudioTag::multichannel_config()`
  lifts the `MultichannelConfig` body into a typed view, and the
  `Multitrack` AudioPacketType / VideoPacketType is wired end-to-end —
  `OneTrack` / `ManyTracks` / `ManyTracksManyCodecs` all round-trip via
  `multitrack` / `multitrack_tag`, with reserved `multitrackType`
  values passed through verbatim. The `audio_channel` /
  `audio_channel_mask` submodules name all 24 spec-defined positions
  including the 22.2 surround extras (SMPTE ST 2036-2).
- **Graceful session close.** `RtmpSession::close` emits a
  `UserControl StreamEOF` before `onStatus("NetStream.Unpublish.Success")`,
  flushes the chunk writer, and half-closes the write side.
  `RtmpClient::poll_event` surfaces every server-originated User
  Control Message (`StreamBegin` / `StreamEOF` / `StreamDry` /
  `StreamIsRecorded` / `PingResponse`) as typed `ClientEvent` variants
  alongside `onStatus`, `_result`, and `_error` replies.
  Server-originated `PingRequest` is auto-replied internally. The rest
  of the protocol-control plumbing (Set Chunk Size, Window Ack Size,
  Set Peer Bandwidth) is handled transparently inside `poll_event`.
- **Acknowledgement window.** The chunk reader counts every byte it
  consumes as the §5.3 sequence number; both peers honour the §5.3
  obligation to acknowledge after receiving a window's worth of bytes.
  A peer's §5.5 Window Acknowledgement Size / §5.6 Set Peer Bandwidth
  is captured, and an `Acknowledgement` is emitted the first time the
  running count crosses each window, re-arming only after another full
  window. `ChunkReader::ack_due` is the public hook. With no window
  negotiated the obligation stays dormant.
- **Injection-robust parser surface.** Every public decode entry point —
  AMF0, AMF3, FLV, the chunk-stream reader, both handshake directions —
  is fuzzed against deterministic random byte streams and
  structurally-corrupted frames (`tests/injection_robustness.rs`).
  Every call returns `Result`, never panics, spins, or over-allocates.
  `amf::MAX_DECODE_DEPTH` and `amf3::MAX_DECODE_DEPTH` (both 64) cap
  nested-container recursion before the call stack runs out.
- **NetConnection `connect` capability negotiation.**
  `RtmpClient::connect_with_capabilities` advertises a
  [`ConnectCapabilities`] block in the publisher's `connect` Command
  Object: the v1 `fourCcList` strict-array, the v2
  `videoFourCcInfoMap` / `audioFourCcInfoMap` per-codec bitmask maps
  (with the `"*"` wildcard), and the v2 `capsEx` u32 bitfield. The
  block is appended after the historical `videoFunction` field so
  legacy peers keep parsing. `RtmpServer::set_capabilities` symmetrically
  stamps the server's own block into `_result(connect)`; the empty /
  default block produces byte-identical output to the legacy command.
- **Reconnect Request.** The `NetConnection.Connect.ReconnectRequest`
  status event is wired end-to-end:
  `RtmpSession::send_reconnect_request` emits it,
  `RtmpClient::poll_event` surfaces it as
  `ClientEvent::ReconnectRequest { tc_url, description }`, and
  `RtmpClient::resolve_reconnect_url` / `resolve_tc_url` apply the
  spec's resolution rule for all four documented `tcUrl` shapes.
  Neither side tears the session down on the event.
- **Inbound NetStream control commands.** A server session surfaces a
  peer-issued RTMP 1.0 §4.2 NetStream control command — `play`,
  `play2`, `pause`, `seek`, `receiveAudio`, `receiveVideo` — as a
  typed [`StreamPacket::Command`] carrying [`NetStreamCommand`], so a
  server application can react (e.g. honour `receiveAudio false` by
  suspending audio forwarding). `NetStreamCommand::parse` classifies a
  decoded AMF0/AMF3 command frame and `to_message` is the byte-level
  inverse (transaction id 0, Null Command Object per §4.2); the
  optional `play` Start/Duration/Reset trailing fields round-trip with
  spec defaults (−2 / −1 / false) materialised only when a later field
  is present, and `play2`'s parameter object is preserved verbatim.
  Teardown commands (`closeStream` / `deleteStream` / `FCUnpublish`)
  still end the session silently and are not surfaced as commands.
- **`ModEx` prelude.** The `ModEx` packet-type signal (a chain of
  size-prefixed `modExData` entries preceding the FourCC) is decoded
  for both audio and video, round-tripping through `VideoTag::mod_ex` /
  `AudioTag::mod_ex`. The `TimestampOffsetNano` subtype (a
  sub-millisecond presentation offset) is exposed via
  `timestamp_offset_nano` and folded onto the `Packet` timeline at
  source — the timeline runs in nanoseconds
  (`RTMP_TIME_BASE = 1/1_000_000_000`), and the per-message offset is
  added to the presentation time (for video, decode timestamp stays
  unmodified).

## Pipeline integration (`SourceRegistry`)

Wire `rtmp://` URIs into the workspace's
[`oxideav_core::SourceRegistry`] so the pipeline executor reads RTMP
streams via the same dispatch as `file://` and `http(s)://`:

```rust
use oxideav_core::SourceRegistry;
let mut reg = SourceRegistry::new();
oxideav_rtmp::register(&mut reg);
// `rtmp://host:port/app/stream-name` opens a one-shot listener that
// accepts a single publisher and surfaces it as a PacketSource
// (audio = stream 0, video = stream 1, both time_base 1/1_000_000_000).
let _src = reg.open("rtmp://0.0.0.0:1935/live/secret-key")?;
// `rtmp-play://host:port/app/stream-name` is the pull direction:
// dial the named remote server as a §4.2.1 play client and surface
// the received stream through the identical PacketSource layout.
let _pulled = reg.open("rtmp-play://origin.example.com:1935/vod/clip")?;
```

Codec ids are auto-detected from the publisher's first audio + video
tags (FourCC + legacy single-byte modes). The opener is listen-style:
each `open()` binds the URL's `host:port`, accepts one publisher,
validates the announced `app` + `stream_name` against the URL path, and
hands packets to the registry. For multi-client service, use
[`RtmpServer::serve`] directly.

## Reusable building blocks

The lower-level modules are public so callers can compose something
non-standard:

- `amf::{encode, decode, encode_command, Amf0Value}`
- `amf3::{encode, decode, decode_all, decode_data_message, encode_all, Amf3Value, Decoder}`
- `caps::{ConnectCapabilities, FourCcInfoMap}` plus the spec-mirroring
  constants; compose with `connect_with_capabilities` /
  `set_capabilities` or call `encode_into` / `from_amf0` directly.
- `chunk::{ChunkReader, ChunkWriter, Message, MessageStreamKind}` —
  `Message::stream_kind()` lifts `msg_stream_id` into a typed view;
  `validate_protocol_control_invariants()` enforces the §5 rule that
  protocol-control messages carry stream id 0;
  `ChunkReader::abort_partial(csid)` applies an inbound Abort Message.
  The writer implements the full §5.3.1.3 extended-timestamp matrix
  (December 2012 revision): the ext field rides every fmt-3 chunk —
  head or continuation — whose most recent fmt-0/1/2 chunk indicated
  one, fmt-1/2 ext fields carry the 32-bit *delta*, and a fmt-3 head
  chunk is only chosen when the message's delta equals the delta
  registered on the csid (§5.3.1.2.4).
- `aggregate::{parse_aggregate, build_aggregate}` — Aggregate Message
  (type 22) parser + builder with the §7.1.6 timestamp re-normalisation
  and stream-id override. Routed end-to-end: `next_packet` /
  `poll_event` decompose incoming aggregates into the same
  `StreamPacket` the publisher would have produced individually, and
  `RtmpClient::send_aggregate(&[Message])` is the outbound helper.
- `handshake::{client_handshake, server_handshake}`
- `flv::{parse_video, build_video, parse_audio, build_audio, ModEx}`
- `flv_file::{FlvWriter, FlvReader, FlvTag, FlvHeaderFlags,
  build_flv_header, build_flv_tag, DEFAULT_MAX_TAG_SIZE}` — FLV file /
  byte-stream serializer + parser. `FlvWriter<W>` frames tags into the
  on-disk `.flv` layout; `FlvReader<R>` walks the header, the
  alternating `PreviousTagSize` back-pointers, and each `FLVTAG`,
  verifying the `PreviousTagSize == 11 + DataSize` invariant and
  bounding per-tag `DataSize` by [`DEFAULT_MAX_TAG_SIZE`] (16 MiB) or a
  caller-supplied cap. Useful as a session recorder and as the
  foundation for an HTTP-FLV bridge.
- `flv_crypt::{parse_encrypted_body, EncryptedTag, FilterParams}` — FLV
  Encryption envelope (Annex F). A tag whose §E.4.1 `Filter` bit is set
  carries the in-clear §F.3.1 `EncryptionTagHeader` (NumFilters /
  FilterName / Length) and §F.3.2 `FilterParams` before the §F.3.3
  ciphered body. `FlvReader` now surfaces such a tag as
  `FlvTag::Encrypted { tag_type, crypt }` (the underlying audio / video
  / script type stays in clear so a player can route without
  decrypting) instead of failing the stream; `FlvWriter::write_encrypted_tag`
  is the inverse. Both `FilterName = "Encryption"` (whole-packet) and
  `"SE"` (Selective Encryption, with the per-packet `EncryptedAU` bit
  and optional 16-byte AES-CBC IV) round-trip. Decryption itself is out
  of scope — the §F.2.5 content key comes from a DRM-server protocol the
  spec leaves undefined — so the envelope is parsed but the body is
  preserved verbatim.
- `message::build_*` — builders for every protocol-control / command
  message we emit, plus the §4.2 `NetStream.*` onStatus code-string
  constants (`STATUS_PLAY_START`, `STATUS_PAUSE_NOTIFY`, …) and
  `build_on_meta_data` (the server→subscriber data-message shape,
  without the publish-side `@setDataFrame` prefix).
- `message::UserControlEvent` — typed view of a User Control Message
  body (§3.7 / §7.1.7). `parse(payload)` classifies the seven
  spec-defined variants or `Unknown` for reserved / future types;
  `to_message()` is the byte-for-byte inverse and the `Unknown` variant
  round-trips verbatim. Spec variants validate their fixed event-data
  size on parse.

## License

MIT — see [LICENSE](LICENSE).
