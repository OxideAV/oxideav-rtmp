# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Other

- typed SequenceEnd surface on both pipelines (Enhanced RTMP v2): VideoTag / AudioTag is_ex_sequence_end + sequence_end_tag builders for VideoPacketType.SequenceEnd / AudioPacketType.SequenceEnd (empty body, round-trips through build/parse); the spec notes AudioPacketType.SequenceEnd has "no less than the same meaning as a silence message"
- typed VideoPacketType.MPEG2TSSequenceStart (Enhanced RTMP v2 §"ExVideoTagBody"): VideoTag::is_ex_mpeg2ts_sequence_start / mpeg2ts_video_descriptor accessors + mpeg2ts_sequence_start_tag builder for the MPEG-2 TS carriage sequence-start variant (av01 AV1VideoDescriptor body); round-trips through build_video / parse_video with no SI24 CTS, mutually exclusive with PacketTypeSequenceStart
- audio silence message (Enhanced RTMP v2 §"ExAudioTagHeader"): a zero-length audio payload is now recognised as the spec-defined silence signal via the new AudioMessage enum + parse_audio_message / build_audio_message / is_silence_payload / build_silence_audio helpers (parse_audio still rejects an empty slice since silence carries no AudioTagHeader)
- typed OnMetaData view (Enhanced RTMP v2 §"Enhancing onMetaData"): OnMetaData::from_amf0 / to_amf0 lift the spec's typical-properties table (audiocodecid, videocodecid, duration, width/height, framerate, stereo, …) into named fields, preserve unknowns verbatim in `extra`, re-encode as the mandated ECMA array, and decode the codec-id FourCC note ("Opus" == 0x4F707573) via audio_fourcc / video_fourcc; the v2 audioTrackIdInfoMap / videoTrackIdInfoMap per-track maps round-trip verbatim
- typed VideoFrameType.Command (StartSeek/EndSeek) for both legacy FLV §E.4.3.1 and Enhanced-RTMP v2 framing: VideoTag::video_command / is_command accessors + command_tag / command_tag_ex builders; the command byte (no AVC packet-type, no SI24 CTS) round-trips
- parse FLV Encryption envelope (Annex F.3.1/F.3.2): FlvReader surfaces a Filter=1 tag as FlvTag::Encrypted (EncryptionTagHeader + FilterParams + ciphered body) instead of failing; FlvWriter::write_encrypted_tag inverse; both "Encryption" and Selective-Encryption ("SE") filters round-trip
- typed NetStreamCommand (RTMP 1.0 §4.2 play/play2/pause/seek/receiveAudio/receiveVideo) parser + builder; server surfaces inbound ones as StreamPacket::Command

## [0.0.6](https://github.com/OxideAV/oxideav-rtmp/compare/v0.0.5...v0.0.6) - 2026-06-15

### Other

- strongly-typed colorInfo HDR metadata for VideoPacketType.Metadata
- decode AMF0 object references (marker 0x07) per FLV §E.4.4.2
- decode externalizable objects via registered per-class handlers (§3.12 U29O-traits-ext)
- RTMP §5.3 Acknowledgement honoured on received-byte window
- Enhanced RTMP v2 NetConnection.Connect.ReconnectRequest end-to-end
- RTMP §5.2 Abort Message builder + reader partial-discard
- typed UserControlEvent enum + round-trip parser
- drop release-plz.toml — use release-plz defaults across the workspace
- bump publisher-close drain windows for Ubuntu CI scheduling
- RTMP §3.7 StreamDry / StreamIsRecorded / PingResponse + builders
- typed MessageStreamKind accessor + spec-§5 protocol-control invariant validator
- bind set_read_timeout to the reader's actual socket clone
- replace drop(client) with close() so Windows CI doesn't race the flush
- route Aggregate Messages (type 22) through next_packet + poll_event
- fold ModEx TimestampOffsetNano onto the Packet timeline
- Aggregate Message (type 22) parser + builder
- Enhanced RTMP v1+v2 NetConnection connect capability negotiation
- rephrase FlvReader::with_max_tag_size docs
- FLV file / byte-stream reader (Annex E)
- FLV file / byte-stream writer (Annex E)

### Added

- **Strongly-typed `colorInfo` HDR metadata for `VideoPacketType.Metadata`**
  (`src/flv.rs`). Enhanced RTMP §"Metadata Frame" defines the
  `VideoPacketType.Metadata` (= 4) video message as an AMF-encoded
  sequence of `[name, value]` pairs, the only defined name being
  `"colorInfo"` — an HDR metadata object carrying `colorConfig`
  (bitDepth + the ITU-T H.273 colourPrimaries / transferCharacteristics /
  matrixCoefficients enumeration indices), `hdrCll` (maxFall / maxCLL
  content light level in cd/m2) and `hdrMdcv` (SMPTE ST 2086:2018
  mastering-display chromaticity coordinates + min/max luminance). The
  body previously passed through as opaque AMF bytes; it now lifts into
  the typed [`ColorInfo`] / [`ColorConfig`] / [`HdrCll`] / [`HdrMdcv`]
  views via `VideoTag::color_info()`, and `VideoTag::color_info_tag(fourcc,
  &ColorInfo)` rebuilds the matching outbound metadata tag. Every property
  is `Option<f64>` (AMF's native double, kept byte-exact rather than
  coerced to an integer that would lose a fractional luminance value), and
  each sub-object is `Option` so a partial `colorInfo` — e.g. only
  `colorConfig` — round-trips. The spec's "reset to original color state"
  signal is preserved: an `Undefined` value (the RECOMMENDED form) and an
  empty `{}` object both decode to [`ColorInfo::is_reset`], and a reset
  `ColorInfo` re-encodes as `Undefined`. A `colorInfo` value of the wrong
  AMF type is a clean `Error::Other`; a metadata tag with a different
  (future) pair name yields `Ok(None)` rather than an error. Seven tests
  cover full-HDR10 round-trip, colorConfig-only partial, the two reset
  forms, the non-metadata-tag / non-colorInfo-name `None` cases, and the
  wrong-type rejection.
