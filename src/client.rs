//! RTMP client: push a live stream to a remote RTMP server.
//!
//! ```text
//!   let mut client = RtmpClient::connect("rtmp://remote/live/key")?;
//!   client.send_video_sequence_header(&avcc_bytes)?;
//!   client.send_audio_sequence_header(&aac_config)?;
//!   loop {
//!       client.send_video(ts_ms, keyframe, &nalu_bytes)?;
//!       client.send_audio(ts_ms, &aac_frame)?;
//!   }
//! ```
//!
//! This crate emits one H.264 NAL per video call (no re-fragmentation
//! into AVCC length-prefixed packets beyond the single-NAL case).
//! Callers with multiple NALUs per sample can concatenate them into
//! one body — RTMP just forwards bytes on the video channel.

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::{Shutdown, TcpStream, ToSocketAddrs};
use std::time::Duration;

use crate::aggregate::parse_aggregate;
use crate::amf::{self, Amf0Value};
use crate::amf3;
use crate::caps::ConnectCapabilities;
use crate::chunk::{ChunkReader, ChunkWriter, Message};
use crate::error::{Error, Result};
use crate::flv::{self, AudioTag, VideoTag};
use crate::handshake::HandshakeKind;
use crate::message::*;

/// Server-originated event observed by an [`RtmpClient`] in publish
/// mode.
///
/// During an active publish the client mostly writes audio / video /
/// data and the server stays mostly silent — but a few server→client
/// notifications matter end-to-end. The most important is
/// [`StreamEof`](Self::StreamEof): the server signalling, per RTMP 1.0
/// §7.1.7, that "the stream is dry, no more data will be sent without
/// additional commands." A symmetric publish-side server uses the same
/// `UserControl StreamEOF` event to mark end-of-publish before closing
/// the TCP write half — and the client should treat that as a clean
/// stream end rather than as an unexpected FIN.
#[derive(Debug, Clone, PartialEq)]
pub enum ClientEvent {
    /// The server emitted `UserControl StreamBegin(stream_id)`
    /// (UCM event type 0). Informational — most servers send this once
    /// right after `createStream` succeeds.
    StreamBegin { stream_id: u32 },
    /// The server emitted `UserControl StreamEOF(stream_id)`
    /// (UCM event type 1). End-of-stream from the server side. After
    /// observing this, the caller should stop writing and shut the
    /// client down via [`RtmpClient::close`].
    StreamEof { stream_id: u32 },
    /// The server emitted `UserControl StreamDry(stream_id)`
    /// (UCM event type 2). Per RTMP 1.0 §3.7, the server uses this to
    /// notify the client "that there is no more data on the stream. If
    /// the server does not detect any message for a time period, it
    /// can notify the subscribed clients that the stream is dry."
    /// Distinct from [`StreamEof`](Self::StreamEof): `StreamDry` is a
    /// "no data right now" signal that may resolve once more data
    /// arrives; `StreamEof` is "playback finished, no more without
    /// further commands."
    StreamDry { stream_id: u32 },
    /// The server emitted `UserControl StreamIsRecorded(stream_id)`
    /// (UCM event type 4). Per RTMP 1.0 §3.7, "the server sends this
    /// event to notify the client that the stream is a recorded
    /// stream." Servers typically emit this right after `StreamBegin`
    /// for an on-demand stream; for a publish-only client the event is
    /// informational and usually ignored.
    StreamIsRecorded { stream_id: u32 },
    /// The server emitted `UserControl PingResponse(timestamp_ms)`
    /// (UCM event type 7). Per RTMP 1.0 §3.7, the client sends a
    /// `PingResponse` "in response to the ping request. The event
    /// data is a 4-byte timestamp, which was received with the
    /// kMsgPingRequest request." A server that emits `PingResponse`
    /// is typically echoing back our own (publisher-side) `PingRequest`
    /// — useful for measuring round-trip latency. The variant carries
    /// the echoed timestamp verbatim.
    PingResponse { timestamp_ms: u32 },
    /// The server emitted `onStatus(...)` carrying NetStream state.
    /// `level` is typically `"status"` / `"warning"` / `"error"`;
    /// `code` is e.g. `"NetStream.Publish.Start"` /
    /// `"NetStream.Unpublish.Success"` / `"NetStream.Publish.BadName"`.
    OnStatus {
        level: String,
        code: String,
        description: String,
    },
    /// The server emitted
    /// `onStatus(NetConnection.Connect.ReconnectRequest)` — Enhanced
    /// RTMP v2 §"Reconnect Request". The server is asking us to
    /// reconnect, e.g. ahead of a server update or to remap us to a
    /// different server instance.
    ///
    /// Per the spec's message flow, on receipt the client "persists
    /// in streaming to/from the current server up to the next
    /// appropriate media boundary, such as a keyframe. Subsequently,
    /// it establishes a connection with a new server and disconnects
    /// from the old server." So: finish the current GOP, then dial
    /// [`RtmpClient::resolve_reconnect_url`]`(tc_url.as_deref())` with
    /// a fresh [`RtmpClient::connect`] and drop this client.
    ///
    /// `tc_url` is the optional Info-Object property naming where to
    /// reconnect — an absolute (`rtmp://host/app`) or relative
    /// (`//host/app`, `/app`) URI reference. `None` means "use the
    /// tcUrl for the current connection" per spec.
    ReconnectRequest {
        tc_url: Option<String>,
        description: String,
    },
    /// The server emitted `_result(transaction_id, ...)` for a command
    /// the client issued. The publish-time `connect` / `createStream`
    /// transactions are consumed internally by [`RtmpClient::connect`];
    /// any subsequent `_result` (e.g. a custom RPC sent after publish
    /// started) surfaces here so the caller can match it against its
    /// own transaction id.
    Result {
        transaction_id: f64,
        values: Vec<Amf0Value>,
    },
    /// The server emitted `_error(transaction_id, ...)`. Symmetric to
    /// [`Result`](Self::Result) but for the failure path.
    ErrorReply {
        transaction_id: f64,
        values: Vec<Amf0Value>,
    },
    /// A §7.2.1.2 NetConnection `call` RPC issued *by the server* —
    /// any command whose name is not a spec-defined built-in, since an
    /// RPC's procedure name rides the command-name field. Answer with
    /// [`RtmpClient::reply_call_result`] /
    /// [`RtmpClient::reply_call_error`] when
    /// [`CallCommand::expects_response`].
    Call(CallCommand),
    /// A Shared Object message (type 19 AMF0 / 16 AMF3, bridged onto
    /// AMF0) from the server — e.g. a `Change` broadcast or the
    /// `UseSuccess` + `Clear` acceptance sequence for an SO this
    /// client `Use`d via [`RtmpClient::send_shared_object`].
    SharedObject(crate::shared_object::SharedObjectMessage<Amf0Value>),
    /// Any other server-originated message (ping, ack, set-chunk-size,
    /// bandwidth — most of which the client handles transparently
    /// inside [`RtmpClient::poll_event`] before this variant ever fires).
    /// The variant exists so the caller's `match` arm can keep going.
    Other,
}

pub(crate) const CLIENT_CHUNK_SIZE: u32 = 4096;
pub(crate) const FLASH_VER: &str = "FMLE/3.0 (compatible; oxideav-rtmp)";

