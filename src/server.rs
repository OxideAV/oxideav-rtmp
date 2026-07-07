//! RTMP server: accepts incoming publishers (§4.2.6 `publish`) and
//! subscribers (§4.2.1 `play`).
//!
//! The exposed flow is intentionally two-phase so consumers can
//! verify stream keys / auth:
//!
//! ```text
//!   let server = RtmpServer::bind("0.0.0.0:1935")?;
//!   loop {
//!       let req = server.accept()?;
//!       if !my_auth(&req.app, &req.stream_name) {
//!           req.reject("unauthorized")?;
//!           continue;
//!       }
//!       let mut session = req.accept()?;
//!       while let Some(pkt) = session.next_packet()? { … }
//!   }
//! ```
//!
//! [`RtmpServer::serve`] wraps the above in a thread-per-connection
//! loop for callers who want to handle many publishers at once.
//! Single-client use — the typical oxideav case — just calls
//! [`RtmpServer::accept`] directly.

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::thread;
use std::time::Duration;

use crate::aggregate::parse_aggregate;
use crate::amf::{self, Amf0Value};
use crate::amf3;
use crate::caps::ConnectCapabilities;
use crate::chunk::{ChunkReader, ChunkWriter, Message};
use crate::error::{Error, Result};
use crate::flv::{parse_audio, parse_video, AudioTag, VideoTag};
use crate::message::*;

/// After-connect server chunk size. Larger = fewer chunk headers per
/// message. 4 KiB is what most commodity ingest paths negotiate in practice.
const SERVER_CHUNK_SIZE: u32 = 4096;
/// Initial window-ack size advertised to the peer. Values of this
/// order are what "normal" RTMP servers announce.
const WINDOW_ACK_SIZE: u32 = 5_000_000;
/// `limit_type` for SetPeerBandwidth — 2 = "dynamic".
const PEER_BW_LIMIT_DYNAMIC: u8 = 2;

/// Listening socket for incoming RTMP publishers.
pub struct RtmpServer {
    listener: TcpListener,
    /// Enhanced RTMP capability block this server advertises in the
    /// `_result(connect)` info object (`videoFourCcInfoMap` / `capsEx`
    /// etc., per `enhanced-rtmp-v2.pdf` §"Enhancing NetConnection
    /// connect Command"). Defaults to empty so legacy publishers see
    /// the pre-2023 byte layout exactly. Mutate with
    /// [`set_capabilities`](Self::set_capabilities).
    capabilities: ConnectCapabilities,
}

impl RtmpServer {
    pub fn bind(addr: impl ToSocketAddrs) -> Result<Self> {
        let listener = TcpListener::bind(addr)?;
        Ok(Self {
            listener,
            capabilities: ConnectCapabilities::default(),
        })
    }

    pub fn local_addr(&self) -> Result<SocketAddr> {
        Ok(self.listener.local_addr()?)
    }

    /// Advertise the given Enhanced RTMP v1+v2 capabilities to every
    /// subsequent `accept`-ed publisher. The block is appended to the
    /// `_result(connect)` info object alongside the standard
    /// `NetConnection.Connect.Success` status; legacy publishers ignore
    /// the unknown properties and stay on the pre-2023 path. Pre-2023
    /// is also what `set_capabilities(ConnectCapabilities::default())`
    /// (or never calling this method) wires up.
    pub fn set_capabilities(&mut self, caps: ConnectCapabilities) -> &mut Self {
        self.capabilities = caps;
        self
    }

    /// Capability block this server currently advertises.
    pub fn capabilities(&self) -> &ConnectCapabilities {
        &self.capabilities
    }

    /// Accept one connection, run the handshake + connect + publish
    /// setup, and return the first point where the consumer gets to
    /// decide whether to take the stream.
    ///
    /// This is the publish-only entry point: a connection that issues
    /// a §4.2.1 `play` instead of `publish` is politely refused with
    /// `onStatus(NetStream.Play.StreamNotFound)` (the spec's
    /// stream-not-found refusal) and the server keeps listening for a
    /// publisher. Use [`accept_any`](Self::accept_any) to serve both
    /// directions.
    pub fn accept(&self) -> Result<PublishRequest> {
        loop {
            match self.accept_any()? {
                SessionRequest::Publish(req) => return Ok(req),
                SessionRequest::Play(req) => {
                    // A publish-only endpoint has no streams to serve:
                    // per §4.2.1 "if the stream to be played is not
                    // found, the Server sends the onStatus message
                    // NetStream.Play.StreamNotFound."
                    let peer = req.peer_addr;
                    let _ = req.reject("publish-only endpoint");
                    eprintln!("oxideav-rtmp: refused play request from {peer}");
                }
            }
        }
    }

    /// Accept one connection and drive it until the peer announces its
    /// direction: `publish` (it wants to send us a stream) or `play`
    /// (it wants to receive one). Returns the matching request so the
    /// consumer can authenticate, then [`PublishRequest::accept`] /
    /// [`PlayRequest::accept`] or `reject` it.
    pub fn accept_any(&self) -> Result<SessionRequest> {
        loop {
            let (stream, peer_addr) = self.listener.accept()?;
            // Individual parse failures shouldn't bring down the
            // server — log via Err(...) once, then keep listening. A
            // caller that wants fine-grained control uses `incoming()`
            // plus their own handshake.
            match drive_until_request(stream, peer_addr, &self.capabilities) {
                Ok(req) => return Ok(req),
                Err(e) => {
                    eprintln!("oxideav-rtmp: dropped connection from {peer_addr}: {e}");
                }
            }
        }
    }

    /// Loop forever, spawning one thread per accepted publisher. The
    /// `handler` is called after `accept()` — i.e. it receives a
    /// `PublishRequest` it can accept / reject the same way the
    /// single-client path does.
    ///
    /// The handler should do its own work on the returned
    /// [`RtmpSession`] (call `next_packet` until it returns `None`,
    /// then drop). Panics in the handler are caught by the per-thread
    /// panic boundary.
    ///
    /// Publish-only: a play connection is refused with
    /// `onStatus(NetStream.Play.StreamNotFound)` before the handler is
    /// ever called. Use [`serve_sessions`](Self::serve_sessions) to
    /// handle both directions.
    pub fn serve<F>(&self, handler: F) -> Result<()>
    where
        F: Fn(PublishRequest) + Send + Sync + 'static,
    {
        self.serve_sessions(move |req| match req {
            SessionRequest::Publish(req) => handler(req),
            SessionRequest::Play(req) => {
                let peer = req.peer_addr;
                let _ = req.reject("publish-only endpoint");
                eprintln!("oxideav-rtmp: refused play request from {peer}");
            }
        })
    }

    /// Loop forever, spawning one thread per accepted connection, and
    /// hand each fully-negotiated [`SessionRequest`] — publish *or*
    /// play — to `handler`.
    ///
    /// A broadcast-style application typically routes
    /// [`SessionRequest::Publish`] into an ingest queue and serves each
    /// [`SessionRequest::Play`] subscriber a copy of the matching
    /// publisher's packets via [`PlaySession::forward`].
    pub fn serve_sessions<F>(&self, handler: F) -> Result<()>
    where
        F: Fn(SessionRequest) + Send + Sync + 'static,
    {
        use std::sync::Arc;
        let handler = Arc::new(handler);
        let caps = Arc::new(self.capabilities.clone());
        for conn in self.listener.incoming() {
            let stream = match conn {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("oxideav-rtmp: accept failed: {e}");
                    continue;
                }
            };
            let peer_addr = match stream.peer_addr() {
                Ok(a) => a,
                Err(_) => continue,
            };
            let h = handler.clone();
            let c = caps.clone();
            thread::Builder::new()
                .name(format!("oxideav-rtmp-session-{peer_addr}"))
                .spawn(move || match drive_until_request(stream, peer_addr, &c) {
                    Ok(req) => h(req),
                    Err(e) => {
                        eprintln!("oxideav-rtmp: dropped connection from {peer_addr}: {e}");
                    }
                })
                .map_err(|e| Error::Other(format!("spawn session thread: {e}")))?;
        }
        Ok(())
    }
}