- **AMF0 object references (marker 0x07) — decoded and dereferenced
  transparently** (`src/amf.rs`). The FLV v10.1 spec §E.4.4.2
  `SCRIPTDATAVALUE` table defines `Type == 7` with the wire shape
  `IF Type == 7 { UI16 }` — a 16-bit big-endian index into the table of
  complex objects (Object, ECMA array, strict array) serialized so far
  in the same context. The decoder previously rejected marker 0x07
  outright, so any `onMetaData`/command payload that used a reference to
  deduplicate a repeated complex value failed to decode entirely. The
  decoder now maintains a per-context reference table (scoped to one
  `decode_all` packet, or one top-level value for `decode`), appends
  each complex value as it is decoded — reserving the slot *before* the
  body so a reference appearing inside that body still resolves — and
  resolves a reference to a clone of the indexed value. Callers never
  see a `Reference` variant in the value graph; encoding always emits
  the expanded (inline) form, which is byte-valid AMF0. An
  out-of-range or truncated reference index surfaces a clean
  `InvalidAmf0` error rather than a panic. Six tests cover prior-object
  resolution, second-object indexing, in-body references, out-of-range
  and truncated indices, and the per-value scope of `decode`.
- **AMF3 §3.12 `U29O-traits-ext` externalizable objects — decodable via
  registered per-class handlers** (`src/amf3.rs`). The spec encodes an
  externalizable object's body as "an indeterminable number of bytes as
  `*(U8)`" whose framing is a private agreement between the sending and
  receiving classes; the generic decoder cannot know where the body
  ends and so previously refused every externalizable object outright,
  even though the encoder already re-emitted the `externalizable_body`
  field verbatim. `Decoder::register_externalizable(class_name, reader)`
  closes that asymmetry: a caller that knows a specific class's
  `IExternalizable.writeExternal` framing registers a body-length
  resolver (`ExternalizableReader = Box<dyn Fn(&[u8], usize) ->
  Result<usize>>`), and `decode` then captures exactly that many body
  bytes into `Amf3Value::Object::externalizable_body`, advances `pos`
  past them, and inserts the object into the object reference table like
  any other complex value. An over-long body length is rejected before
  any out-of-bounds read; an unregistered externalizable class is still
  refused loudly (the decoder never guesses). Handlers are decoder
  configuration and survive `reset_tables`. Six tests: fixed-length and
  length-prefixed handlers, decode→encode wire round-trip, overrun
  rejection, handler persistence across reset, and object-reference
  participation.

- **RTMP 1.0 §5.3 Acknowledgement / §5.5 Window Acknowledgement Size —
  honoured end-to-end** (`src/chunk.rs`, `src/server.rs`,
  `src/client.rs`). Until now both peers advertised a Window
  Acknowledgement Size but neither side ever *sent* the §5.3
  Acknowledgement the spec mandates ("the client or the server sends
  the acknowledgment to the peer after receiving bytes equal to the
  window size") — the client code even carried a `// future
  refinement` comment in its control-message branch. `ChunkReader` now
  counts every byte it consumes off the wire (basic header, message
  header, extended timestamp, payload) as the §5.3 sequence number via
  a new `read_exact_counted` funnel, stores the peer-negotiated window
  (`set_window_ack_size`, fed from inbound §5.5 Window Ack Size and the
  §5.6 Set Peer Bandwidth output-bandwidth value, which the spec
  defines as equal to the window size), and exposes `ack_due()` —
  returns `Some(seq)` the first time the received-byte count crosses
  the window, re-arming only after another full window so a steady
  stream never spams acks. `RtmpSession::next_packet` (server) and
  `RtmpClient::poll_event` (client) call it after each wire read and
  emit `build_ack(seq)` when one is owed; both setup paths
  (`drive_until_publish` / `wait_for_result`) seed the window so the
  obligation is live before the first media frame. `received_bytes()`
  / `window_ack_size()` accessors round out the public surface.
  Resetting the window re-bases the byte accounting so an
  already-counted byte never instantly owes an ack, and with no window
  negotiated the obligation stays dormant (byte-identical to the
  pre-§5.3 behaviour). New `tests/acknowledgement_window.rs` drives a
  raw publisher (built from the public `chunk` / `message` /
  `handshake` modules) that advertises a 256-byte window and confirms
  the real `RtmpServer` acks back with a plausible sequence number;
  four `chunk.rs` unit tests cover the byte-count, window-crossing,
  one-ack-per-window, and re-base behaviours.
- **Enhanced RTMP v2 Reconnect Request — end-to-end**
  (`src/message.rs`, `src/server.rs`, `src/client.rs`). Round 277 wires
  the `NetConnection.Connect.ReconnectRequest` status event from
  `enhanced-rtmp-v2.pdf` §"Reconnect Request" — until now the crate
  advertised the matching `capsEx` `Reconnect` bit (`CAPS_EX_RECONNECT`)
  during connect-capability negotiation but had no way to *send* or
  *react to* the event itself. New
  `message::build_reconnect_request(tc_url, description)` emits the
  spec's NetConnection-level onStatus shape — `["onStatus", 0.0, null,
  info]` on message stream 0, with the Info Object carrying the
  mandatory `code = NetConnection.Connect.ReconnectRequest`
  (exported as `RECONNECT_REQUEST_CODE`) + `level = "status"` pairs
  and the optional `tcUrl` / `description` properties (omitted from
  the wire when `None`, per their "optional" marking in the spec's
  Info Object table). `RtmpSession::send_reconnect_request` is the
  ingest-side helper — used "prior to the shutdown of the live
  streaming server or when the server intends to remap the client to
  another server instance," after which the old server keeps
  processing publisher messages per spec (so `next_packet` pumping
  continues unchanged). On the publisher side,
  `RtmpClient::poll_event` now classifies the event into the new typed
  `ClientEvent::ReconnectRequest { tc_url, description }` variant
  (code match alone is not enough — the spec says level MUST be
  `status`, so a mismatched level falls through as a plain
  `OnStatus`). The new `resolve_tc_url(base, reference)` /
  `RtmpClient::resolve_reconnect_url(Option<&str>)` helpers apply the
  spec's target-resolution rule — "if not specified, use the tcUrl for
  the current connection. A relative URI reference should be resolved
  relative to the tcUrl for the current connection" — covering all
  four reference shapes the Info Object table gives as examples
  (absolute `rtmp://host:port/app`, network-path `//host/app`,
  absolute-path `/app`, and relative-path `app`); `RtmpClient::tc_url()`
  exposes the stored base. Tests: wire-shape + optional-property
  omission in `src/message.rs`, classification + resolution tables in
  `src/client.rs`, and `tests/reconnect_request.rs` drives both
  loopback flows end-to-end — including the spec's "old server SHOULD
  continue processing messages from the client until the client
  disconnects" behaviour, proven by publishing a post-request keyframe
  that the requesting server still receives.