pub struct RtmpClient {
    stream: TcpStream,
    /// Kept around so `recv` helpers (ack, onStatus replies, the
    /// server-side `UserControl StreamEOF` mirror of our own
    /// publish-side teardown) have somewhere to drain the server's
    /// side. Surfaced through [`poll_event`](Self::poll_event).
    reader: ChunkReader<TcpStream>,
    writer: ChunkWriter<TcpStream>,
    stream_id: u32,
    /// Monotonic counter used for AMF command transaction ids.
    next_tx: f64,
    /// Set once we've observed the read half drain (EOF / connection
    /// reset) so subsequent `poll_event` calls return `Ok(None)` rather
    /// than re-entering [`ChunkReader::read_message`] on a dead socket.
    /// Distinct from a `StreamEOF` user-control event — the server
    /// normally sends `StreamEOF` *first* then a trailing onStatus, so
    /// `poll_event` keeps reading until the kernel reports EOF.
    read_eof: bool,
    /// Enhanced RTMP capability block lifted from the server's
    /// `_result(connect)` info object. Empty when the server didn't
    /// advertise any v1+v2 capabilities (the historical pre-2023
    /// shape). Inspect via [`server_capabilities`](Self::server_capabilities).
    server_caps: ConnectCapabilities,
    /// The `tcUrl` this client dialled, kept so an Enhanced RTMP v2
    /// `NetConnection.Connect.ReconnectRequest` whose Info Object
    /// omits `tcUrl` — or names a *relative* URI reference — can be
    /// resolved per spec ("if not specified, use the tcUrl for the
    /// current connection. A relative URI reference should be
    /// resolved relative to the tcUrl for the current connection").
    tc_url: String,
    /// Sub-messages decomposed out of a server-originated Aggregate
    /// Message (type 22) per RTMP 1.0 §7.1.6 but not yet routed
    /// through the [`poll_event`](Self::poll_event) classify path.
    /// In publish mode a remote server rarely batches its replies
    /// this way, but `poll_event` decomposes the aggregate
    /// transparently so a publisher that opted into a server-side
    /// aggregate digest (`@enableEnhancedRTMP`-style negotiation, or
    /// a peer reflecting its own ack stream as an aggregate) still
    /// sees the per-event classification.
    pending_subs: VecDeque<Message>,
    /// §5.4.5 Set Peer Bandwidth limit-type state (Hard / Soft /
    /// Dynamic) for the peer's output-bandwidth requests.
    peer_bw: PeerBandwidthLimiter,
    /// Which handshake flavour the connection settled on — always
    /// [`HandshakeKind::Simple`] for the plain constructors,
    /// negotiated for
    /// [`connect_with_digest_handshake`](Self::connect_with_digest_handshake).
    handshake_kind: HandshakeKind,
}

/// Parsed RTMP URL: `rtmp://host[:port]/app/stream_name`.
#[derive(Debug, Clone)]
pub struct RtmpUrl {
    pub host: String,
    pub port: u16,
    pub app: String,
    pub stream_name: String,
    pub tc_url: String,
}

impl RtmpUrl {
    pub fn parse(url: &str) -> Result<Self> {
        let s = url
            .strip_prefix("rtmp://")
            .ok_or_else(|| Error::Other(format!("not an rtmp:// URL: {url}")))?;
        // authority/path
        let slash = s
            .find('/')
            .ok_or_else(|| Error::Other("missing /app in rtmp URL".into()))?;
        let authority = &s[..slash];
        let path = &s[slash + 1..];
        let (host, port) = match authority.rsplit_once(':') {
            Some((h, p)) => (
                h.to_owned(),
                p.parse::<u16>()
                    .map_err(|e| Error::Other(format!("rtmp URL bad port: {e}")))?,
            ),
            None => (authority.to_owned(), 1935),
        };
        let (app, stream_name) = match path.find('/') {
            Some(i) => (path[..i].to_owned(), path[i + 1..].to_owned()),
            None => (path.to_owned(), String::new()),
        };
        let tc_url = format!("rtmp://{authority}/{app}");
        Ok(Self {
            host,
            port,
            app,
            stream_name,
            tc_url,
        })
    }
}

impl RtmpClient {
    /// Dial the given `rtmp://host[:port]/app/stream_name` URL,
    /// perform the full handshake + connect + createStream + publish
    /// sequence, and return a ready-to-send client.
    pub fn connect(url: &str) -> Result<Self> {
        let parsed = RtmpUrl::parse(url)?;
        Self::connect_parsed(&parsed, "live", &ConnectCapabilities::default(), false)
    }

    /// Same as [`connect`](Self::connect) but lets the caller pick the
    /// RTMP `publish` type (typically `"live"`, `"record"`, or
    /// `"append"`).
    pub fn connect_with_type(url: &str, publish_type: &str) -> Result<Self> {
        let parsed = RtmpUrl::parse(url)?;
        Self::connect_parsed(
            &parsed,
            publish_type,
            &ConnectCapabilities::default(),
            false,
        )
    }

    /// Connect and advertise the supplied Enhanced RTMP v1+v2
    /// capability block in the NetConnection `connect` command
    /// (`enhanced-rtmp-v2.pdf` §"Enhancing NetConnection connect
    /// Command"). The block is appended to the legacy Command Object
    /// in the documented order — peers that don't speak E-RTMP keep
    /// parsing the message correctly because the extras are tacked on
    /// after the historical `videoFunction` field.
    ///
    /// The server's `_result` properties object is parsed for the same
    /// set of keys and stashed on the client; retrieve it via
    /// [`server_capabilities`](Self::server_capabilities) to learn
    /// which v1+v2 features the peer agreed to support.
    pub fn connect_with_capabilities(
        url: &str,
        publish_type: &str,
        caps: &ConnectCapabilities,
    ) -> Result<Self> {
        let parsed = RtmpUrl::parse(url)?;
        Self::connect_parsed(&parsed, publish_type, caps, false)
    }

    /// Same as [`connect_with_capabilities`](Self::connect_with_capabilities)
    /// but running the digest (HMAC-SHA256) handshake instead of the
    /// plain echo exchange: C1 goes out with an embedded digest and a
    /// non-zero version field, and the server's digested S1/S2 are
    /// validated. Falls back to the simple exchange transparently when
    /// the server doesn't answer with a digested S1 — inspect
    /// [`handshake_kind`](Self::handshake_kind) to learn what was
    /// negotiated and whether the server's response digest verified.
    pub fn connect_with_digest_handshake(
        url: &str,
        publish_type: &str,
        caps: &ConnectCapabilities,
    ) -> Result<Self> {
        let parsed = RtmpUrl::parse(url)?;
        Self::connect_parsed(&parsed, publish_type, caps, true)
    }

    /// Which handshake flavour the connection settled on
    /// ([`HandshakeKind::Simple`] unless the client was built via
    /// [`connect_with_digest_handshake`](Self::connect_with_digest_handshake)
    /// and the server proved digest support).
    pub fn handshake_kind(&self) -> HandshakeKind {
        self.handshake_kind
    }

    /// Capability block advertised by the server in `_result(connect)`.
    ///
    /// Empty when the peer is pre-2023 / unaware of E-RTMP — callers can
    /// detect that with [`ConnectCapabilities::is_empty`]. Otherwise
    /// describes which FourCC codecs the server reports it can decode /
    /// encode / forward, plus the v2 `capsEx` bitfield (Reconnect,
    /// Multitrack, ModEx, TimestampNanoOffset).
    pub fn server_capabilities(&self) -> &ConnectCapabilities {
        &self.server_caps
    }

    /// The `tcUrl` this client dialled (e.g.
    /// `rtmp://host:1935/app`) — the base every Enhanced RTMP v2
    /// reconnect target resolves against.
    pub fn tc_url(&self) -> &str {
        &self.tc_url
    }

    /// Resolve the `tcUrl` carried by an Enhanced RTMP v2
    /// [`ClientEvent::ReconnectRequest`] into the absolute URL to
    /// re-dial, per `enhanced-rtmp-v2.pdf` §"Reconnect Request":
    ///
    /// * `None` → "use the tcUrl for the current connection".
    /// * `Some(reference)` → "absolute or relative URI reference of
    ///   the server to which to reconnect. A relative URI reference
    ///   should be resolved relative to the tcUrl for the current
    ///   connection." All four spec example shapes are honoured:
    ///   `rtmp://foo.mydomain.com:1935/realtimeapp` (absolute),
    ///   `//192.0.2.0/realtimeapp` (network-path: keep our scheme),
    ///   `/realtimeapp` (absolute-path: keep scheme + authority), and
    ///   `realtimeapp` (relative-path: merge onto our tcUrl's path).
    ///
    /// Append the stream key (`/{stream_name}`) and feed the result to
    /// [`RtmpClient::connect`] to complete the spec's reconnect flow.
    pub fn resolve_reconnect_url(&self, tc_url: Option<&str>) -> String {
        match tc_url {
            Some(reference) => resolve_tc_url(&self.tc_url, reference),
            None => self.tc_url.clone(),
        }
    }