/// A fully-negotiated inbound connection, classified by the direction
/// the peer announced after `createStream`:
///
/// * [`Publish`](Self::Publish) — the peer issued a §4.2.6 `publish`
///   command; it wants to *send* us a stream.
/// * [`Play`](Self::Play) — the peer issued a §4.2.1 `play` command;
///   it wants to *receive* one.
#[allow(clippy::large_enum_variant)]
pub enum SessionRequest {
    /// Incoming publisher (§4.2.6) — accept to pump packets *in*.
    Publish(PublishRequest),
    /// Incoming subscriber (§4.2.1) — accept to send packets *out*.
    Play(PlayRequest),
}

/// The protocol has gotten through `publish` — we know which app the
/// client connected to and the stream name (commonly the stream key).
/// Consumer decides whether to accept.
pub struct PublishRequest {
    pub app: String,
    pub stream_name: String,
    /// Usually `"live"`; occasionally `"record"` or `"append"`.
    pub publish_type: String,
    pub peer_addr: SocketAddr,
    /// The `tcUrl` field from the client's connect command — useful
    /// when consumers want the full url for logging.
    pub tc_url: String,
    /// Enhanced RTMP v1+v2 capability block lifted from the publisher's
    /// `connect` Command Object (`fourCcList` /
    /// `audio|videoFourCcInfoMap` / `capsEx`, per
    /// `enhanced-rtmp-v2.pdf` §"Enhancing NetConnection connect
    /// Command"). Empty for legacy publishers that don't advertise any
    /// E-RTMP capabilities.
    pub capabilities: ConnectCapabilities,
    pending: PendingSession,
}

struct PendingSession {
    stream: TcpStream,
    reader: ChunkReader<TcpStream>,
    writer: ChunkWriter<TcpStream>,
    stream_id: u32,
    /// Kept in the struct so a future "send _result for publish"
    /// tweak can reference the right tx id. Currently we skip the
    /// _result and go straight to onStatus.
    #[allow(dead_code)]
    publish_tx_id: f64,
}

impl PublishRequest {
    /// Take the stream: send `NetStream.Publish.Start` and return a
    /// session the caller pumps via [`RtmpSession::next_packet`].
    pub fn accept(self) -> Result<RtmpSession> {
        let PublishRequest {
            app,
            stream_name,
            publish_type,
            peer_addr,
            tc_url: _,
            capabilities: _,
            pending,
        } = self;
        let PendingSession {
            stream,
            reader,
            mut writer,
            stream_id,
            publish_tx_id: _,
        } = pending;

        writer.write_message(
            CSID_PROTOCOL_CONTROL,
            &build_user_control_stream_begin(stream_id),
        )?;
        writer.write_message(
            CSID_COMMAND,
            &build_on_status(
                stream_id,
                "status",
                STATUS_PUBLISH_START,
                &format!("Started publishing {stream_name}"),
            ),
        )?;
        writer.flush()?;

        Ok(RtmpSession {
            stream,
            reader,
            writer,
            app,
            stream_name,
            publish_type,
            peer_addr,
            stream_id,
            ended: false,
            pending_subs: VecDeque::new(),
        })
    }

    /// Politely reject the publish: emit `NetStream.Publish.BadName`
    /// with `reason` as the description, then drop the connection.
    pub fn reject(self, reason: &str) -> Result<()> {
        let PublishRequest { pending, .. } = self;
        let PendingSession {
            stream,
            mut writer,
            stream_id,
            ..
        } = pending;
        let _ = writer.write_message(
            CSID_COMMAND,
            &build_on_status(stream_id, "error", STATUS_PUBLISH_BAD_NAME, reason),
        );
        let _ = writer.flush();
        let _ = stream.shutdown(Shutdown::Both);
        Err(Error::Rejected(reason.to_string()))
    }
}

/// The protocol has gotten through a §4.2.1 `play` command — the peer
/// is a subscriber asking to *receive* the named stream. Consumer
/// decides whether to serve it.
pub struct PlayRequest {
    /// Application name from the peer's `connect` command.
    pub app: String,
    /// §4.2.1 Stream Name argument (may carry an `mp3:` / `mp4:`
    /// prefix per the spec's naming table).
    pub stream_name: String,
    /// §4.2.1 optional Start argument in seconds. Spec default −2
    /// ("first try the live stream, then the recorded one"); −1 means
    /// live only; ≥ 0 seeks a recorded stream. `None` when omitted.
    pub start: Option<f64>,
    /// §4.2.1 optional Duration argument in seconds (default −1 =
    /// until the stream ends). `None` when omitted.
    pub duration: Option<f64>,
    /// §4.2.1 optional Reset flag — flush any queued playlist first.
    /// Drives whether [`accept`](Self::accept) emits
    /// `NetStream.Play.Reset` ("sent by the server only if the play
    /// command sent by the client has set the reset flag").
    pub reset: Option<bool>,
    /// §3.7 `SetBufferLength` observed before the `play` command, in
    /// milliseconds — the subscriber's requested buffer depth. `None`
    /// when the peer never sent one.
    pub buffer_length_ms: Option<u32>,
    pub peer_addr: SocketAddr,
    /// The `tcUrl` field from the subscriber's connect command.
    pub tc_url: String,
    /// Enhanced RTMP v1+v2 capability block from the subscriber's
    /// `connect` Command Object. Empty for legacy players.
    pub capabilities: ConnectCapabilities,
    pending: PendingSession,
}

impl PlayRequest {
    /// Serve the subscriber a **live** stream: run the §4.2.1
    /// Figure 5 acceptance sequence — `UserControl StreamBegin`,
    /// `onStatus(NetStream.Play.Reset)` (only when the peer's play
    /// command set the reset flag, per spec), then
    /// `onStatus(NetStream.Play.Start)` — and return the
    /// [`PlaySession`] to feed via `send_audio` / `send_video` /
    /// [`forward`](PlaySession::forward).
    ///
    /// The Figure 5 `SetChunkSize` step was already performed right
    /// after `connect` (chunk size is connection-level state), and
    /// `StreamIsRecorded` is skipped for a live stream — use
    /// [`accept_recorded`](Self::accept_recorded) when serving
    /// recorded / seekable content.
    pub fn accept(self) -> Result<PlaySession> {
        self.accept_mode(false)
    }

    /// Same as [`accept`](Self::accept) but announces the stream as
    /// recorded: the Figure 5 `UserControl StreamIsRecorded` event is
    /// emitted ahead of `StreamBegin`, matching the spec's flow ("the
    /// server sends this event to notify the client that the stream
    /// is a recorded stream").
    pub fn accept_recorded(self) -> Result<PlaySession> {
        self.accept_mode(true)
    }

    fn accept_mode(self, recorded: bool) -> Result<PlaySession> {
        let PlayRequest {
            app,
            stream_name,
            reset,
            peer_addr,
            pending,
            ..
        } = self;
        let PendingSession {
            stream,
            reader,
            mut writer,
            stream_id,
            publish_tx_id: _,
        } = pending;

        if recorded {
            writer.write_message(
                CSID_PROTOCOL_CONTROL,
                &build_user_control_stream_is_recorded(stream_id),
            )?;
        }
        writer.write_message(
            CSID_PROTOCOL_CONTROL,
            &build_user_control_stream_begin(stream_id),
        )?;
        if reset == Some(true) {
            // "NetStream.Play.Reset is sent by the server only if the
            // play command sent by the client has set the reset flag."
            writer.write_message(
                CSID_COMMAND,
                &build_on_status(
                    stream_id,
                    "status",
                    STATUS_PLAY_RESET,
                    &format!("Playing and resetting {stream_name}"),
                ),
            )?;
        }
        writer.write_message(
            CSID_COMMAND,
            &build_on_status(
                stream_id,
                "status",
                STATUS_PLAY_START,
                &format!("Started playing {stream_name}"),
            ),
        )?;
        writer.flush()?;

        Ok(PlaySession {
            stream,
            reader,
            writer,
            app,
            stream_name,
            peer_addr,
            stream_id,
            ended: false,
            pending_subs: VecDeque::new(),
        })
    }