- **Abort Message (protocol control type 2) builder + reader-side
  partial-message discard** (`src/message.rs`, `src/chunk.rs`). Round 271
  closes the last protocol-control message that had a type-id constant
  (`MSG_ABORT = 2`) but no builder and no consumer effect. New
  `message::build_abort(chunk_stream_id)` emits the exact RTMP 1.0 §5.2
  wire layout — a single 4-byte big-endian chunk stream ID (Figure 3) on
  the control stream (`msg_type_id = MSG_ABORT`, `msg_stream_id = 0`,
  `timestamp = 0`). New `ChunkReader::abort_partial(chunk_stream_id) ->
  bool` gives the message its spec-mandated receiver behaviour: per §5.2
  the Abort Message tells a peer "waiting for chunks to complete a
  message" to "discard the partially received message over a chunk
  stream and abort processing of that message," so `abort_partial`
  clears the half-filled reassembly buffer for the named csid and
  returns `true` when a non-empty partial was actually discarded.
  Only the in-flight payload bytes are cleared; the csid's header state
  (last timestamp / type / length / extended-timestamp latch) is left
  intact because a subsequent fmt-1/2/3 chunk still relies on it per
  §5.3.2. An Abort for a csid with no in-flight message — or one the
  reader has never seen — is a no-op, matching the spec's "if it is
  waiting for chunks" precondition. Like `ChunkReader::set_chunk_size`,
  the reader does not auto-apply an inbound Abort; the message-layer
  caller dispatches the decoded 4-byte csid here. Total lib tests:
  226 → 228 (+2 — `message::tests::abort_wire_bytes` asserting the
  byte-exact §5.2 payload, `chunk::tests::abort_partial_discards_in_
  flight_message` driving a two-chunk message truncated after its first
  chunk so the reader holds a partial buffer, then asserting
  `abort_partial` discards it, is idempotent on the now-empty csid, and
  is a no-op for an unseen csid). Sourced entirely from RTMP 1.0 §5.2
  (staged at `docs/streaming/rtmp/rtmp_specification_1.0.pdf` /
  `rtmp.part2.Message-Formats.pdf`).
- **`message::UserControlEvent` — public typed view of a User Control
  Message body per RTMP 1.0 §3.7 / §7.1.7** (`src/message.rs`,
  `src/lib.rs`). Round 264 promotes the previously-private
  classify-and-extract logic in `client.rs` into a reusable building
  block on the `message` module. `UserControlEvent::parse(payload)`
  classifies the 2-byte BE event type + variable event data into one
  of the seven spec-defined variants —
  [`UserControlEvent::StreamBegin { stream_id }`] (UCM 0),
  [`UserControlEvent::StreamEof { stream_id }`] (UCM 1),
  [`UserControlEvent::StreamDry { stream_id }`] (UCM 2),
  [`UserControlEvent::SetBufferLength { stream_id, buffer_ms }`]
  (UCM 3, the only 8-byte event-data variant),
  [`UserControlEvent::StreamIsRecorded { stream_id }`] (UCM 4),
  [`UserControlEvent::PingRequest { timestamp_ms }`] (UCM 6),
  [`UserControlEvent::PingResponse { timestamp_ms }`] (UCM 7) — or
  the catch-all [`UserControlEvent::Unknown { event_type, data }`]
  for the spec-reserved type 5 and any future event type ≥ 8. The
  reverse direction is [`UserControlEvent::to_message`]: each
  spec-defined variant rebuilds byte-for-byte the same protocol
  control [`Message`] (`msg_type_id = MSG_USER_CONTROL`,
  `msg_stream_id = 0`, `timestamp = 0`) the matching
  `build_user_control_*` builder emits, and `Unknown` concatenates
  `event_type:U16BE | data` verbatim so a forwarding ingest preserves
  forward-compatible UCMs without re-encoding. Spec-defined variants
  validate their fixed event-data size on parse (4 bytes for the
  stream-id-carrying variants and ping, 8 bytes for
  `SetBufferLength`) and surface
  [`Error::ProtocolViolation`] on truncation;
  [`UserControlEvent::Unknown`] accepts any tail length (including
  zero) so a forwarding ingest never rejects forward-compatible
  messages. [`UserControlEvent::event_type`] /
  [`UserControlEvent::is_spec_defined`] accessors round out the
  surface, and `UserControlEvent` is re-exported at the crate root.
  Total lib tests: 222 → 226 (+4 —
  `user_control_event_parse_recognises_spec_types`,
  `user_control_event_round_trip_matches_builder_bytes`,
  `user_control_event_unknown_preserves_event_type_and_tail`,
  `user_control_event_parse_rejects_truncated_payload`). Sourced
  entirely from RTMP 1.0 spec §3.7 / §7.1.7 (staged at
  `docs/streaming/rtmp/rtmp-v1-0-spec-veovera.pdf` /
  `rtmp.part3.Commands-Messages.pdf`).