    fn connect_parsed(
        u: &RtmpUrl,
        publish_type: &str,
        caps: &ConnectCapabilities,
        digest_handshake: bool,
    ) -> Result<Self> {
        let sock_addr = (u.host.as_str(), u.port)
            .to_socket_addrs()
            .map_err(Error::from)?
            .next()
            .ok_or_else(|| Error::Other(format!("resolved no addresses for {}", u.host)))?;
        let stream = TcpStream::connect_timeout(&sock_addr, Duration::from_secs(15))?;
        let _ = stream.set_nodelay(true);

        // Handshake on a fresh clone — no chunk state is shared with
        // it.
        let mut hs = stream.try_clone()?;
        let handshake_kind = if digest_handshake {
            crate::handshake::client_handshake_digest(
                &mut hs,
                crate::handshake::DigestScheme::Schema1,
            )?
        } else {
            crate::handshake::client_handshake(&mut hs)?;
            HandshakeKind::Simple
        };

        let mut reader = ChunkReader::new(stream.try_clone()?);
        let mut writer = ChunkWriter::new(stream.try_clone()?);

        // We bump chunk size immediately — most commodity publishers
        // do this too. Saves a bunch of chunk headers over the A/V path.
        writer.write_message(
            CSID_PROTOCOL_CONTROL,
            &build_set_chunk_size(CLIENT_CHUNK_SIZE),
        )?;
        writer.set_chunk_size(CLIENT_CHUNK_SIZE as usize);

        // Send connect.
        let tx = 1.0;
        writer.write_message(
            CSID_COMMAND,
            &build_connect_with_caps(tx, &u.app, &u.tc_url, FLASH_VER, caps),
        )?;
        writer.flush()?;

        // Drain until we see the _result for connect; lift the server's
        // capability advertisement out of the info object.
        let connect_result = wait_for_result(&mut reader, &mut writer, tx)?;
        // Info object is the last Object/EcmaArray AMF0 value after the
        // properties slot; per §"Enhancing NetConnection connect Command"
        // the server stamps its capabilities into one of the `_result`
        // parameters. Walk back to the first Object/ECMA-array carrying
        // any of the documented capability keys.
        let server_caps = extract_server_caps(&connect_result);

        // releaseStream + FCPublish — optional but standard.
        let tx_release = 2.0;
        writer.write_message(
            CSID_COMMAND,
            &build_release_stream(tx_release, &u.stream_name),
        )?;
        let tx_fc = 3.0;
        writer.write_message(CSID_COMMAND, &build_fc_publish(tx_fc, &u.stream_name))?;

        // createStream.
        let tx_cs = 4.0;
        writer.write_message(CSID_COMMAND, &build_create_stream(tx_cs))?;
        writer.flush()?;

        let stream_id = wait_for_create_stream_result(&mut reader, &mut writer, tx_cs)?;

        // publish.
        let tx_pub = 5.0;
        writer.write_message(
            CSID_COMMAND,
            &build_publish(tx_pub, stream_id, &u.stream_name, publish_type),
        )?;
        writer.flush()?;

        // Wait for Publish.Start. Ignore any interleaved control
        // messages. Some servers don't bother sending onStatus —
        // don't block forever, just wait briefly.
        wait_for_publish_start(&mut reader, &mut writer)?;

        Ok(Self {
            stream,
            reader,
            writer,
            stream_id,
            next_tx: 10.0,
            read_eof: false,
            server_caps,
            tc_url: u.tc_url.clone(),
            pending_subs: VecDeque::new(),
            peer_bw: PeerBandwidthLimiter::new(),
            handshake_kind,
        })
    }

    /// Send the AVC sequence header (`AVCDecoderConfigurationRecord`
    /// aka avcC). Must be called once before any NALU-carrying
    /// [`send_video`](Self::send_video).
    pub fn send_video_sequence_header(&mut self, avc_c: &[u8]) -> Result<()> {
        let tag = VideoTag {
            mod_ex: Vec::new(),
            frame_type: flv::VIDEO_FRAME_KEYFRAME,
            codec_id: flv::VIDEO_CODEC_AVC,
            avc_packet_type: Some(flv::AVC_PACKET_TYPE_SEQUENCE_HEADER),
            composition_time: 0,
            body: avc_c.to_vec(),
            ex_packet_type: None,
            fourcc: None,

            multitrack: None,
        };
        self.send_video_tag(0, &tag)
    }

    /// Send one video access unit. `body` is the AVCC-formatted
    /// content (one or more `[u32 length BE][NALU bytes]` pairs).
    /// `is_keyframe` drives the FLV frame_type bits.
    pub fn send_video(&mut self, timestamp_ms: u32, is_keyframe: bool, body: &[u8]) -> Result<()> {
        let tag = VideoTag {
            mod_ex: Vec::new(),
            frame_type: if is_keyframe {
                flv::VIDEO_FRAME_KEYFRAME
            } else {
                flv::VIDEO_FRAME_INTER
            },
            codec_id: flv::VIDEO_CODEC_AVC,
            avc_packet_type: Some(flv::AVC_PACKET_TYPE_NALU),
            composition_time: 0,
            body: body.to_vec(),
            ex_packet_type: None,
            fourcc: None,

            multitrack: None,
        };
        self.send_video_tag(timestamp_ms, &tag)
    }

    fn send_video_tag(&mut self, ts: u32, tag: &VideoTag) -> Result<()> {
        let payload = flv::build_video(tag);
        self.writer.write_message(
            CSID_VIDEO,
            &Message {
                msg_type_id: MSG_VIDEO,
                msg_stream_id: self.stream_id,
                timestamp: ts,
                payload,
            },
        )?;
        self.writer.flush()?;
        Ok(())
    }

    /// Send the AAC `AudioSpecificConfig` (2 bytes for LC-AAC 44.1k
    /// stereo: `0x12 0x10`). Must be called once before any
    /// raw-frame [`send_audio`](Self::send_audio).
    pub fn send_audio_sequence_header(&mut self, asc: &[u8]) -> Result<()> {
        let tag = AudioTag {
            mod_ex: Vec::new(),
            sound_format: flv::AUDIO_FORMAT_AAC,
            sound_rate: 3,
            sound_size_16bit: true,
            stereo: true,
            aac_packet_type: Some(flv::AAC_PACKET_TYPE_SEQUENCE_HEADER),
            body: asc.to_vec(),
            ex_packet_type: None,
            audio_fourcc: None,

            multitrack: None,
        };
        self.send_audio_tag(0, &tag)
    }

    /// Send one raw AAC frame.
    pub fn send_audio(&mut self, timestamp_ms: u32, aac_frame: &[u8]) -> Result<()> {
        let tag = AudioTag {
            mod_ex: Vec::new(),
            sound_format: flv::AUDIO_FORMAT_AAC,
            sound_rate: 3,
            sound_size_16bit: true,
            stereo: true,
            aac_packet_type: Some(flv::AAC_PACKET_TYPE_RAW),
            body: aac_frame.to_vec(),
            ex_packet_type: None,
            audio_fourcc: None,

            multitrack: None,
        };
        self.send_audio_tag(timestamp_ms, &tag)
    }

    fn send_audio_tag(&mut self, ts: u32, tag: &AudioTag) -> Result<()> {
        let payload = flv::build_audio(tag);
        self.writer.write_message(
            CSID_AUDIO,
            &Message {
                msg_type_id: MSG_AUDIO,
                msg_stream_id: self.stream_id,
                timestamp: ts,
                payload,
            },
        )?;
        self.writer.flush()?;
        Ok(())
    }

    /// Send `@setDataFrame("onMetaData", metadata)`. Metadata is an
    /// AMF0 value, typically an ECMA array or object populated with
    /// `width`, `height`, `duration`, `videodatarate`, `framerate`,
    /// `videocodecid`, `audiodatarate`, `audiocodecid`, etc.
    pub fn send_metadata(&mut self, metadata: Amf0Value) -> Result<()> {
        let msg = build_set_data_frame(self.stream_id, metadata);
        self.writer.write_message(CSID_DATA, &msg)?;
        self.writer.flush()?;
        Ok(())
    }