    /// Refuse the play request: emit the §4.2.1
    /// `onStatus(NetStream.Play.StreamNotFound)` error ("if the stream
    /// to be played is not found") with `reason` as the description,
    /// then drop the connection.
    pub fn reject(self, reason: &str) -> Result<()> {
        let PlayRequest { pending, .. } = self;
        let PendingSession {
            stream,
            mut writer,
            stream_id,
            ..
        } = pending;
        let _ = writer.write_message(
            CSID_COMMAND,
            &build_on_status(stream_id, "error", STATUS_PLAY_STREAM_NOT_FOUND, reason),
        );
        let _ = writer.flush();
        let _ = stream.shutdown(Shutdown::Both);
        Err(Error::Rejected(reason.to_string()))
    }
}

/// Subscriber-originated event observed by a [`PlaySession`] while
/// serving a play stream.
#[derive(Debug, Clone, PartialEq)]
pub enum PlaySessionEvent {
    /// A §4.2 NetStream control command — `pause`, `seek`,
    /// `receiveAudio`, `receiveVideo`, or a further `play` / `play2`
    /// (playlist switch). The session does not act on these itself;
    /// the application decides (and typically answers `pause` / `seek`
    /// with [`PlaySession::notify_pause`] /
    /// [`PlaySession::notify_seek`] per §4.2.7 / §4.2.8).
    Command(NetStreamCommand),
    /// §3.7 `SetBufferLength` — the subscriber (re-)announced how many
    /// milliseconds of buffer it keeps filled. May arrive at any time
    /// (e.g. after a pause).
    SetBufferLength { buffer_ms: u32 },
    /// A §7.2.1.2 NetConnection `call` RPC from the subscriber. Reply
    /// with [`PlaySession::reply_call_result`] /
    /// [`PlaySession::reply_call_error`] when
    /// [`CallCommand::expects_response`].
    Call(CallCommand),
    /// The subscriber answered a server-initiated
    /// [`PlaySession::send_call`] with `_result` (`success == true`)
    /// or `_error`; `values` is the whole decoded response frame.
    CallReply {
        success: bool,
        transaction_id: f64,
        values: Vec<Amf0Value>,
    },
}

/// Active play (subscriber) session after [`PlayRequest::accept`].
///
/// The server side of RTMP's subscribe direction: push tags out with
/// [`send_audio`](Self::send_audio) / [`send_video`](Self::send_video)
/// / [`send_metadata`](Self::send_metadata) (or relay a whole
/// [`StreamPacket`] with [`forward`](Self::forward)), and pump
/// [`next_event`](Self::next_event) — typically from a companion
/// thread, or between sends with a read timeout — to observe the
/// subscriber's §4.2 control commands and detect teardown.
pub struct PlaySession {
    stream: TcpStream,
    reader: ChunkReader<TcpStream>,
    writer: ChunkWriter<TcpStream>,
    app: String,
    stream_name: String,
    peer_addr: SocketAddr,
    stream_id: u32,
    ended: bool,
    /// Sub-messages decomposed out of an Aggregate Message (type 22)
    /// per RTMP 1.0 §7.1.6 but not yet dispatched.
    pending_subs: VecDeque<Message>,
}

impl PlaySession {
    pub fn app(&self) -> &str {
        &self.app
    }
    pub fn stream_name(&self) -> &str {
        &self.stream_name
    }
    pub fn peer_addr(&self) -> SocketAddr {
        self.peer_addr
    }
    /// NetStream message stream id this session sends A/V on (the id
    /// returned to the subscriber from `_result(createStream)`).
    pub fn stream_id(&self) -> u32 {
        self.stream_id
    }

    /// Configure a read timeout on the socket clone
    /// [`next_event`](Self::next_event) blocks on, so a send loop can
    /// poll for subscriber commands between frames.
    pub fn set_read_timeout(&mut self, d: Option<Duration>) -> Result<()> {
        self.reader.inner_mut().set_read_timeout(d)?;
        let _ = self.stream.set_read_timeout(d);
        Ok(())
    }

    /// Send one audio tag at `timestamp_ms` on the play stream.
    pub fn send_audio(&mut self, timestamp_ms: u32, tag: &AudioTag) -> Result<()> {
        let payload = crate::flv::build_audio(tag);
        self.send_media(MSG_AUDIO, CSID_AUDIO, timestamp_ms, payload)
    }

    /// Send one video tag at `timestamp_ms` on the play stream.
    pub fn send_video(&mut self, timestamp_ms: u32, tag: &VideoTag) -> Result<()> {
        let payload = crate::flv::build_video(tag);
        self.send_media(MSG_VIDEO, CSID_VIDEO, timestamp_ms, payload)
    }

    fn send_media(&mut self, type_id: u8, csid: u32, ts: u32, payload: Vec<u8>) -> Result<()> {
        self.writer.write_message(
            csid,
            &Message {
                msg_type_id: type_id,
                msg_stream_id: self.stream_id,
                timestamp: ts,
                payload,
            },
        )?;
        self.writer.flush()?;
        Ok(())
    }

    /// Send an `onMetaData` data message to the subscriber. Unlike the
    /// publish direction there is no `@setDataFrame` RPC prefix — the
    /// server relays the bare `["onMetaData", meta]` pair.
    pub fn send_metadata(&mut self, metadata: &Amf0Value) -> Result<()> {
        self.writer
            .write_message(CSID_DATA, &build_on_meta_data(self.stream_id, metadata))?;
        self.writer.flush()?;
        Ok(())
    }

    /// Relay one publisher-side [`StreamPacket`] to this subscriber —
    /// the core of an RTMP fan-out: `Audio` / `Video` re-frame on this
    /// session's stream id at the packet's original timestamp,
    /// `Metadata` re-frames as a bare `onMetaData` data message, and a
    /// publisher-side `Command` / `Call` is not forwarded (control
    /// commands and RPCs are per-connection, not stream content).
    pub fn forward(&mut self, packet: &StreamPacket) -> Result<()> {
        match packet {
            StreamPacket::Audio { timestamp, tag } => self.send_audio(*timestamp, tag),
            StreamPacket::Video { timestamp, tag } => self.send_video(*timestamp, tag),
            StreamPacket::Metadata(meta) => self.send_metadata(meta),
            StreamPacket::Command(_) | StreamPacket::Call(_) | StreamPacket::CallReply { .. } => {
                Ok(())
            }
        }
    }

    /// Emit `onStatus(NetStream.Pause.Notify)` /
    /// `onStatus(NetStream.Unpause.Notify)` — the §4.2.8 replies to a
    /// subscriber's `pause` command ("the server sends a status
    /// message NetStream.Pause.Notify when the stream is paused.
    /// NetStream.Unpause.Notify is sent when a stream in un-paused").
    /// Call after honouring a [`NetStreamCommand::Pause`] event.
    pub fn notify_pause(&mut self, paused: bool) -> Result<()> {
        let (code, desc) = if paused {
            (STATUS_PAUSE_NOTIFY, "Pausing")
        } else {
            (STATUS_UNPAUSE_NOTIFY, "Unpausing")
        };
        self.send_status(code, desc)
    }