- **RTMP 1.0 §3.7 User Control Message events surfaced through
  `ClientEvent` + matching `RtmpSession` server-side helpers**
  (`src/message.rs`, `src/client.rs`, `src/server.rs`,
  `tests/user_control_events.rs`). Round 248 closes the
  publish-direction User Control Message coverage gap: prior to this
  commit only `StreamBegin` (UCM 0) and `StreamEOF` (UCM 1) reached
  the publisher as typed [`ClientEvent`] variants, and the remaining
  spec-defined events were either swallowed silently into
  [`ClientEvent::Other`] (`StreamDry` UCM 2, `SetBufferLength` UCM 3,
  `StreamIsRecorded` UCM 4, `PingResponse` UCM 7) or had no builder
  available at all (`StreamDry`, `SetBufferLength`, `StreamIsRecorded`,
  `PingRequest` from the server side, `PingRequest` /
  `PingResponse` from the client side). Closed end-to-end: new
  `message::build_user_control_stream_dry` /
  `_set_buffer_length(stream_id, buffer_ms)` /
  `_stream_is_recorded` / `_ping_request(timestamp_ms)` /
  `_ping_response(timestamp_ms)` builders emit the exact §3.7 wire
  layouts (2-byte BE event type + 4-byte BE stream id for the
  stream-id-carrying variants; 4-byte BE timestamp for ping; 4-byte
  stream id + 4-byte buffer length for the only 8-byte UCM event,
  `SetBufferLength`). `RtmpClient::poll_event` decodes these into
  three new typed [`ClientEvent`] variants:
  [`ClientEvent::StreamDry`] (carries `stream_id`, distinct from
  `StreamEof`: per spec §3.7 `StreamDry` is a transient
  "no-data-right-now" signal whereas `StreamEof` is "playback
  finished"), [`ClientEvent::StreamIsRecorded`] (server announces
  the stream is on-demand / archival), and
  [`ClientEvent::PingResponse`] (carries the echoed 4-byte
  `timestamp_ms` for RTT measurement). `SetBufferLength` is
  publisher-direction inbound; the classify path validates the
  8-byte event-data length (returning `Error::ProtocolViolation` on
  truncation per the spec's fixed-size invariant) and otherwise
  reports it as [`ClientEvent::Other`]. Server-originated
  `PingRequest` (UCM 6) is auto-replied internally as before —
  promoting it to a `ClientEvent` would expose a protocol-level
  liveness probe as an application event, which the spec assigns
  to the client. New `RtmpClient::send_ping_request(timestamp_ms)`
  emits a UCM-6 from the publisher direction so an outer event
  loop can measure round-trip time by stamping a monotonic clock
  into the request and subtracting from the matching
  [`ClientEvent::PingResponse`] echoed back; new
  `RtmpSession::send_stream_dry` / `send_stream_is_recorded` /
  `send_ping_request` give an ingest the symmetric server-side
  emitters. The `MessageStreamKind` classifier + protocol-control
  invariant validator from the round-247 commit keeps these on
  `msg_stream_id == 0` per RTMP Message Formats §5 — the new
  builders all stamp `msg_stream_id = 0` directly. Total lib
  tests: 217 → 222 (+5 — `user_control_stream_dry_wire_bytes`,
  `user_control_set_buffer_length_wire_bytes`,
  `user_control_stream_is_recorded_wire_bytes`,
  `user_control_ping_request_wire_bytes`,
  `user_control_ping_response_wire_bytes`). Total integration
  tests: 77 → 82 (+5 — `client_observes_stream_dry_from_server`,
  `client_observes_stream_is_recorded_from_server`,
  `client_auto_replies_to_server_ping_request_without_surfacing`,
  `client_surfaces_server_ping_response_as_typed_event` driving a
  hand-rolled chunk-stream PingResponse injection,
  `client_rejects_truncated_set_buffer_length` confirming the
  8-byte validation refuses a 6-byte payload with a
  `ProtocolViolation`). Resolves the remaining `Other`-swallow gap
  identified by reading RTMP Commands Messages spec §3.7 against
  the round-247 classify-path coverage.

- **Typed `MessageStreamKind` accessor + spec-§5 protocol-control
  invariant validator on `chunk::Message`** (`src/chunk.rs`,
  `src/lib.rs`, `tests/message_stream_kind.rs`). The chunk-reassembled
  `Message`'s raw `msg_stream_id: u32` now lifts into a typed
  three-way classification — `Control` (`msg_stream_id == 0`, the
  "control stream" defined in RTMP Message Formats spec §5 carrying
  every NetConnection command + every protocol-control / user-control
  message), `NetStream(id)` (the canonical 1..=`0x00FF_FFFF` handle a
  server returns from `_result(createStream)` per the RTMP Commands
  Messages spec §4.1.3 and that publishers stamp into every A/V /
  metadata / aggregate message from then on), and `Reserved(raw)` (any
  value with bits set in the top byte — the Message Formats spec §4.1
  message header carries the stream ID as a 3-byte field, so anything
  above `0x00FF_FFFF` is reserved). `Message::stream_kind()` returns
  the typed view; `Message::is_control_stream()` is the convenience
  shorthand; `Message::validate_protocol_control_invariants()` returns
  `Err(ProtocolViolation)` whenever a protocol-control message
  (`msg_type_id` 1..=6) carries a non-zero `msg_stream_id` per the
  Message Formats spec §5 mandate "Protocol control messages MUST
  have message stream ID 0 (called as control stream)" and whenever
  the §4.1 reserved high-byte rule is violated. The new types
  re-export through the crate root (`oxideav_rtmp::Message` /
  `oxideav_rtmp::MessageStreamKind`). Four-test integration suite in
  `tests/message_stream_kind.rs` covers the round-trip case (build a
  SetChunkSize via `build_set_chunk_size`, render through
  `ChunkWriter`, reassemble through `ChunkReader`, confirm the typed
  view + validator both pass), the NetStream classification case
  (`msg_stream_id == 1` → `NetStream(1)`), the forged-msid rejection
  case (SetChunkSize stamped with `msg_stream_id = 1` refused with a
  diagnostic naming "protocol-control" + "msg_stream_id"), and the
  reserved-high-byte rejection case (`msg_stream_id = 0x0100_0001`).
- **Aggregate Message (type 22) dispatch through `RtmpSession::next_packet`
  and `RtmpClient::poll_event`, plus `RtmpClient::send_aggregate`
  outbound** (`src/server.rs`, `src/client.rs`,
  `tests/aggregate_routing.rs`). Round 230 closes the matching consumer
  side of the round-229 `aggregate` parser + builder. Previously a
  publisher that bundled several frames into one Aggregate Message
  (RTMP 1.0 §7.1.6, message type id `22` — fewer chunk headers per A/V
  burst) had its body silently swallowed by the dispatch loop's `_ =>
  swallow` fallback arm: the `parse_aggregate` entry point was reachable
  only via a hand-written caller pulling raw `Message` values out of
  [`chunk::ChunkReader`]. Server-side: `next_packet` now drains a new
  per-session `pending_subs: VecDeque<Message>` queue ahead of every
  wire read, and a fresh `MSG_AGGREGATE` arm decomposes incoming
  aggregates into that queue using the same `aggregate::parse_aggregate`
  the round-229 commit ships — the §7.1.6 timestamp re-normalisation
  (`t_i + (aggregate.timestamp - t_0)`) and the spec's
  "aggregate.msg_stream_id overrides sub.msg_stream_id" rule are both
  applied transparently. Per-message dispatch logic factored out of the
  giant `next_packet` body into a `handle_message(&mut self, Message) ->
  Result<Option<StreamPacket>>` helper so wire-read subs and queued
  subs share one code path. The five real-world sub-message types
  (audio / video / data AMF0 / data AMF3 / command AMF0 / command
  AMF3 — including the existing `closeStream` / `deleteStream` /
  `FCUnpublish` teardown detection) all flow through the same arms as
  individually-sent messages. A sub whose `msg_type_id` is itself `22`
  (a forged or speculative nested aggregate; the spec doesn't model
  this but a defensive parser must survive it) is forwarded back to
  the queue and decomposed on the next dispatch tick, so a bounded
  nesting depth resolves to bounded parser work rather than stack
  growth. Client-side: `RtmpClient` carries the same
  `pending_subs` queue and a refactored `poll_event` loops over both
  the queue and the wire read so a server that ever bundled its
  `onStatus` / `_result` / `UserControl` replies into an aggregate
  surfaces the per-sub `ClientEvent`s in publish order. Queued subs
  classified as `ClientEvent::Other` are dropped from the surface so a
  caller pumping `poll_event` doesn't observe N back-to-back `Other`s.
  New outbound helper `RtmpClient::send_aggregate(&[Message]) ->
  Result<()>` is the symmetric publisher API on top of
  `aggregate::build_aggregate`: every sub's `msg_stream_id` is
  overridden to the active publish stream id per §7.1.6, the
  aggregate is framed on `CSID_DATA` (6), and a zero-length slice is
  a no-op. 4 new integration tests in `tests/aggregate_routing.rs`:
  (1) a video + audio + onMetaData aggregate round-trips through real
  loopback `RtmpClient::send_aggregate` →
  `ChunkReader::read_message` → `parse_aggregate` →
  `RtmpSession::next_packet` and surfaces three discrete
  `StreamPacket`s in publish order; (2) a two-sub aggregate with a
  23-ms gap confirms the §7.1.6 offset reaches the per-sub
  `StreamPacket.timestamp` exactly (1000 → 1000, 1023 → 1023, no
  drift); (3) an aggregate carrying a `closeStream` AMF0 command sub
  drives the same teardown path the standalone command takes — the
  server reports `Ok(None)` after the prior media sub instead of
  spinning on the post-FIN socket; (4) the client-side `poll_event`
  contract is exercised via a smoke check holding the dispatch
  contract live as a tested public-API surface (the full server →
  client aggregate flow is covered by `client_stream_eof.rs`
  unchanged). Total lib tests: 217 (unchanged — the new work is
  end-to-end via the integration harness, where queue draining
  through real `TcpStream` state is the load-bearing surface).
  Total integration tests: 73 → 77 (+4). Resolves the
  `next_packet` / `poll_event` half of the RTMP 1.0 §7.1.6
  "Aggregate Message body is not yet decomposed" gap (round 229 closed
  the parser / builder half).

### Changed

- **`RTMP_TIME_BASE` switched from 1/1000 (ms) to 1/1_000_000_000 (ns)
  to fold `TimestampOffsetNano` onto the `Packet` timeline**
  (`src/adapter.rs`, `tests/packet_source.rs`,
  `tests/enhanced_rtmp_audio.rs`, `tests/enhanced_rtmp_video.rs`,
  README, `src/lib.rs`). Resolves the r0.0.5 README follow-up
  "folding that nanosecond offset into the millisecond `Packet`
  timeline is a follow-up." `enhanced-rtmp-v2.pdf` §"ExVideoTagHeader"
  / §"ExAudioTagHeader" assigns the `TimestampOffsetNano` ModEx
  subtype (the only `ModExType` defined today) the duty of adjusting
  the *presentation* time of the current media message without
  altering the core RTMP timestamp; the spec explicitly carries a
  `TODO: Integrate this nanosecond offset into timestamp management`
  marker, which is what this commit closes on the consumer side.
  `audio_to_packet(timestamp_ms, &AudioTag)` now emits
  `pts == dts == timestamp_ms * RTMP_MS_TO_NS +
  AudioTag::timestamp_offset_nano()` (audio has no separate decode
  time, so both PTS and DTS receive the offset);
  `video_to_packet(timestamp_ms, &VideoTag)` emits
  `dts = timestamp_ms * RTMP_MS_TO_NS` (decode timestamp,
  unmodified per spec) and
  `pts = (timestamp_ms + composition_time) * RTMP_MS_TO_NS +
  VideoTag::timestamp_offset_nano()` so legacy AVC composition-time
  offsets in milliseconds and Enhanced-RTMP nanosecond presentation
  offsets compose without precision loss. New public
  `RTMP_MS_TO_NS = 1_000_000` constant exported alongside
  `RTMP_TIME_BASE` so a consumer can recover the wire ms value
  (`pts / RTMP_MS_TO_NS`) when it needs the legacy unit.
  Multiple `TimestampOffsetNano` ModEx entries are summed via the
  existing `VideoTag::timestamp_offset_nano` /
  `AudioTag::timestamp_offset_nano` accessors (one
  `bytesToUI24` per entry); ModEx entries of other subtypes do not
  feed the sum, matching the typed accessor contract. The
  `StreamInfo::time_base` exposed by `RtmpPacketSource` follows
  `RTMP_TIME_BASE` so a downstream `PacketSource` consumer (e.g.
  `oxideav-cli`'s pipeline executor) reads a single uniform
  nanosecond clock from the registry. 5 new lib unit tests in
  `src/adapter.rs::tests` (`audio_timestamp_offset_nano_folds_into_
  presentation_time`, `video_timestamp_offset_nano_folds_into_
  pts_only`, `video_timestamp_offset_nano_stacks_on_cts_and_
  dts_unchanged`, `video_timestamp_offset_nano_sums_multiple_
  modex_entries`, `time_base_is_nanoseconds`) cover: a single
  750_000-ns audio offset stacking on both PTS and DTS; a
  123_456-ns video offset reaching PTS only; a HEVC × CodedFrames
  CTS (17 ms) + 500_000-ns offset composing on PTS without
  perturbing DTS; a multi-entry ModEx chain with an interleaved
  unknown ModExType correctly summing only the
  `TimestampOffsetNano` contributions (200_000 + 300_000); and the
  `RTMP_TIME_BASE == 1/1_000_000_000` + `RTMP_MS_TO_NS == 1_000_000`
  invariants. Existing tests that previously asserted ms-valued
  PTS/DTS were rewritten to multiply by `RTMP_MS_TO_NS`; the ModEx
  integration test in `tests/enhanced_rtmp_video.rs` now asserts the
  nano fold reaches PTS only on the recovered HEVC CodedFrames.
  Total lib tests: 212 → 217 (+5).

### Added

- **Aggregate Message (type 22) parser + builder** (`src/aggregate.rs`,
  `tests/aggregate_chunk_round_trip.rs`, `tests/injection_robustness.rs`).
  RTMP 1.0 §7.1.6 defines the *Aggregate Message* as a single
  `Message` of type id 22 whose payload carries a sequence of
  FLV-shaped sub-messages so several audio / video / data frames can
  travel through the chunk stream as one message. New `aggregate`
  module exposes `parse_aggregate(&Message) -> Result<Vec<Message>>`
  and `build_aggregate(stream_id, &[Message]) -> Result<Message>`,
  both re-exported at the crate root. The sub-header layout mirrors
  §6.1.1 (1 + 3 + 4 + 3 = 11 bytes) which the spec explicitly says
  "matches the format of FLV file" — the FLV §E.4.1 split-timestamp
  (`UI24 ts_low | UI8 ts_high`) and the §E.3 `PreviousTagSize ==
  11 + DataSize` back-pointer invariant are both honoured. The
  §7.1.6 timestamp re-normalisation rule ("the difference between
  the timestamps of the aggregate message and the first sub-message
  is the offset used to renormalize the timestamps of the
  sub-messages") is applied transparently by the parser, lifting
  each sub's wire timestamp `t_i` onto the stream clock as
  `t_i + (aggregate.timestamp - t_0)`; the builder sets
  `aggregate.timestamp == subs[0].timestamp` so the SHOULD-be-zero
  offset holds. Sub `Stream ID` fields are written as 0 on the wire
  (per §7.1.6 / §E.4.1) and the parser overrides every decoded sub's
  `msg_stream_id` with the aggregate's, matching the spec ("the
  message stream ID of the aggregate message overrides the message
  stream IDs of the sub-messages"). Adversarial inputs all surface
  as typed `Result::Err`: truncated headers / payloads / back
  pointers → `UnexpectedEof`; mismatched back pointer or
  non-type-22 outer message → `InvalidChunk`; UI24-cap overflow on
  the build side → `InvalidChunk`. 14 new lib unit tests in
  `src/aggregate.rs` cover: three-sub round-trip with zero offset;
  the §7.1.6 offset shift applied to two subs with a deliberately
  non-zero outer timestamp; empty aggregate symmetry; wrong outer
  type rejection; truncated sub-header / sub-payload / back-pointer
  fail-fast; mismatched back pointer; sub-header `StreamID = 0`
  invariant on build; outer `timestamp = subs[0].timestamp`
  invariant on build; 100-sub round-trip (proves bookkeeping
  scales); UI24-cap rejection on a 16-MiB+1-byte sub payload; a
  forged UI24-max DataSize → clean `UnexpectedEof`; and a
  1024-iteration deterministic-xorshift fuzz pass guaranteeing
  `parse_aggregate` is panic-free on arbitrary bytes. 2 new
  integration tests in `tests/aggregate_chunk_round_trip.rs` drive
  `build_aggregate → ChunkWriter::write_message →
  ChunkReader::read_message → parse_aggregate` on a realistic
  video+audio+script bundle and assert the byte-exact §6.1.1
  sub-header layout (offsets [1..4] DataSize UI24 BE, [4..7] +
  [7] FLV split-timestamp, [8..11] StreamID UI24 = 0, trailing UI32
  back-pointer = `11 + DataSize`). 2 new entries in
  `tests/injection_robustness.rs` extend the property-test sweep
  with 1024 random-byte aggregate payloads (no panics) plus an
  oversize-DataSize fail-fast assertion. Total lib tests: 198 → 212
  (+14). Total integration tests: 69 → 73 (+4). Resolves the RTMP
  1.0 §7.1.6 portion of the README's "Aggregate Message body is
  not yet decomposed" gap.

- **Enhanced RTMP v1+v2 NetConnection `connect` capability negotiation**
  (`src/caps.rs`, `src/message.rs`, `src/client.rs`, `src/server.rs`,
  `tests/connect_capabilities.rs`). The `fourCcList` /
  `videoFourCcInfoMap` / `audioFourCcInfoMap` / `capsEx` properties
  defined in `enhanced-rtmp-v2.pdf` §"Enhancing NetConnection connect
  Command" are now exchanged end-to-end between
  `RtmpClient::connect_with_capabilities` and `RtmpServer::set_capabilities`.
  New `ConnectCapabilities` struct exposes all four entries plus the
  legacy `objectEncoding` byte; new `FourCcInfoMap` keeps the per-codec
  `(FourCC, mask)` entries in insertion order and implements the spec's
  wildcard-OR rule via `effective_mask`. New constants mirror the spec
  enums verbatim: `FourCcInfoMask` (`FOURCC_INFO_CAN_DECODE = 0x01` /
  `_CAN_ENCODE = 0x02` / `_CAN_FORWARD = 0x04`) and `CapsExMask`
  (`CAPS_EX_RECONNECT = 0x01` / `_MULTITRACK = 0x02` / `_MOD_EX = 0x04`
  / `_TIMESTAMP_NANO_OFFSET = 0x08`); the `"*"` catch-all key is the new
  `FOURCC_WILDCARD` constant. The Command Object properties are appended
  to the historical `app` / `tcUrl` / `flashVer` / `fpad` /
  `capabilities` / `audioCodecs` / `videoCodecs` / `videoFunction`
  block in the documented spec order (`objectEncoding` → `fourCcList` →
  `videoFourCcInfoMap` → `audioFourCcInfoMap` → `capsEx`); empty /
  default fields are skipped so an empty capability block produces
  byte-identical output to the pre-2023 [`build_connect`]. The server's
  `_result(connect)` info object echoes its own capabilities through the
  matching `build_connect_result_with_caps` builder. Surfaced to callers
  as `PublishRequest::capabilities` (client-advertised) and
  `RtmpClient::server_capabilities()` (server-advertised); the
  `ConnectCapabilities::from_amf0` parser silently drops malformed
  values (non-numeric mask bytes, non-finite numbers, negative masks,
  `String` for `capsEx`, etc.) and saturates out-of-u32 numbers to
  `u32::MAX`, matching the spec's "fail gracefully" rule. Resolves the
  r0.0.4 README note "The `connect` command's `fourCcList`
  advertisement (Enhanced RTMP v1 Table 5) is not populated by the
  client yet" and the symmetric r0.0.4 audio-/v2-video notes about
  `audioFourCcInfoMap` / `videoFourCcInfoMap` / `capsEx` not being
  populated by `RtmpClient::connect`. 18 new unit tests in
  `src/caps.rs` cover: `FourCcInfoMask` / `CapsExMask` constants match
  the spec table; FourCC wildcard is the single-byte `"*"`;
  `FourCcInfoMap::insert` preserves insertion order across duplicate
  keys; `effective_mask` ORs in the wildcard entry; AMF0 round-trip;
  malformed mask entries dropped; oversize mask saturates; default
  capabilities emit nothing; documented v1+v2 order; full round-trip
  through encode→AMF0 wire→decode for a fully-populated block;
  `has_fourcc` wildcard + explicit; `supports_caps_ex` bit-test;
  malformed `capsEx` falls back to default; non-object inputs return
  empty; `objectEncoding` round-trips 0 / 3; ECMA-array parses the same
  as Object. Plus 4 unit tests in `src/message.rs`:
  `build_connect_with_caps` with empty caps matches legacy bytes
  exactly; non-empty caps append the properties in documented order
  after `videoFunction`; `build_connect_result_with_caps` echoes the
  block inside the info object alongside
  `NetConnection.Connect.Success`; empty caps match legacy
  `build_connect_result` byte-for-byte. Plus 4 integration tests in
  `tests/connect_capabilities.rs`: full loopback round-trips both
  directions through a real TCP socket; legacy client against a v2
  server still receives the server's advertisement; v2 client against a
  legacy server observes empty server caps; `capsEx` bit-test surfaces
  Reconnect / Multitrack / ModEx / TimestampNanoOffset after a real
  loopback. Total lib tests: 177 → 198 (+21 — 18 caps + 4 message tests
  hosted in the existing `message::tests` module). Total integration
  tests: 65 → 69 (+4).

- **FLV file / byte-stream reader** (`src/flv_file.rs`,
  `tests/flv_file_record.rs`). Inverse of the round-204 `FlvWriter`:
  new `FlvReader<R: Read>` wraps a `Read` source and walks the §E.2
  9-byte file header, the §E.3 alternating `PreviousTagSize` /
  `FLVTAG` body, and each §E.4.1 `FLVTAG` header, surfacing every
  tag as a strongly-typed `FlvTag` enum
  (`Audio { timestamp_ms, tag: AudioTag }` /
  `Video { timestamp_ms, tag: VideoTag }` /
  `Script { timestamp_ms, name, value: Amf0Value }` /
  `Unknown { tag_type, timestamp_ms, body }`). Audio + video bodies
  decode through the existing `flv::parse_audio` / `flv::parse_video`
  paths so every wire shape the writer emits — legacy AVC/AAC,
  Enhanced-RTMP v1 FourCC (`hvc1` / `av01` / `vp09`), Enhanced-RTMP
  v2 FourCC (`vp08` / `avc1` / `vvc1` / Opus / FLAC / AC-3 / E-AC-3 /
  MP3 / FourCC-AAC), `Multitrack`, `MultichannelConfig`, and `ModEx`
  preludes — round-trips byte-for-byte through reader → writer
  without re-implementing the §E.3 walk. Script tags decode as an
  AMF0 `Name + Value` pair per §E.4.4; a script body that fails AMF0
  decode is preserved verbatim as `FlvTag::Unknown` (`tag_type =
  18`) so a forwarding consumer never silently drops bytes.
  `FlvReader::new` consumes the §E.2 header (signature `F` `L` `V` +
  version + `TypeFlagsAudio` / `TypeFlagsVideo` + UI32 `DataOffset`)
  and the mandatory `PreviousTagSize0 == 0` back-pointer eagerly,
  refusing wrong-signature / wrong-version / nonzero-`PreviousTagSize0`
  inputs up front. A larger `DataOffset` (forward-compatible header
  extension) is skipped transparently. Verifies the §E.3
  `PreviousTagSize == 11 + DataSize` invariant on every tag and
  refuses to advance past a mismatch (forged producer / transport
  corruption). UI24 `DataSize` is bounded by a configurable
  `max_tag_size` (default = UI24 ceiling, `DEFAULT_MAX_TAG_SIZE`)
  via the new `FlvReader::with_max_tag_size` constructor — HTTP-FLV
  proxies generally want a tighter cap, trusted local files can
  raise it back to the wire ceiling. The §E.4.1 `StreamID = 0`
  invariant is enforced, the §E.4.1 `Filter = 1` (Annex F encrypted
  body) surfaces as a clean `Error::Other` rather than silently
  passing through, and truncated header / payload / back-pointer
  all surface as `Error::UnexpectedEof`. `FlvReader::read_tag`
  returns `Ok(None)` on a clean end-of-stream at a tag boundary and
  latches so subsequent calls don't re-enter the reader on an
  exhausted source. `FlvReader::read_all` consumes the rest of the
  stream into a `Vec<FlvTag>` for one-shot use. 17 new tests (15
  unit in `src/flv_file.rs`, 1 integration in
  `tests/flv_file_record.rs`, plus 1 new doc-test added via the
  module-level rewording): writer-then-reader round-trips for empty
  stream / AVC sequence header / AAC sequence header / interleaved
  video+audio+video triple / Enhanced-RTMP v2 HEVC `CodedFrames` /
  full 4-tag stream (script + video SH + audio SH + video inter)
  byte-for-byte through reader → writer; AMF0 `onMetaData`
  name+value pair decoded into typed `EcmaArray`; `TimestampExtended`
  high byte re-joined into a 32-bit value; bad signature / wrong
  version / nonzero `PreviousTagSize0` / `DataOffset < 9` rejected
  cleanly; forward-compatible header padding (`DataOffset = 11`)
  skipped transparently; corrupt `PreviousTagSize` / oversize
  `DataSize` / nonzero `StreamID` / `Filter = 1` rejected with
  matching error variants; truncated FLVTAG header and payload
  surface `Error::UnexpectedEof`; unknown `TagType` value (5) lifted
  as `FlvTag::Unknown` with the verbatim body. End-to-end
  integration test drives an RTMP loopback, records every received
  `StreamPacket` to an in-memory FLV byte stream via `FlvWriter`,
  then walks the resulting buffer back through `FlvReader` and
  asserts every tag's body matches the publisher's input. Total lib
  tests: 162 → 177 (+15); total integration tests: 64 → 65 (+1).
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