    /// Send `@setDataFrame(handler, value)` for an arbitrary handler
    /// name (`"onCuePoint"`, `"onFI"`, …): ask the server to store the
    /// frame and replay it under `handler` to every future subscriber.
    /// [`send_metadata`](Self::send_metadata) is the
    /// `handler == "onMetaData"` special case.
    pub fn send_data_frame(&mut self, handler: &str, value: Amf0Value) -> Result<()> {
        let msg = build_set_data_frame_named(self.stream_id, handler, value);
        self.writer.write_message(CSID_DATA, &msg)?;
        self.writer.flush()?;
        Ok(())
    }

    /// Send `@clearDataFrame(handler)` — revoke a data frame
    /// previously stored via
    /// [`send_data_frame`](Self::send_data_frame) /
    /// [`send_metadata`](Self::send_metadata) so the server stops
    /// replaying it to new subscribers.
    pub fn clear_data_frame(&mut self, handler: &str) -> Result<()> {
        let msg = build_clear_data_frame(self.stream_id, handler);
        self.writer.write_message(CSID_DATA, &msg)?;
        self.writer.flush()?;
        Ok(())
    }

    /// `@clearDataFrame("onMetaData")` — the inverse of
    /// [`send_metadata`](Self::send_metadata).
    pub fn clear_metadata(&mut self) -> Result<()> {
        self.clear_data_frame("onMetaData")
    }

    /// Send a Shared Object message (AMF0, type 19) to the server —
    /// e.g. a `Use` subscribing to a named SO or a `RequestChange`
    /// proposing a property value. Server-side SO traffic surfaces as
    /// [`ClientEvent::SharedObject`] through
    /// [`poll_event`](Self::poll_event). SO traffic rides the command
    /// chunk stream on message stream 0.
    pub fn send_shared_object(
        &mut self,
        so: &crate::shared_object::SharedObjectMessage<Amf0Value>,
    ) -> Result<()> {
        self.writer
            .write_message(CSID_COMMAND, &so.to_message_amf0(0)?)?;
        self.writer.flush()?;
        Ok(())
    }

    /// Send `onMetaData` as an AMF3-encoded data message (RTMP message
    /// type 15) instead of the AMF0 default.
    ///
    /// The body is framed per the Enhanced RTMP v2 clarification of the
    /// historical AMF 3 §4.1 / AMF 0 §3.1 rules: the payload leads with
    /// a *format selector* byte ([`amf3::FORMAT_SELECTOR_AMF0`], the
    /// only defined value), after which the message structure is AMF0
    /// and each value switches to AMF3 by prefixing it with the
    /// `avmplus-object-marker` (`0x11`) — the switch is not sticky.
    /// Most ingest endpoints stay on AMF0, so prefer
    /// [`send_metadata`](Self::send_metadata); this exists for peers that
    /// negotiated an AMF3 channel (`objectEncoding` 3).
    pub fn send_metadata_amf3(&mut self, metadata: amf3::Amf3Value) -> Result<()> {
        let mut payload = vec![amf3::FORMAT_SELECTOR_AMF0];
        payload.push(amf3::AVMPLUS_OBJECT_MARKER);
        amf3::encode(&mut payload, &amf3::Amf3Value::String("onMetaData".into()));
        payload.push(amf3::AVMPLUS_OBJECT_MARKER);
        amf3::encode(&mut payload, &metadata);
        let msg = Message {
            msg_type_id: MSG_DATA_AMF3,
            msg_stream_id: self.stream_id,
            timestamp: 0,
            payload,
        };
        self.writer.write_message(CSID_DATA, &msg)?;
        self.writer.flush()?;
        Ok(())
    }

    /// Send a batch of audio / video / data sub-messages as one
    /// Aggregate Message (RTMP 1.0 §7.1.6, message type id 22).
    ///
    /// An aggregate trades one extra 11-byte sub-header per sub-message
    /// (plus a 4-byte back-pointer) for the chunk-header overhead the
    /// chunk writer would emit on each sub if it sent them
    /// individually. For an active publish with several A/V messages
    /// queued at the same timestamp this can cut the chunk-header
    /// surface in half.
    ///
    /// `subs` carries pre-built FLV-shaped messages — typically AVC
    /// video (type 9) and AAC audio (type 8) bodies the caller has
    /// already framed via [`flv::build_video`] / [`flv::build_audio`].
    /// Caller-supplied `msg_stream_id` fields are overridden to this
    /// client's publish stream id per §7.1.6 ("the message stream ID
    /// of the aggregate message overrides the message stream IDs of
    /// the sub-messages"). The aggregate's own wire timestamp is set
    /// to the first sub's timestamp so the §7.1.6 re-normalisation
    /// offset is zero on the wire.
    ///
    /// Returns the same errors as
    /// [`build_aggregate`](crate::aggregate::build_aggregate) plus any
    /// I/O error from the underlying chunk writer. An empty `subs`
    /// slice is a no-op.
    pub fn send_aggregate(&mut self, subs: &[Message]) -> Result<()> {
        if subs.is_empty() {
            return Ok(());
        }
        // Override every sub's msg_stream_id to ours so the §7.1.6
        // override invariant holds without surprising the caller.
        let normalized: Vec<Message> = subs
            .iter()
            .map(|s| Message {
                msg_type_id: s.msg_type_id,
                msg_stream_id: self.stream_id,
                timestamp: s.timestamp,
                payload: s.payload.clone(),
            })
            .collect();
        let agg = crate::aggregate::build_aggregate(self.stream_id, &normalized)?;
        // CSID_DATA (6) is the natural data-channel id — aggregates
        // aren't a protocol-control event.
        self.writer.write_message(CSID_DATA, &agg)?;
        self.writer.flush()?;
        Ok(())
    }

    /// Send a `UserControl PingRequest` (RTMP 1.0 §3.7, UCM type 6)
    /// carrying the supplied 4-byte timestamp.
    ///
    /// The peer is expected to echo the value back as a `PingResponse`
    /// (UCM type 7), which surfaces from
    /// [`poll_event`](Self::poll_event) as
    /// [`ClientEvent::PingResponse`]. Typical use is round-trip-time
    /// measurement: stamp the local monotonic clock into the request,
    /// then subtract from the response timestamp once the matching
    /// `PingResponse` arrives. The publish direction normally never
    /// needs this — but a publisher pumping a low-bandwidth feed over
    /// a flaky link may want to probe liveness explicitly rather than
    /// wait for TCP keepalive.
    pub fn send_ping_request(&mut self, timestamp_ms: u32) -> Result<()> {
        self.writer.write_message(
            CSID_PROTOCOL_CONTROL,
            &build_user_control_ping_request(timestamp_ms),
        )?;
        self.writer.flush()?;
        Ok(())
    }

    /// Emit a §5.3 Acknowledgement if the reader's received-byte count
    /// has crossed the server-negotiated §5.5 window since the last
    /// one. No-op until the server advertises a Window Acknowledgement
    /// Size / Set Peer Bandwidth (every commodity ingest does so right
    /// after `connect`).
    fn maybe_send_ack(&mut self) -> Result<()> {
        if let Some(seq) = self.reader.ack_due() {
            self.writer
                .write_message(CSID_PROTOCOL_CONTROL, &build_ack(seq))?;
            self.writer.flush()?;
        }
        Ok(())
    }