    /// Emit `onStatus(NetStream.Seek.Notify)` — the §4.2.7 reply to a
    /// subscriber's `seek` command ("the server sends a status message
    /// NetStream.Seek.Notify when seek is successful").
    pub fn notify_seek(&mut self) -> Result<()> {
        self.send_status(STATUS_SEEK_NOTIFY, "Seeking")
    }

    /// Emit the §4.2.4 / §4.2.5 reply to a `receiveAudio(true)` /
    /// `receiveVideo(true)` command. Per spec, "the server does not
    /// send any response, if the [receiveAudio / receiveVideo] command
    /// is sent with the bool flag set as false. If this flag is set to
    /// true, server responds with status messages NetStream.Seek.Notify
    /// and NetStream.Play.Start" — this helper sends exactly those two
    /// statuses, in that order. Call after honouring a
    /// [`NetStreamCommand::ReceiveAudio`] /
    /// [`NetStreamCommand::ReceiveVideo`] event carrying `true`; do
    /// not reply to the `false` form.
    pub fn notify_receive_resumed(&mut self) -> Result<()> {
        self.writer.write_message(
            CSID_COMMAND,
            &build_on_status(self.stream_id, "status", STATUS_SEEK_NOTIFY, "Seeking"),
        )?;
        self.writer.write_message(
            CSID_COMMAND,
            &build_on_status(
                self.stream_id,
                "status",
                STATUS_PLAY_START,
                &format!("Started playing {}", self.stream_name),
            ),
        )?;
        self.writer.flush()?;
        Ok(())
    }

    /// Emit an arbitrary `onStatus` on the play stream with
    /// `level = "status"`.
    pub fn send_status(&mut self, code: &str, description: &str) -> Result<()> {
        self.writer.write_message(
            CSID_COMMAND,
            &build_on_status(self.stream_id, "status", code, description),
        )?;
        self.writer.flush()?;
        Ok(())
    }

    /// Emit a `UserControl StreamDry(stream_id)` (§3.7 UCM 2) — "no
    /// more data on the stream" right now; typically sent when the
    /// upstream publisher stalls but the session should stay up.
    pub fn send_stream_dry(&mut self) -> Result<()> {
        self.writer.write_message(
            CSID_PROTOCOL_CONTROL,
            &build_user_control_stream_dry(self.stream_id),
        )?;
        self.writer.flush()?;
        Ok(())
    }

    /// Emit a `UserControl PingRequest(timestamp_ms)` (§3.7 UCM 6) to
    /// probe subscriber liveness; the peer echoes a `PingResponse`.
    pub fn send_ping_request(&mut self, timestamp_ms: u32) -> Result<()> {
        self.writer.write_message(
            CSID_PROTOCOL_CONTROL,
            &build_user_control_ping_request(timestamp_ms),
        )?;
        self.writer.flush()?;
        Ok(())
    }

    /// Read the next subscriber-originated event.
    ///
    /// Protocol control (Set Chunk Size / Window Ack Size / Set Peer
    /// Bandwidth / Acknowledgement) and §5.3 ack emission are handled
    /// internally; a §4.2 NetStream control command or a
    /// `SetBufferLength` surfaces as a [`PlaySessionEvent`]. Returns
    /// `Ok(None)` once the subscriber tears the stream down
    /// (`closeStream` / `deleteStream` per §4.2.3, or TCP EOF) —
    /// after which the session should be dropped or [`close`](Self::close)d.
    pub fn next_event(&mut self) -> Result<Option<PlaySessionEvent>> {
        while !self.ended {
            if let Some(sub) = self.pending_subs.pop_front() {
                if let Some(ev) = self.handle_event_message(sub)? {
                    return Ok(Some(ev));
                }
                continue;
            }
            let msg = match self.reader.read_message() {
                Ok(m) => m,
                Err(Error::Io(e))
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::UnexpectedEof | std::io::ErrorKind::ConnectionReset
                    ) =>
                {
                    self.ended = true;
                    return Ok(None);
                }
                Err(Error::UnexpectedEof) => {
                    self.ended = true;
                    return Ok(None);
                }
                Err(e) => return Err(e),
            };
            self.maybe_send_ack()?;
            if let Some(ev) = self.handle_event_message(msg)? {
                return Ok(Some(ev));
            }
        }
        Ok(None)
    }

    fn handle_event_message(&mut self, msg: Message) -> Result<Option<PlaySessionEvent>> {
        match msg.msg_type_id {
            MSG_COMMAND_AMF0 => {
                let values = amf::decode_all(&msg.payload)?;
                self.classify_command_values(&values)
            }
            MSG_COMMAND_AMF3 => {
                let values = amf3::decode_message_to_amf0(&msg.payload)?;
                self.classify_command_values(&values)
            }
            MSG_USER_CONTROL => {
                match UserControlEvent::parse(&msg.payload)? {
                    UserControlEvent::SetBufferLength { buffer_ms, .. } => {
                        Ok(Some(PlaySessionEvent::SetBufferLength { buffer_ms }))
                    }
                    UserControlEvent::PingRequest { timestamp_ms } => {
                        // Answer the liveness probe transparently.
                        self.writer.write_message(
                            CSID_PROTOCOL_CONTROL,
                            &build_user_control_ping_response(timestamp_ms),
                        )?;
                        self.writer.flush()?;
                        Ok(None)
                    }
                    _ => Ok(None),
                }
            }
            MSG_SET_CHUNK_SIZE => {
                let size = read_u32_be(&msg.payload)? & 0x7FFF_FFFF;
                self.reader.set_chunk_size(size as usize);
                Ok(None)
            }
            MSG_WINDOW_ACK_SIZE => {
                let size = read_u32_be(&msg.payload)?;
                self.reader.set_window_ack_size(size);
                Ok(None)
            }
            MSG_SET_PEER_BANDWIDTH => {
                if msg.payload.len() >= 4 {
                    let size = read_u32_be(&msg.payload[..4])?;
                    self.reader.set_window_ack_size(size);
                }
                Ok(None)
            }
            MSG_AGGREGATE => {
                let subs = parse_aggregate(&msg)?;
                self.pending_subs.extend(subs);
                Ok(None)
            }
            _ => Ok(None),
        }
    }

    fn classify_command_values(
        &mut self,
        values: &[Amf0Value],
    ) -> Result<Option<PlaySessionEvent>> {
        if let Some(name) = values.first().and_then(Amf0Value::as_str) {
            if matches!(name, "closeStream" | "deleteStream" | "close") {
                // §4.2.3: "NetStream sends the deleteStream command
                // when the NetStream object is getting destroyed. …
                // The server does not send any response." The §7.2.1
                // NetConnection `close` ends the session the same way.
                self.ended = true;
                return Ok(None);
            }
            if let Some(cmd) = NetStreamCommand::parse(values)? {
                return Ok(Some(PlaySessionEvent::Command(cmd)));
            }
            if matches!(name, "_result" | "_error") {
                let transaction_id = values.get(1).and_then(Amf0Value::as_f64).unwrap_or(0.0);
                return Ok(Some(PlaySessionEvent::CallReply {
                    success: name == "_result",
                    transaction_id,
                    values: values.to_vec(),
                }));
            }
            if !is_reserved_command_name(name) {
                // §7.2.1.2 RPC — see StreamPacket::Call.
                return Ok(CallCommand::parse(values).map(PlaySessionEvent::Call));
            }
            return Ok(None);
        }
        Ok(NetStreamCommand::parse(values)?.map(PlaySessionEvent::Command))
    }

    /// Issue a §7.2.1.2 NetConnection `call` RPC *to the subscriber*.
    /// Non-zero `transaction_id` requests a response (surfaced as
    /// [`PlaySessionEvent::CallReply`]); 0 is fire-and-forget.
    pub fn send_call(&mut self, call: &CallCommand) -> Result<()> {
        self.writer
            .write_message(CSID_COMMAND, &call.to_message())?;
        self.writer.flush()?;
        Ok(())
    }

    /// Answer a subscriber [`PlaySessionEvent::Call`] with the
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

    fn maybe_send_ack(&mut self) -> Result<()> {
        if let Some(seq) = self.reader.ack_due() {
            self.writer
                .write_message(CSID_PROTOCOL_CONTROL, &build_ack(seq))?;
            self.writer.flush()?;
        }
        Ok(())
    }

    /// End the play stream politely: emit `UserControl
    /// StreamEOF(stream_id)` — §7.1.7: "the server sends this event to
    /// notify the client that the playback of data is over as
    /// requested on this stream" — flush every buffered chunk, and
    /// half-close the write side so the subscriber drains everything
    /// before observing EOF.
    pub fn close(mut self) -> Result<()> {
        let _ = self.writer.write_message(
            CSID_PROTOCOL_CONTROL,
            &build_user_control_stream_eof(self.stream_id),
        );
        let _ = self.writer.flush();
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
}

/// Active publish after `accept`. Iterate via [`RtmpSession::next_packet`].
pub struct RtmpSession {
    stream: TcpStream,
    reader: ChunkReader<TcpStream>,
    writer: ChunkWriter<TcpStream>,
    app: String,
    stream_name: String,
    publish_type: String,
    peer_addr: SocketAddr,
    stream_id: u32,
    ended: bool,
    /// Sub-messages decomposed out of an Aggregate Message (type 22)
    /// per RTMP 1.0 §7.1.6 but not yet surfaced as a [`StreamPacket`].
    /// When [`next_packet`](Self::next_packet) sees a `MSG_AGGREGATE`
    /// on the wire, [`parse_aggregate`] splits the body into
    /// FLV-shaped sub-messages (audio / video / data / command) with
    /// the §7.1.6 timestamp re-normalisation already applied and the
    /// `msg_stream_id` override resolved to the aggregate's; those
    /// subs land here and the dispatch loop drains the queue ahead of
    /// every subsequent wire read so the caller observes the
    /// per-sub packets in the order the publisher packed them.
    pending_subs: VecDeque<Message>,
}

/// One media-layer event reported to the caller.
#[derive(Debug, Clone)]
pub enum StreamPacket {
    Audio {
        timestamp: u32,
        tag: AudioTag,
    },
    Video {
        timestamp: u32,
        tag: VideoTag,
    },
    /// `@setDataFrame("onMetaData", <amf0>)`. The AMF0 value is the
    /// metadata object (usually width, height, codec ids, framerate,
    /// bitrate, audiodatarate, ...).
    Metadata(Amf0Value),
    /// A NetStream control command the peer issued on the stream —
    /// `play` / `play2` / `pause` / `seek` / `receiveAudio` /
    /// `receiveVideo` per RTMP 1.0 Commands-Messages §4.2. Surfaced so
    /// a server application can react (e.g. honour `receiveAudio false`
    /// by suspending audio forwarding). The session does not act on
    /// these itself; teardown commands (`closeStream` / `deleteStream`
    /// / `FCUnpublish`) are still consumed silently and end the
    /// session.
    Command(NetStreamCommand),
    /// A §7.2.1.2 NetConnection `call` RPC from the peer — any command
    /// whose name is not a spec-defined built-in, since the RPC's
    /// procedure name rides the command-name field. Reply with
    /// [`RtmpSession::reply_call_result`] /
    /// [`RtmpSession::reply_call_error`] when
    /// [`CallCommand::expects_response`].
    Call(CallCommand),
    /// The peer answered a server-initiated
    /// [`RtmpSession::send_call`] with `_result` (`success == true`)
    /// or `_error`. `values` is the whole decoded §7.2.1.2 response
    /// frame (`[name, transaction_id, command_object, response]`);
    /// match `transaction_id` against the id passed to `send_call`.
    CallReply {
        success: bool,
        transaction_id: f64,
        values: Vec<Amf0Value>,
    },
}

impl RtmpSession {
    pub fn app(&self) -> &str {
        &self.app
    }
    pub fn stream_name(&self) -> &str {
        &self.stream_name
    }
    pub fn publish_type(&self) -> &str {
        &self.publish_type
    }
    pub fn peer_addr(&self) -> SocketAddr {
        self.peer_addr
    }

    /// Configure a read timeout on the underlying TCP socket — helpful
    /// when you want `next_packet` to return periodically so an outer
    /// shutdown signal can be observed. Passes through to
    /// [`TcpStream::set_read_timeout`].
    ///
    /// The timeout is applied to the chunk reader's actual socket
    /// clone (the one [`next_packet`](Self::next_packet) reads
    /// through) rather than the session's bookkeeping clone. On
    /// Linux a sockopt set through one `try_clone` descriptor carries
    /// to its sibling clones because they share one file description;
    /// Windows assigns each clone its own kernel handle with
    /// independent socket options, so the timeout must be installed
    /// on the exact socket that will issue the `recv` call.
    pub fn set_read_timeout(&mut self, d: Option<Duration>) -> Result<()> {
        self.reader.inner_mut().set_read_timeout(d)?;
        // Also apply to the bookkeeping clone for any future direct
        // reads through `self.stream` (none today, but defensive).
        let _ = self.stream.set_read_timeout(d);
        Ok(())
    }

    /// Emit a `UserControl StreamDry(stream_id)` event on the publish
    /// stream (RTMP 1.0 §3.7, UCM type 2).
    ///
    /// Per spec: "the server sends this event to notify the client
    /// that there is no more data on the stream. If the server does
    /// not detect any message for a time period, it can notify the
    /// subscribed clients that the stream is dry." Distinct from
    /// [`close`](Self::close)'s `StreamEOF`: `StreamDry` is a
    /// transient "we have nothing right now" signal that may resolve
    /// when more data arrives, not a teardown.
    pub fn send_stream_dry(&mut self) -> Result<()> {
        self.writer.write_message(
            CSID_PROTOCOL_CONTROL,
            &build_user_control_stream_dry(self.stream_id),
        )?;
        self.writer.flush()?;
        Ok(())
    }

    /// Emit a `UserControl StreamIsRecorded(stream_id)` event on the
    /// publish stream (RTMP 1.0 §3.7, UCM type 4).
    ///
    /// Per spec: "the server sends this event to notify the client
    /// that the stream is a recorded stream." A server fronting an
    /// archival recorder may want to advertise this after the publish
    /// handshake settles so a forwarding peer knows the captured
    /// stream is replayable rather than ephemeral.
    pub fn send_stream_is_recorded(&mut self) -> Result<()> {
        self.writer.write_message(
            CSID_PROTOCOL_CONTROL,
            &build_user_control_stream_is_recorded(self.stream_id),
        )?;
        self.writer.flush()?;
        Ok(())
    }

    /// Emit a `UserControl PingRequest(timestamp_ms)` event (RTMP 1.0
    /// §3.7, UCM type 6).
    ///
    /// Per spec, "the server sends this event to test whether the
    /// client is reachable. Event data is a 4-byte timestamp,
    /// representing the local server time when the server dispatched
    /// the command." The client (our [`RtmpClient`]) replies with the
    /// matching `PingResponse` carrying the same 4 bytes —
    /// `RtmpClient::poll_event` answers the ping internally without
    /// surfacing the request to the publisher caller.
    pub fn send_ping_request(&mut self, timestamp_ms: u32) -> Result<()> {
        self.writer.write_message(
            CSID_PROTOCOL_CONTROL,
            &build_user_control_ping_request(timestamp_ms),
        )?;
        self.writer.flush()?;
        Ok(())
    }