    /// Poll for one server-originated event.
    ///
    /// Reads up to one inbound RTMP message from the server, applies
    /// protocol-level housekeeping internally (set-chunk-size,
    /// window-ack-size, set-peer-bandwidth, ping-request/response,
    /// acks), and surfaces externally-visible notifications as a
    /// [`ClientEvent`]:
    ///
    /// * `UserControl StreamBegin(sid)` → [`ClientEvent::StreamBegin`]
    /// * `UserControl StreamEOF(sid)`   → [`ClientEvent::StreamEof`]
    ///   (mirror of [`RtmpSession::close`](crate::RtmpSession::close)'s
    ///   server-side teardown; RTMP 1.0 §7.1.7)
    /// * `onStatus(...)`                → [`ClientEvent::OnStatus`]
    /// * `_result(tx_id, ...)`          → [`ClientEvent::Result`]
    /// * `_error(tx_id, ...)`           → [`ClientEvent::ErrorReply`]
    /// * everything else                → [`ClientEvent::Other`]
    ///
    /// Returns `Ok(None)` once the server has signalled a clean stream
    /// end (`StreamEOF`) or once the TCP read half observes EOF /
    /// connection-reset. After `Ok(None)` is returned the caller
    /// should stop writing and finish the session with
    /// [`close`](Self::close).
    ///
    /// This is a blocking call. Set a finite read timeout on
    /// [`inner_mut`](Self::inner_mut) ahead of time if you want
    /// `poll_event` to return periodically with an `Err(Error::Io)`
    /// kind `WouldBlock` / `TimedOut` so an outer event loop can do
    /// other work between polls — the underlying TCP read deadline is
    /// the timeout granularity, not a poll interval.
    pub fn poll_event(&mut self) -> Result<Option<ClientEvent>> {
        loop {
            if self.read_eof {
                return Ok(None);
            }
            // Drain any sub-messages decomposed from a prior
            // server-originated Aggregate Message (RTMP 1.0 §7.1.6)
            // ahead of any further wire read so the publish order is
            // preserved.
            if let Some(sub) = self.pending_subs.pop_front() {
                let ev = self.classify_message(sub)?;
                if matches!(ev, ClientEvent::Other) {
                    // Don't return `Other` for a queued sub — the caller
                    // typically pumps `poll_event` to observe semantic
                    // events; an aggregate full of `Other` would
                    // otherwise return N `Other`s in a row.
                    continue;
                }
                return Ok(Some(ev));
            }
            let msg = match self.reader.read_message() {
                Ok(m) => m,
                Err(Error::Io(e))
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::UnexpectedEof | std::io::ErrorKind::ConnectionReset
                    ) =>
                {
                    self.read_eof = true;
                    return Ok(None);
                }
                Err(Error::UnexpectedEof) => {
                    self.read_eof = true;
                    return Ok(None);
                }
                Err(e) => return Err(e),
            };
            // §5.3: acknowledge once the server has sent a full window
            // of bytes since our last ack (window set by the server's
            // §5.5 Window Acknowledgement Size / §5.6 Set Peer
            // Bandwidth).
            self.maybe_send_ack()?;
            if msg.msg_type_id == MSG_AGGREGATE {
                // RTMP 1.0 §7.1.6 Aggregate Message. Decompose into
                // FLV-shaped sub-messages with the §7.1.6 timestamp
                // re-normalisation applied and the message-stream-id
                // override resolved; queue them so subsequent calls
                // surface the per-sub events in publish order. Don't
                // return on the aggregate itself — drain the queue.
                let subs = parse_aggregate(&msg)?;
                self.pending_subs.extend(subs);
                continue;
            }
            return Ok(Some(self.classify_message(msg)?));
        }
    }

    /// Per-message classification shared between the wire-read path
    /// and the aggregate-sub-drain path.
    fn classify_message(&mut self, msg: Message) -> Result<ClientEvent> {
        match msg.msg_type_id {
            MSG_SET_CHUNK_SIZE => {
                let size = read_u32_be(&msg.payload)? & 0x7FFF_FFFF;
                self.reader.set_chunk_size(size as usize);
                Ok(ClientEvent::Other)
            }
            MSG_WINDOW_ACK_SIZE => {
                // §5.5: the server tells us which window size to use
                // when sending §5.3 Acknowledgements. Honour it so our
                // ack cadence matches the server's expectation.
                let size = read_u32_be(&msg.payload)?;
                self.reader.set_window_ack_size(size);
                Ok(ClientEvent::Other)
            }
            MSG_SET_PEER_BANDWIDTH => {
                // §5.4.5: run the Hard / Soft / Dynamic limit-type
                // state machine; when the effective window changes,
                // adopt it as our send-side ack window and answer with
                // the SHOULD-mandated Window Acknowledgement Size.
                let (window, limit) = parse_set_peer_bandwidth(&msg.payload)?;
                if let Some(w) = self.peer_bw.apply(window, limit) {
                    self.reader.set_window_ack_size(w);
                    self.writer
                        .write_message(CSID_PROTOCOL_CONTROL, &build_window_ack_size(w))?;
                    self.writer.flush()?;
                }
                Ok(ClientEvent::Other)
            }
            MSG_ACK => {
                // The server's §5.3 sequence number (bytes it has
                // received from us). Informational for a publisher.
                Ok(ClientEvent::Other)
            }
            MSG_USER_CONTROL => {
                let (event_type, event_data) = parse_user_control(&msg.payload)?;
                match event_type {
                    USR_STREAM_BEGIN => {
                        let sid = ucm_stream_id(event_data)?;
                        Ok(ClientEvent::StreamBegin { stream_id: sid })
                    }
                    USR_STREAM_EOF => {
                        let sid = ucm_stream_id(event_data)?;
                        // Don't latch here: the server typically sends
                        // a trailing onStatus / Unpublish.Success after
                        // StreamEOF, then half-closes; we let the
                        // subsequent read drain those and report EOF
                        // naturally.
                        Ok(ClientEvent::StreamEof { stream_id: sid })
                    }
                    USR_STREAM_DRY => {
                        // Per RTMP 1.0 §3.7, StreamDry is a transient
                        // "no data on the stream right now" signal —
                        // distinct from StreamEOF, which terminates
                        // playback. Surface to the caller so an outer
                        // event loop can react (e.g. warn UI, switch
                        // to a fallback stream); don't latch read_eof.
                        let sid = ucm_stream_id(event_data)?;
                        Ok(ClientEvent::StreamDry { stream_id: sid })
                    }
                    USR_SET_BUFFER_LENGTH => {
                        // Per RTMP 1.0 §3.7, SetBufferLength is the only
                        // standard UCM event with an 8-byte event-data
                        // body: 4-byte stream id + 4-byte buffer length
                        // in milliseconds. It is sent from a *playback*
                        // client to the server — a publish-direction
                        // client almost never sees it inbound. We
                        // validate the payload size so a malformed
                        // SetBufferLength from a confused peer surfaces
                        // a clean error instead of silently truncating;
                        // and we surface it as Other (no action required
                        // on the publisher side).
                        if event_data.len() < 8 {
                            return Err(Error::ProtocolViolation(
                                "UserControl SetBufferLength: event data < 8 bytes".into(),
                            ));
                        }
                        Ok(ClientEvent::Other)
                    }
                    USR_STREAM_IS_RECORDED => {
                        // Per RTMP 1.0 §3.7, server announces that the
                        // stream is a recorded (on-demand) stream.
                        // Surface to the caller — a publish client may
                        // want to log this if the server marks our own
                        // publish stream as recorded after we asked for
                        // "live" (mismatched publish type).
                        let sid = ucm_stream_id(event_data)?;
                        Ok(ClientEvent::StreamIsRecorded { stream_id: sid })
                    }
                    USR_PING_REQUEST => {
                        // Server pings — reply with PingResponse echoing
                        // the same 4-byte timestamp body so the server's
                        // liveness probe succeeds.
                        let ts_bytes = event_data;
                        if ts_bytes.len() >= 4 {
                            let mut p = Vec::with_capacity(6);
                            p.extend_from_slice(&USR_PING_RESPONSE.to_be_bytes());
                            p.extend_from_slice(&ts_bytes[..4]);
                            let _ = self.writer.write_message(
                                CSID_PROTOCOL_CONTROL,
                                &Message {
                                    msg_type_id: MSG_USER_CONTROL,
                                    msg_stream_id: 0,
                                    timestamp: 0,
                                    payload: p,
                                },
                            );
                            let _ = self.writer.flush();
                        }
                        Ok(ClientEvent::Other)
                    }
                    USR_PING_RESPONSE => {
                        // Per RTMP 1.0 §3.7, this echoes back the 4-byte
                        // timestamp the publisher carried in a prior
                        // PingRequest. Surface to the caller so a
                        // round-trip-time measurement loop can compare
                        // the echoed value to its own send-time clock.
                        if event_data.len() < 4 {
                            return Err(Error::ProtocolViolation(
                                "UserControl PingResponse: event data < 4 bytes".into(),
                            ));
                        }
                        let ts = u32::from_be_bytes([
                            event_data[0],
                            event_data[1],
                            event_data[2],
                            event_data[3],
                        ]);
                        Ok(ClientEvent::PingResponse { timestamp_ms: ts })
                    }
                    _ => {
                        // Unknown / reserved UCM event type — surface as
                        // Other; forwarding ingest may receive future
                        // event types we don't model yet.
                        Ok(ClientEvent::Other)
                    }
                }
            }
            MSG_COMMAND_AMF0 => {
                let values = amf::decode_all(&msg.payload)?;
                Ok(classify_command(values))
            }
            MSG_COMMAND_AMF3 => {
                let values = amf3::decode_message_to_amf0(&msg.payload)?;
                Ok(classify_command(values))
            }
            MSG_SHARED_OBJECT_AMF0 | MSG_SHARED_OBJECT_AMF3 => Ok(ClientEvent::SharedObject(
                crate::shared_object::parse_shared_object(&msg)?,
            )),
            MSG_AGGREGATE => {
                // A sub-message inside an aggregate is itself an
                // aggregate. Forward to the same queue so the next
                // `poll_event` tick decomposes it. The wire path above
                // already handles top-level aggregates directly.
                let subs = parse_aggregate(&msg)?;
                self.pending_subs.extend(subs);
                Ok(ClientEvent::Other)
            }
            _ => Ok(ClientEvent::Other),
        }
    }

    /// Issue a §7.2.1.2 NetConnection `call` RPC: "The call method of
    /// the NetConnection object runs remote procedure calls (RPC) at
    /// the receiving end. The called RPC name is passed as a parameter
    /// to the call command."
    ///
    /// With `expects_response`, a fresh non-zero transaction id is
    /// allocated and returned — match it against the
    /// [`ClientEvent::Result`] / [`ClientEvent::ErrorReply`] that
    /// [`poll_event`](Self::poll_event) later surfaces. Without it the
    /// spec's fire-and-forget transaction id 0 is used (and returned).
    /// `command_object` is `Amf0Value::Null` when there is no command
    /// info to attach.
    pub fn call(
        &mut self,
        procedure: &str,
        command_object: Amf0Value,
        arguments: &[Amf0Value],
        expects_response: bool,
    ) -> Result<f64> {
        let tx = if expects_response {
            let t = self.next_tx;
            self.next_tx += 1.0;
            t
        } else {
            0.0
        };
        self.writer.write_message(
            CSID_COMMAND,
            &build_call(procedure, tx, command_object, arguments),
        )?;
        self.writer.flush()?;
        Ok(tx)
    }

    /// Answer a server-initiated [`ClientEvent::Call`] with the
    /// §7.2.1.2 `_result(transaction_id, command_object, response)`
    /// structure.
    pub fn reply_call_result(
        &mut self,
        transaction_id: f64,
        command_object: Amf0Value,
        response: Amf0Value,
    ) -> Result<()> {
        self.writer.write_message(
            CSID_COMMAND,
            &build_call_result(transaction_id, command_object, response),
        )?;
        self.writer.flush()?;
        Ok(())
    }

    /// Failure counterpart of
    /// [`reply_call_result`](Self::reply_call_result).
    pub fn reply_call_error(
        &mut self,
        transaction_id: f64,
        command_object: Amf0Value,
        response: Amf0Value,
    ) -> Result<()> {
        self.writer.write_message(
            CSID_COMMAND,
            &build_call_error(transaction_id, command_object, response),
        )?;
        self.writer.flush()?;
        Ok(())
    }

    /// Send `closeStream` / `deleteStream` and shut the TCP socket.
    pub fn close(mut self) -> Result<()> {
        let tx = self.next_tx;
        self.next_tx += 1.0;
        let payload = amf::encode_command(
            "closeStream",
            tx,
            Amf0Value::Null,
            &[Amf0Value::Number(self.stream_id as f64)],
        );
        let _ = self.writer.write_message(
            CSID_COMMAND,
            &Message {
                msg_type_id: MSG_COMMAND_AMF0,
                msg_stream_id: self.stream_id,
                timestamp: 0,
                payload,
            },
        );
        let _ = self.writer.flush();
        // Shut down the write half only (send a graceful FIN) rather
        // than the whole socket. `Shutdown::Both` tears the read half
        // down at the same instant, which on some platforms makes the
        // kernel answer the peer's still-unacked data with a RST; that
        // RST discards any A/V messages the peer hasn't yet drained
        // from its receive buffer — closeStream and the last frames we
        // just wrote can be thrown away mid-stream. A write-half FIN
        // lets the peer read every buffered frame plus our closeStream
        // command, then observe EOF cleanly. The read half closes when
        // `self` (and its owned `TcpStream`) drops at end of scope.
        let _ = self.stream.shutdown(Shutdown::Write);
        // Drain the read half until the peer's FIN before `self` drops:
        // if the peer's final messages (status replies, acks, mirrored
        // teardown commands) were still unread when the descriptor
        // closed, the kernel would answer them with an RST — and an
        // RST may discard everything the peer has not yet read,
        // including the goodbye flushed above.
        crate::netutil::drain_until_fin(&self.stream, crate::netutil::DRAIN_BUDGET);
        Ok(())
    }

    pub fn inner_mut(&mut self) -> &mut TcpStream {
        &mut self.stream
    }

    /// Apply a `recv` timeout to the chunk reader's actual socket
    /// clone (the one [`poll_event`](Self::poll_event) blocks on)
    /// rather than the session's bookkeeping clone.
    ///
    /// On Linux a sockopt set through one `try_clone` descriptor
    /// carries to its sibling clones because they share one file
    /// description; on Windows each clone has its own kernel handle
    /// with independent socket options, so the timeout has to be
    /// installed on the exact socket the `recv` call will issue
    /// against. Use this rather than
    /// [`inner_mut`](Self::inner_mut)`.set_read_timeout(...)` when the
    /// goal is to bound `poll_event`'s block time.
    pub fn set_read_timeout(&mut self, d: Option<Duration>) -> Result<()> {
        self.reader.inner_mut().set_read_timeout(d)?;
        let _ = self.stream.set_read_timeout(d);
        Ok(())
    }
}