    /// Ask the publisher to reconnect — Enhanced RTMP v2 §"Reconnect
    /// Request".
    ///
    /// Emits the `onStatus(NetConnection.Connect.ReconnectRequest)`
    /// NetConnection command (message stream 0, transaction id 0, null
    /// Command Object). Per the spec's message flow, a server does
    /// this "prior to the shutdown of the live streaming server or
    /// when the server intends to remap the client to another server
    /// instance" — and when remapping, it MUST pass the target via
    /// `tc_url` (absolute or relative URI reference; `None` tells the
    /// client to re-dial the tcUrl of the current connection).
    ///
    /// After sending, the spec requires the old server to "continue
    /// processing messages from the client until the client
    /// disconnects" — so keep pumping
    /// [`next_packet`](Self::next_packet) as usual; the publisher
    /// drains up to its next appropriate media boundary (such as a
    /// keyframe) before it actually moves.
    ///
    /// Note: per §"Enhancing NetConnection connect Command" the peer
    /// advertises reconnect support via the `capsEx`
    /// [`CAPS_EX_RECONNECT`](crate::caps::CAPS_EX_RECONNECT) bit —
    /// check [`PublishRequest::capabilities`] before relying on the
    /// client honouring this event.
    pub fn send_reconnect_request(
        &mut self,
        tc_url: Option<&str>,
        description: Option<&str>,
    ) -> Result<()> {
        self.writer
            .write_message(CSID_COMMAND, &build_reconnect_request(tc_url, description))?;
        self.writer.flush()?;
        Ok(())
    }

    /// Close the session politely.
    ///
    /// On the wire we emit, in order:
    ///
    /// 1. A `UserControl StreamEOF(stream_id)` event so the peer's
    ///    chunk-stream state machine learns the publish is done before
    ///    it observes the TCP FIN (RTMP 1.0 §7.1.7).
    /// 2. `onStatus(NetStream.Unpublish.Success)` on the publish stream.
    /// 3. A chunk-writer `flush()` so every buffered chunk reaches the
    ///    kernel before the half-close.
    ///
    /// Then we send a write-half FIN (`Shutdown::Write`) rather than
    /// tearing both halves down at once. `Shutdown::Both` instantly
    /// closes the read half too, which on some platforms makes the
    /// kernel answer the peer's still-unacked data with a RST and
    /// discard any A/V messages the peer hasn't yet drained from its
    /// receive buffer — closeStream / the StreamEOF event / the last
    /// frames just written can be thrown away mid-stream. A write-half
    /// FIN lets the peer read everything we just wrote, then observe
    /// EOF cleanly. The read half closes when `self` (and its owned
    /// `TcpStream`) drops at end of scope.
    pub fn close(mut self) -> Result<()> {
        let _ = self.writer.write_message(
            CSID_PROTOCOL_CONTROL,
            &build_user_control_stream_eof(self.stream_id),
        );
        let _ = self.writer.write_message(
            CSID_COMMAND,
            &build_on_status(
                self.stream_id,
                "status",
                STATUS_UNPUBLISH_SUCCESS,
                "Stream closed.",
            ),
        );
        let _ = self.writer.flush();
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

    /// Read the next audio / video / metadata packet from the
    /// publisher. Returns `Ok(None)` when the peer cleanly closed the
    /// stream (via `closeStream` / `deleteStream` / `FCUnpublish`).
    ///
    /// Aggregate Messages (RTMP 1.0 §7.1.6, message type id `22`) are
    /// decomposed transparently: the sub-messages enter an internal
    /// queue and the dispatch loop drains them in publish order ahead
    /// of any further wire read, so a publisher that bundles several
    /// frames into one aggregate (fewer chunk headers on the wire)
    /// surfaces the same per-frame `StreamPacket` sequence as a
    /// publisher that sends them individually.
    pub fn next_packet(&mut self) -> Result<Option<StreamPacket>> {
        while !self.ended {
            // Drain queued aggregate sub-messages ahead of any further
            // wire read so the publisher's pack order is preserved.
            if let Some(sub) = self.pending_subs.pop_front() {
                if let Some(pkt) = self.handle_message(sub)? {
                    return Ok(Some(pkt));
                }
                continue;
            }
            let msg = match self.reader.read_message() {
                Ok(m) => m,
                Err(Error::Io(e))
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::UnexpectedEof | std::io::ErrorKind::ConnectionReset
                    ) =>
                {
                    return Ok(None);
                }
                Err(e) => return Err(e),
            };
            // §5.3: once the publisher has sent a full window of bytes,
            // owe it an Acknowledgement carrying the running sequence
            // number. Send before dispatching so the ack reflects the
            // bytes through this message.
            self.maybe_send_ack()?;
            if let Some(pkt) = self.handle_message(msg)? {
                return Ok(Some(pkt));
            }
        }
        Ok(None)
    }

    /// Emit a §5.3 Acknowledgement if the reader's received-byte count
    /// has crossed the peer-negotiated §5.5 window since the last one.
    /// No-op until a window has been negotiated (`Window Acknowledgement
    /// Size` / `Set Peer Bandwidth` from the publisher).
    fn maybe_send_ack(&mut self) -> Result<()> {
        if let Some(seq) = self.reader.ack_due() {
            self.writer
                .write_message(CSID_PROTOCOL_CONTROL, &build_ack(seq))?;
            self.writer.flush()?;
        }
        Ok(())
    }

    /// Per-message dispatch shared between the wire path and the
    /// aggregate-sub-drain path. Returns `Ok(Some(packet))` if the
    /// message produced a user-visible event, `Ok(None)` if it was
    /// consumed silently (protocol control, command teardown setting
    /// `self.ended`, etc.) and the loop should keep reading.
    fn handle_message(&mut self, msg: Message) -> Result<Option<StreamPacket>> {
        match msg.msg_type_id {
            MSG_AUDIO => {
                let tag = parse_audio(&msg.payload)?;
                Ok(Some(StreamPacket::Audio {
                    timestamp: msg.timestamp,
                    tag,
                }))
            }
            MSG_VIDEO => {
                let tag = parse_video(&msg.payload)?;
                Ok(Some(StreamPacket::Video {
                    timestamp: msg.timestamp,
                    tag,
                }))
            }
            MSG_DATA_AMF0 => {
                // @setDataFrame + onMetaData + <object>
                let values = amf::decode_all(&msg.payload)?;
                // Common shape: ["@setDataFrame", "onMetaData",
                // <meta>]. Some clients omit "@setDataFrame" and
                // just send ["onMetaData", <meta>]. Accept both.
                Ok(metadata_object(&values).map(StreamPacket::Metadata))
            }
            MSG_DATA_AMF3 => {
                // AMF3-encoded data message (type 15). Per the
                // Enhanced RTMP v2 clarification the body starts with
                // a format selector byte (0 = AMF0 values, each AMF3
                // value introduced by the 0x11 avmplus marker); legacy
                // selector-less frames are still accepted. Every value
                // bridges onto the AMF0 shape so metadata flows
                // through the same path as MSG_DATA_AMF0.
                let values = amf3::decode_message_to_amf0(&msg.payload)?;
                Ok(metadata_object(&values).map(StreamPacket::Metadata))
            }
            MSG_COMMAND_AMF0 => {
                let values = amf::decode_all(&msg.payload)?;
                self.handle_command_values(&values)
            }
            MSG_COMMAND_AMF3 => {
                // AMF3-encoded command (type 17). Bridge each value onto
                // the AMF0 shape so the same dispatch handles teardown
                // detection and §4.2 NetStream control commands.
                let values = amf3::decode_message_to_amf0(&msg.payload)?;
                self.handle_command_values(&values)
            }
            MSG_AGGREGATE => {
                // RTMP 1.0 §7.1.6 Aggregate Message. Split into
                // FLV-shaped sub-messages with the §7.1.6 timestamp
                // re-normalisation applied and the message-stream-id
                // override resolved; queue them so subsequent calls
                // surface the per-sub packets in publish order. Sub
                // ordering is preserved verbatim. A nested aggregate
                // (sub `msg_type_id == 22`) is forwarded to the queue
                // and the next dispatch tick recurses through the same
                // `MSG_AGGREGATE` arm so a bounded depth of nesting
                // resolves transparently; an unbounded chain would
                // surface as repeated parser work, not stack growth.
                let subs = parse_aggregate(&msg)?;
                self.pending_subs.extend(subs);
                Ok(None)
            }
            MSG_SET_CHUNK_SIZE => {
                let size = read_u32_be(&msg.payload)? & 0x7FFF_FFFF;
                self.reader.set_chunk_size(size as usize);
                Ok(None)
            }
            MSG_WINDOW_ACK_SIZE => {
                // §5.5: the peer is telling us which window size to use
                // when sending Acknowledgements. Honour it so our §5.3
                // ack cadence matches what the publisher expects.
                let size = read_u32_be(&msg.payload)?;
                self.reader.set_window_ack_size(size);
                Ok(None)
            }
            MSG_SET_PEER_BANDWIDTH => {
                // §5.6: "The output bandwidth value is the same as the
                // window size for the peer." The first 4 bytes carry
                // that window size; adopt it as our send-side ack
                // window too. (The trailing Limit type byte is
                // advisory and doesn't change our framing.)
                if msg.payload.len() >= 4 {
                    let size = read_u32_be(&msg.payload[..4])?;
                    self.reader.set_window_ack_size(size);
                }
                Ok(None)
            }
            MSG_ACK | MSG_USER_CONTROL => {
                // Informational — the peer's §5.3 sequence number (ACK)
                // or a user-control event we don't surface as a packet.
                Ok(None)
            }
            _ => {
                // Unknown / unhandled — swallow and keep going.
                Ok(None)
            }
        }
    }