/// Consume messages from `reader` until we see a command named
/// `_result` for `expected_tx`. Forward relevant protocol-control
/// updates (SetChunkSize) to the reader.
pub(crate) fn wait_for_result<R: Read, W: Write>(
    reader: &mut ChunkReader<R>,
    _writer: &mut ChunkWriter<W>,
    expected_tx: f64,
) -> Result<Vec<Amf0Value>> {
    loop {
        let msg = reader.read_message()?;
        match msg.msg_type_id {
            MSG_SET_CHUNK_SIZE => {
                let size = read_u32_be(&msg.payload)? & 0x7FFF_FFFF;
                reader.set_chunk_size(size as usize);
            }
            MSG_WINDOW_ACK_SIZE => {
                // §5.5: capture the server's window during setup so the
                // §5.3 ack obligation is live before the first media
                // frame flows.
                let size = read_u32_be(&msg.payload)?;
                reader.set_window_ack_size(size);
            }
            MSG_SET_PEER_BANDWIDTH if msg.payload.len() >= 4 => {
                // §5.6: output bandwidth equals the peer's window size.
                let size = read_u32_be(&msg.payload[..4])?;
                reader.set_window_ack_size(size);
            }
            MSG_COMMAND_AMF0 => {
                let values = amf::decode_all(&msg.payload)?;
                let name = values.first().and_then(Amf0Value::as_str).unwrap_or("");
                let tx = values.get(1).and_then(Amf0Value::as_f64).unwrap_or(-1.0);
                if name == "_result" && tx == expected_tx {
                    return Ok(values);
                }
                if name == "_error" {
                    return Err(Error::Other(format!(
                        "RTMP _error from server: {:?}",
                        values.get(3)
                    )));
                }
                // Any other status notifications before our _result
                // (StreamBegin, bandwidth negotiations, etc.) — ignore.
            }
            _ => {}
        }
    }
}