    /// Dispatch a decoded command frame (shared between the AMF0 and
    /// AMF3 command arms). Teardown commands (`closeStream` /
    /// `deleteStream` / `FCUnpublish`) end the session and produce no
    /// packet; a recognised §4.2 NetStream control command
    /// (`play` / `play2` / `pause` / `seek` / `receiveAudio` /
    /// `receiveVideo`) surfaces as [`StreamPacket::Command`]; anything
    /// else is consumed silently.
    fn handle_command_values(&mut self, values: &[Amf0Value]) -> Result<Option<StreamPacket>> {
        if let Some(name) = values.first().and_then(Amf0Value::as_str) {
            if matches!(
                name,
                "closeStream" | "deleteStream" | "FCUnpublish" | "close"
            ) {
                // §4.2.3 deleteStream / closeStream and the §7.2.1
                // NetConnection `close` all end the session; none get
                // a response.
                self.ended = true;
                return Ok(None);
            }
            if let Some(cmd) = NetStreamCommand::parse(values)? {
                return Ok(Some(StreamPacket::Command(cmd)));
            }
            if matches!(name, "_result" | "_error") {
                // §7.2.1.2 response to a server-initiated RPC
                // ([`send_call`](Self::send_call)).
                let transaction_id = values.get(1).and_then(Amf0Value::as_f64).unwrap_or(0.0);
                return Ok(Some(StreamPacket::CallReply {
                    success: name == "_result",
                    transaction_id,
                    values: values.to_vec(),
                }));
            }
            if !is_reserved_command_name(name) {
                // §7.2.1.2: an RPC's procedure name rides the
                // command-name field, so any non-built-in command is a
                // `call` aimed at the application.
                return Ok(CallCommand::parse(values).map(StreamPacket::Call));
            }
            return Ok(None);
        }
        Ok(NetStreamCommand::parse(values)?.map(StreamPacket::Command))
    }

    /// Issue a §7.2.1.2 NetConnection `call` RPC *to the publisher* —
    /// either peer may run RPCs at the other's end. Choose a non-zero
    /// `transaction_id` to request a response (the peer's `_result` /
    /// `_error` surfaces as [`StreamPacket::CallReply`]); pass 0 for
    /// fire-and-forget.
    pub fn send_call(&mut self, call: &CallCommand) -> Result<()> {
        self.writer
            .write_message(CSID_COMMAND, &call.to_message())?;
        self.writer.flush()?;
        Ok(())
    }

    /// Answer a peer [`StreamPacket::Call`] whose
    /// [`CallCommand::expects_response`] with the §7.2.1.2 response
    /// structure: `_result(transaction_id, command_object, response)`.
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
    /// [`reply_call_result`](Self::reply_call_result) — `_error` with
    /// the same §7.2.1.2 response structure.
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
}

// ---------------------------------------------------------------------------
// Protocol driver: handshake → connect → createStream → publish | play
// ---------------------------------------------------------------------------