pub(crate) fn wait_for_create_stream_result<R: Read, W: Write>(
    reader: &mut ChunkReader<R>,
    writer: &mut ChunkWriter<W>,
    expected_tx: f64,
) -> Result<u32> {
    let values = wait_for_result(reader, writer, expected_tx)?;
    // _result carries the new stream id as the last AMF0 value
    // (either arg slot [3] or further back if the peer sent an extra
    // props object).
    let sid = values
        .iter()
        .rev()
        .find_map(Amf0Value::as_f64)
        .ok_or_else(|| Error::InvalidCommand("createStream result has no stream id".into()))?;
    Ok(sid as u32)
}

fn wait_for_publish_start<R: Read, W: Write>(
    reader: &mut ChunkReader<R>,
    _writer: &mut ChunkWriter<W>,
) -> Result<()> {
    // Be lenient: the spec says the server SHOULD send an onStatus
    // with NetStream.Publish.Start, but some servers skip it. Bail
    // after we've seen a user-control StreamBegin OR an onStatus on
    // the publish stream.
    for _ in 0..20 {
        let msg = match reader.read_message() {
            Ok(m) => m,
            Err(Error::Io(ref e))
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                return Ok(());
            }
            Err(e) => return Err(e),
        };
        match msg.msg_type_id {
            MSG_USER_CONTROL => return Ok(()),
            MSG_COMMAND_AMF0 => {
                let values = amf::decode_all(&msg.payload)?;
                if values
                    .first()
                    .and_then(Amf0Value::as_str)
                    .map(|n| n == "onStatus" || n == "_result")
                    .unwrap_or(false)
                {
                    return Ok(());
                }
            }
            _ => {}
        }
    }
    Ok(())
}

pub(crate) fn read_u32_be(buf: &[u8]) -> Result<u32> {
    if buf.len() < 4 {
        return Err(Error::ProtocolViolation("need 4 bytes for u32be".into()));
    }
    Ok(u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]))
}

/// Locate the Enhanced RTMP capability block in a server's
/// `_result(connect, ...)` AMF0 value list.
///
/// Per `enhanced-rtmp-v2.pdf` §"Enhancing NetConnection connect Command"
/// the server stamps `videoFourCcInfoMap` / `audioFourCcInfoMap` /
/// `capsEx` etc. into one of the `_result` parameters; in practice
/// every implementation we have seen drops them into the trailing info
/// object (the slot that carries `NetConnection.Connect.Success`). Some
/// servers also drop them into the properties slot (the second AMF0
/// value, right after the transaction id), so we walk every
/// Object / ECMA-array in the response and return the first one whose
/// `ConnectCapabilities::from_amf0` carries an Enhanced-RTMP property.
///
/// `objectEncoding` alone doesn't count: every pre-2023 server stamps
/// `objectEncoding = 0` into its info object as part of the legacy
/// `NetConnection.Connect.Success` shape, so picking that up would make
/// every legacy server look like a v2 advertisement. A fully-legacy
/// server therefore returns the empty capability block.
pub(crate) fn extract_server_caps(values: &[Amf0Value]) -> ConnectCapabilities {
    for v in values {
        if matches!(v, Amf0Value::Object(_) | Amf0Value::EcmaArray(_)) {
            let caps = ConnectCapabilities::from_amf0(v);
            if has_enhanced_rtmp_property(&caps) {
                return caps;
            }
        }
    }
    ConnectCapabilities::default()
}

/// True if `caps` carries any of the Enhanced RTMP v1+v2 capability
/// properties (`fourCcList`, `videoFourCcInfoMap`,
/// `audioFourCcInfoMap`, `capsEx`). `objectEncoding` alone — the
/// legacy `_result(connect)` field — does not count.
fn has_enhanced_rtmp_property(caps: &ConnectCapabilities) -> bool {
    !caps.fourcc_list.is_empty()
        || !caps.video_fourcc_info_map.is_empty()
        || !caps.audio_fourcc_info_map.is_empty()
        || caps.caps_ex != 0
}

/// Decode the `User Control` payload framing per RTMP 1.0 §6.2 /
/// §7.1.7: a 16-bit BE event type followed by variable-length event
/// data. Returns `(event_type, event_data)` borrowed from the input.
fn parse_user_control(buf: &[u8]) -> Result<(u16, &[u8])> {
    if buf.len() < 2 {
        return Err(Error::ProtocolViolation(
            "UserControl: payload < 2 bytes".into(),
        ));
    }
    let event_type = u16::from_be_bytes([buf[0], buf[1]]);
    Ok((event_type, &buf[2..]))
}

/// Stream-id-carrying UCM events (Stream Begin / Stream EOF / Stream
/// Dry / StreamIsRecorded) all use a 4-byte BE stream id as their
/// event data.
fn ucm_stream_id(event_data: &[u8]) -> Result<u32> {
    if event_data.len() < 4 {
        return Err(Error::ProtocolViolation(
            "UserControl: event data < 4 bytes (need stream id)".into(),
        ));
    }
    Ok(u32::from_be_bytes([
        event_data[0],
        event_data[1],
        event_data[2],
        event_data[3],
    ]))
}

/// Resolve a reconnect-target URI reference against the current
/// connection's `tcUrl`, per `enhanced-rtmp-v2.pdf` §"Reconnect
/// Request" ("a relative URI reference should be resolved relative to
/// the tcUrl for the current connection"). Handles the four reference
/// shapes the spec's Info Object table gives as examples:
///
/// 1. `rtmp://foo.mydomain.com:1935/realtimeapp` — absolute (has a
///    scheme): taken verbatim.
/// 2. `//192.0.2.0/realtimeapp` — network-path reference: inherits
///    only the base's scheme.
/// 3. `/realtimeapp` — absolute-path reference: inherits the base's
///    scheme + authority.
/// 4. `realtimeapp` — relative-path reference: merged onto the base's
///    path with the last segment replaced.
///
/// An empty reference resolves to the base itself.
pub fn resolve_tc_url(base: &str, reference: &str) -> String {
    if reference.is_empty() {
        return base.to_owned();
    }
    if reference.contains("://") {
        // Absolute URI reference — already carries its own scheme.
        return reference.to_owned();
    }
    let (scheme, after_scheme) = match base.find("://") {
        Some(i) => (&base[..i], &base[i + 3..]),
        None => ("rtmp", base),
    };
    if let Some(net_path) = reference.strip_prefix("//") {
        // Network-path reference: keep only our scheme.
        return format!("{scheme}://{net_path}");
    }
    let (authority, base_path) = match after_scheme.find('/') {
        Some(i) => (&after_scheme[..i], &after_scheme[i..]),
        None => (after_scheme, ""),
    };
    if reference.starts_with('/') {
        // Absolute-path reference: keep scheme + authority.
        return format!("{scheme}://{authority}{reference}");
    }
    // Relative-path reference: merge — drop the base path's last
    // segment, keep everything up to (and including) its final '/'.
    let dir = match base_path.rfind('/') {
        Some(i) => &base_path[..=i],
        None => "/",
    };
    format!("{scheme}://{authority}{dir}{reference}")
}