fn drive_until_request(
    stream: TcpStream,
    peer_addr: SocketAddr,
    server_caps: &ConnectCapabilities,
) -> Result<SessionRequest> {
    // TCP-level defaults: nodelay (RTMP is command-heavy during setup),
    // keepalive so idle publishers are detected.
    let _ = stream.set_nodelay(true);

    // Run the handshake on a plain clone of the stream (no chunk state
    // yet).
    let mut hs_stream = stream.try_clone()?;
    crate::handshake::server_handshake(&mut hs_stream)?;

    // Reader / writer share the same TCP stream via `try_clone`.
    let reader_stream = stream.try_clone()?;
    let writer_stream = stream.try_clone()?;
    let mut reader = ChunkReader::new(reader_stream);
    let mut writer = ChunkWriter::new(writer_stream);

    // Wait for connect. These get populated when we see the
    // `connect` command below.
    let tc_url;
    let app;
    let client_capabilities;
    loop {
        let msg = reader.read_message()?;
        match msg.msg_type_id {
            MSG_SET_CHUNK_SIZE => {
                let size = read_u32_be(&msg.payload)? & 0x7FFF_FFFF;
                reader.set_chunk_size(size as usize);
            }
            MSG_WINDOW_ACK_SIZE => {
                let size = read_u32_be(&msg.payload)?;
                reader.set_window_ack_size(size);
            }
            MSG_SET_PEER_BANDWIDTH if msg.payload.len() >= 4 => {
                let size = read_u32_be(&msg.payload[..4])?;
                reader.set_window_ack_size(size);
            }
            MSG_COMMAND_AMF0 | MSG_COMMAND_AMF3 => {
                // An objectEncoding-3 peer may issue its commands as
                // type-17 (AMF3) messages — Enhanced RTMP v2 requires
                // servers to accept them. `decode_message_to_amf0`
                // handles both the v2 format-selector framing and the
                // legacy AMF3 shapes, bridging onto the same AMF0
                // command values. Replies stay AMF0 (type 20): AMF0
                // support is mandatory for every peer, and only
                // format 0 is defined for the AMF3 message types.
                let values = if msg.msg_type_id == MSG_COMMAND_AMF3 {
                    amf3::decode_message_to_amf0(&msg.payload)?
                } else {
                    amf::decode_all(&msg.payload)?
                };
                let name = values
                    .first()
                    .and_then(Amf0Value::as_str)
                    .ok_or_else(|| Error::InvalidCommand("missing command name".into()))?;
                if name != "connect" {
                    return Err(Error::InvalidCommand(format!(
                        "expected `connect` first, got `{name}`"
                    )));
                }
                let tx_id = values.get(1).and_then(Amf0Value::as_f64).unwrap_or(1.0);
                let cmd_obj = values.get(2).ok_or_else(|| {
                    Error::InvalidCommand("`connect` missing command object".into())
                })?;
                tc_url = cmd_obj
                    .get("tcUrl")
                    .and_then(Amf0Value::as_str)
                    .unwrap_or("")
                    .to_owned();
                app = cmd_obj
                    .get("app")
                    .and_then(Amf0Value::as_str)
                    .unwrap_or("")
                    .to_owned();
                // Lift Enhanced RTMP v1+v2 capability advertisement out
                // of the Command Object. Legacy publishers leave this
                // empty.
                client_capabilities = ConnectCapabilities::from_amf0(cmd_obj);

                // Reply: WindowAckSize + SetPeerBandwidth + StreamBegin
                // + _result + SetChunkSize. Order matches what most
                // commodity ingest servers send. The server's own
                // capability advertisement rides inside the _result
                // info object — see `build_connect_result_with_caps`.
                writer.write_message(
                    CSID_PROTOCOL_CONTROL,
                    &build_window_ack_size(WINDOW_ACK_SIZE),
                )?;
                writer.write_message(
                    CSID_PROTOCOL_CONTROL,
                    &build_set_peer_bandwidth(WINDOW_ACK_SIZE, PEER_BW_LIMIT_DYNAMIC),
                )?;
                writer.write_message(CSID_PROTOCOL_CONTROL, &build_user_control_stream_begin(0))?;
                writer.write_message(
                    CSID_COMMAND,
                    &build_connect_result_with_caps(tx_id, server_caps),
                )?;
                writer.write_message(
                    CSID_PROTOCOL_CONTROL,
                    &build_set_chunk_size(SERVER_CHUNK_SIZE),
                )?;
                writer.set_chunk_size(SERVER_CHUNK_SIZE as usize);
                writer.flush()?;
                break;
            }
            _ => {
                // Silently accept other pre-connect messages (usually
                // nothing but SetChunkSize).
            }
        }
    }

    // Handle releaseStream / FCPublish / createStream until the peer
    // announces its direction with publish (§4.2.6) or play (§4.2.1).
    let mut next_stream_id: u32 = 1;
    // §3.7 SetBufferLength: "this event is sent before the server
    // starts processing the stream" — capture it so a play consumer
    // can honour the subscriber's requested buffer depth.
    let mut buffer_length_ms: Option<u32> = None;
    loop {
        let msg = reader.read_message()?;
        match msg.msg_type_id {
            MSG_SET_CHUNK_SIZE => {
                let size = read_u32_be(&msg.payload)? & 0x7FFF_FFFF;
                reader.set_chunk_size(size as usize);
                continue;
            }
            MSG_WINDOW_ACK_SIZE => {
                let size = read_u32_be(&msg.payload)?;
                reader.set_window_ack_size(size);
                continue;
            }
            MSG_SET_PEER_BANDWIDTH if msg.payload.len() >= 4 => {
                let size = read_u32_be(&msg.payload[..4])?;
                reader.set_window_ack_size(size);
                continue;
            }
            MSG_USER_CONTROL => {
                if let Ok(UserControlEvent::SetBufferLength { buffer_ms, .. }) =
                    UserControlEvent::parse(&msg.payload)
                {
                    buffer_length_ms = Some(buffer_ms);
                }
                continue;
            }
            MSG_COMMAND_AMF0 | MSG_COMMAND_AMF3 => {
                // Same AMF3 acceptance as the connect loop above.
                let values = if msg.msg_type_id == MSG_COMMAND_AMF3 {
                    amf3::decode_message_to_amf0(&msg.payload)?
                } else {
                    amf::decode_all(&msg.payload)?
                };
                let name = values
                    .first()
                    .and_then(Amf0Value::as_str)
                    .ok_or_else(|| Error::InvalidCommand("missing command name".into()))?
                    .to_owned();
                let tx_id = values.get(1).and_then(Amf0Value::as_f64).unwrap_or(0.0);
                match name.as_str() {
                    "releaseStream" | "FCPublish" => {
                        // Many peers want a _result back; send a minimal
                        // one. Arg slot [3] is the stream name we can
                        // echo.
                        let payload = amf::encode_command(
                            "_result",
                            tx_id,
                            Amf0Value::Null,
                            &[Amf0Value::Undefined],
                        );
                        let reply = Message {
                            msg_type_id: MSG_COMMAND_AMF0,
                            msg_stream_id: 0,
                            timestamp: 0,
                            payload,
                        };
                        writer.write_message(CSID_COMMAND, &reply)?;
                        writer.flush()?;
                    }
                    "createStream" => {
                        let sid = next_stream_id;
                        next_stream_id += 1;
                        writer.write_message(
                            CSID_COMMAND,
                            &build_create_stream_result(tx_id, sid as f64),
                        )?;
                        writer.flush()?;
                    }
                    "publish" => {
                        // Args: [stream_name, publish_type].
                        let stream_name = values
                            .get(3)
                            .and_then(Amf0Value::as_str)
                            .ok_or_else(|| {
                                Error::InvalidCommand("publish missing stream_name".into())
                            })?
                            .to_owned();
                        let publish_type = values
                            .get(4)
                            .and_then(Amf0Value::as_str)
                            .unwrap_or("live")
                            .to_owned();
                        return Ok(SessionRequest::Publish(PublishRequest {
                            app,
                            stream_name,
                            publish_type,
                            peer_addr,
                            tc_url,
                            capabilities: client_capabilities,
                            pending: PendingSession {
                                stream,
                                reader,
                                writer,
                                stream_id: msg.msg_stream_id.max(1),
                                publish_tx_id: tx_id,
                            },
                        }));
                    }
                    "play" => {
                        // §4.2.1: [stream_name, start?, duration?,
                        // reset?] after the Null Command Object.
                        // NetStreamCommand::parse implements exactly
                        // that argument table.
                        let cmd = NetStreamCommand::parse(&values)?;
                        let Some(NetStreamCommand::Play {
                            stream_name,
                            start,
                            duration,
                            reset,
                        }) = cmd
                        else {
                            return Err(Error::InvalidCommand(
                                "`play` did not parse as a NetStream play command".into(),
                            ));
                        };
                        return Ok(SessionRequest::Play(PlayRequest {
                            app,
                            stream_name,
                            start,
                            duration,
                            reset,
                            buffer_length_ms,
                            peer_addr,
                            tc_url,
                            capabilities: client_capabilities,
                            pending: PendingSession {
                                stream,
                                reader,
                                writer,
                                stream_id: msg.msg_stream_id.max(1),
                                publish_tx_id: tx_id,
                            },
                        }));
                    }
                    _ => {
                        // Unknown command — keep listening.
                    }
                }
            }
            _ => {
                // Ignore audio / video / data / control messages
                // arriving before publish — not strictly legal but
                // seen in the wild.
            }
        }
    }
}

/// Pull the metadata object out of a decoded data-message value list.
///
/// `@setDataFrame("onMetaData", <meta>)` is the standard publish shape;
/// some clients omit the leading `@setDataFrame` and send just
/// `["onMetaData", <meta>]`. Either way the payload object is the last
/// Object / ECMA-array value in the list, so search from the back.
pub(crate) fn metadata_object(values: &[Amf0Value]) -> Option<Amf0Value> {
    values
        .iter()
        .rev()
        .find(|v| matches!(v, Amf0Value::Object(_) | Amf0Value::EcmaArray(_)))
        .cloned()
}

fn read_u32_be(buf: &[u8]) -> Result<u32> {
    if buf.len() < 4 {
        return Err(Error::ProtocolViolation("need 4 bytes for u32be".into()));
    }
    Ok(u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]))
}

/// Free TCP-level helper for `stream`-owner code to read pending
/// writes synchronously.
#[allow(dead_code)]
fn flush_writer<W: Write>(w: &mut W) -> Result<()> {
    w.flush()?;
    Ok(())
}

#[allow(dead_code)]
fn read_exact<R: Read>(r: &mut R, n: usize) -> Result<Vec<u8>> {
    let mut buf = vec![0u8; n];
    r.read_exact(&mut buf)?;
    Ok(buf)
}