/// Classify a decoded AMF0 command message into a [`ClientEvent`].
/// Matches `onStatus` / `_result` / `_error` by name and pulls the
/// transaction id / info object out of the expected slots.
fn classify_command(values: Vec<Amf0Value>) -> ClientEvent {
    let name = values.first().and_then(Amf0Value::as_str).unwrap_or("");
    match name {
        "onStatus" => {
            // ["onStatus", 0.0, null, <info-object>]
            if let Some(info) = values.get(3) {
                let level = info
                    .get("level")
                    .and_then(Amf0Value::as_str)
                    .unwrap_or("")
                    .to_owned();
                let code = info
                    .get("code")
                    .and_then(Amf0Value::as_str)
                    .unwrap_or("")
                    .to_owned();
                let description = info
                    .get("description")
                    .and_then(Amf0Value::as_str)
                    .unwrap_or("")
                    .to_owned();
                // Enhanced RTMP v2 §"Reconnect Request": the server
                // asks us to reconnect via a NetConnection-level
                // onStatus whose code MUST be
                // NetConnection.Connect.ReconnectRequest and whose
                // level MUST be "status". Lift the optional tcUrl so
                // the caller can re-dial; an event with the right code
                // but the wrong level is NOT a valid reconnect request
                // per spec, so it falls through as a plain OnStatus.
                if code == crate::message::RECONNECT_REQUEST_CODE && level == "status" {
                    let tc_url = info
                        .get("tcUrl")
                        .and_then(Amf0Value::as_str)
                        .map(str::to_owned);
                    return ClientEvent::ReconnectRequest {
                        tc_url,
                        description,
                    };
                }
                return ClientEvent::OnStatus {
                    level,
                    code,
                    description,
                };
            }
            ClientEvent::Other
        }
        "_result" => {
            let tx = values.get(1).and_then(Amf0Value::as_f64).unwrap_or(0.0);
            ClientEvent::Result {
                transaction_id: tx,
                values,
            }
        }
        "_error" => {
            let tx = values.get(1).and_then(Amf0Value::as_f64).unwrap_or(0.0);
            ClientEvent::ErrorReply {
                transaction_id: tx,
                values,
            }
        }
        _ => {
            // §7.2.1.2: a call RPC's procedure name rides the
            // command-name field, so any non-built-in command from the
            // server is an RPC aimed at this client.
            if !crate::message::is_reserved_command_name(name) {
                if let Some(call) = CallCommand::parse(&values) {
                    return ClientEvent::Call(call);
                }
            }
            ClientEvent::Other
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_user_control_stream_eof_recovers_stream_id() {
        // Wire layout per RTMP 1.0 §7.1.7: [event_type=0x0001 BE]
        // [stream_id BE]. Our build_user_control_stream_eof emits this
        // exact 6-byte body — same six bytes the auditor's
        // session_close test asserts on.
        let payload: [u8; 6] = [0x00, 0x01, 0x00, 0x00, 0x00, 0x07];
        let (event_type, event_data) = parse_user_control(&payload).expect("parse UCM");
        assert_eq!(event_type, USR_STREAM_EOF);
        assert_eq!(ucm_stream_id(event_data).expect("sid"), 7);
    }

    #[test]
    fn parse_user_control_rejects_truncated_payload() {
        // < 2 bytes — can't even read the event type.
        assert!(parse_user_control(&[0x00]).is_err());
        assert!(parse_user_control(&[]).is_err());
        // 2 bytes (event type only) but the event type is a
        // stream-id-carrying variant: event-data is empty so the SID
        // extractor refuses.
        let (event_type, event_data) = parse_user_control(&[0x00, 0x01]).expect("parse UCM");
        assert_eq!(event_type, USR_STREAM_EOF);
        assert!(ucm_stream_id(event_data).is_err());
    }

    #[test]
    fn classify_command_recognises_on_status() {
        let info = Amf0Value::Object(vec![
            ("level".into(), Amf0Value::String("status".into())),
            (
                "code".into(),
                Amf0Value::String("NetStream.Publish.Start".into()),
            ),
            ("description".into(), Amf0Value::String("ready".into())),
        ]);
        let values = vec![
            Amf0Value::String("onStatus".into()),
            Amf0Value::Number(0.0),
            Amf0Value::Null,
            info,
        ];
        match classify_command(values) {
            ClientEvent::OnStatus {
                level,
                code,
                description,
            } => {
                assert_eq!(level, "status");
                assert_eq!(code, "NetStream.Publish.Start");
                assert_eq!(description, "ready");
            }
            other => panic!("expected OnStatus, got {other:?}"),
        }
    }

    /// Enhanced RTMP v2 §"Reconnect Request": an onStatus whose code
    /// is `NetConnection.Connect.ReconnectRequest` (level `status`)
    /// lifts to [`ClientEvent::ReconnectRequest`] with the optional
    /// `tcUrl` extracted — and the same code under a non-`status`
    /// level is NOT a valid reconnect request per spec ("to reconnect
    /// the level MUST be set to status"), so it stays a plain
    /// OnStatus.
    #[test]
    fn classify_command_recognises_reconnect_request() {
        let msg = crate::message::build_reconnect_request(
            Some("//192.0.2.0/realtimeapp"),
            Some("server update"),
        );
        let values = amf::decode_all(&msg.payload).unwrap();
        match classify_command(values) {
            ClientEvent::ReconnectRequest {
                tc_url,
                description,
            } => {
                assert_eq!(tc_url.as_deref(), Some("//192.0.2.0/realtimeapp"));
                assert_eq!(description, "server update");
            }
            other => panic!("expected ReconnectRequest, got {other:?}"),
        }

        // tcUrl omitted → None ("use the tcUrl for the current
        // connection").
        let msg = crate::message::build_reconnect_request(None, None);
        let values = amf::decode_all(&msg.payload).unwrap();
        match classify_command(values) {
            ClientEvent::ReconnectRequest { tc_url, .. } => assert_eq!(tc_url, None),
            other => panic!("expected ReconnectRequest, got {other:?}"),
        }

        // Wrong level → plain OnStatus passthrough.
        let info = Amf0Value::Object(vec![
            ("level".into(), Amf0Value::String("error".into())),
            (
                "code".into(),
                Amf0Value::String(crate::message::RECONNECT_REQUEST_CODE.into()),
            ),
        ]);
        let values = vec![
            Amf0Value::String("onStatus".into()),
            Amf0Value::Number(0.0),
            Amf0Value::Null,
            info,
        ];
        match classify_command(values) {
            ClientEvent::OnStatus { level, code, .. } => {
                assert_eq!(level, "error");
                assert_eq!(code, crate::message::RECONNECT_REQUEST_CODE);
            }
            other => panic!("expected OnStatus, got {other:?}"),
        }
    }

    /// The four reference shapes from the spec's Info Object table
    /// (`enhanced-rtmp-v2.pdf` §"Reconnect Request"), resolved against
    /// a typical publisher tcUrl.
    #[test]
    fn resolve_tc_url_spec_example_shapes() {
        let base = "rtmp://origin.example.com:1935/live";
        // 1. Absolute URI reference — verbatim.
        assert_eq!(
            resolve_tc_url(base, "rtmp://foo.mydomain.com:1935/realtimeapp"),
            "rtmp://foo.mydomain.com:1935/realtimeapp"
        );
        // 2. Network-path reference — inherits only our scheme.
        assert_eq!(
            resolve_tc_url(base, "//192.0.2.0/realtimeapp"),
            "rtmp://192.0.2.0/realtimeapp"
        );
        // 3. Absolute-path reference — inherits scheme + authority.
        assert_eq!(
            resolve_tc_url(base, "/realtimeapp"),
            "rtmp://origin.example.com:1935/realtimeapp"
        );
        // 4. Relative-path reference — merged onto the base path with
        //    the last segment replaced.
        assert_eq!(
            resolve_tc_url(base, "realtimeapp"),
            "rtmp://origin.example.com:1935/realtimeapp"
        );
        // Deeper base path: only the last segment is replaced.
        assert_eq!(
            resolve_tc_url("rtmp://h/app/inst", "other"),
            "rtmp://h/app/other"
        );
        // Empty reference → base itself.
        assert_eq!(resolve_tc_url(base, ""), base);
        // Base without any path: relative ref lands at the root.
        assert_eq!(
            resolve_tc_url("rtmp://h:1935", "realtimeapp"),
            "rtmp://h:1935/realtimeapp"
        );
    }

    #[test]
    fn classify_command_recognises_result_and_error() {
        let result = classify_command(vec![
            Amf0Value::String("_result".into()),
            Amf0Value::Number(42.0),
            Amf0Value::Null,
            Amf0Value::Number(7.0),
        ]);
        match result {
            ClientEvent::Result {
                transaction_id,
                values,
            } => {
                assert_eq!(transaction_id, 42.0);
                assert_eq!(values.len(), 4);
            }
            other => panic!("expected Result, got {other:?}"),
        }

        let err = classify_command(vec![
            Amf0Value::String("_error".into()),
            Amf0Value::Number(99.0),
            Amf0Value::Null,
            Amf0Value::Null,
        ]);
        match err {
            ClientEvent::ErrorReply { transaction_id, .. } => {
                assert_eq!(transaction_id, 99.0);
            }
            other => panic!("expected ErrorReply, got {other:?}"),
        }
    }
}
