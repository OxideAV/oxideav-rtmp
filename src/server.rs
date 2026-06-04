//! RTMP server: accepts an incoming publisher.
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
    pub fn accept(&self) -> Result<PublishRequest> {
        loop {
            let (stream, peer_addr) = self.listener.accept()?;
            // Individual parse failures shouldn't bring down the
            // server — log via Err(...) once, then keep listening. A
            // caller that wants fine-grained control uses `incoming()`
            // plus their own handshake.
            match drive_until_publish(stream, peer_addr, &self.capabilities) {
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
    pub fn serve<F>(&self, handler: F) -> Result<()>
    where
        F: Fn(PublishRequest) + Send + Sync + 'static,
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
                .spawn(move || match drive_until_publish(stream, peer_addr, &c) {
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
                "NetStream.Publish.Start",
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
            &build_on_status(stream_id, "error", "NetStream.Publish.BadName", reason),
        );
        let _ = writer.flush();
        let _ = stream.shutdown(Shutdown::Both);
        Err(Error::Rejected(reason.to_string()))
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
    pub fn set_read_timeout(&self, d: Option<Duration>) -> Result<()> {
        self.stream.set_read_timeout(d)?;
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
                "NetStream.Unpublish.Success",
                "Stream closed.",
            ),
        );
        let _ = self.writer.flush();
        let _ = self.stream.shutdown(Shutdown::Write);
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
            if let Some(pkt) = self.handle_message(msg)? {
                return Ok(Some(pkt));
            }
        }
        Ok(None)
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
                // AMF3-encoded data message (type 15). Per AMF3 §4.1
                // the body is an AMF0 frame switching to AMF3 via the
                // avmplus marker; decode it and bridge each value onto
                // the AMF0 shape so metadata flows through the same
                // path as MSG_DATA_AMF0.
                let values: Vec<Amf0Value> = amf3::decode_data_message(&msg.payload)?
                    .iter()
                    .map(amf3::Amf3Value::to_amf0)
                    .collect();
                Ok(metadata_object(&values).map(StreamPacket::Metadata))
            }
            MSG_COMMAND_AMF0 => {
                // Likely closeStream / deleteStream /
                // FCUnpublish — peer is shutting down.
                let values = amf::decode_all(&msg.payload)?;
                if let Some(name) = values.first().and_then(Amf0Value::as_str) {
                    if matches!(name, "closeStream" | "deleteStream" | "FCUnpublish") {
                        self.ended = true;
                    }
                }
                Ok(None)
            }
            MSG_COMMAND_AMF3 => {
                // AMF3-encoded command (type 17). Same teardown
                // detection as the AMF0 command path.
                let values: Vec<Amf0Value> = amf3::decode_data_message(&msg.payload)?
                    .iter()
                    .map(amf3::Amf3Value::to_amf0)
                    .collect();
                if let Some(name) = values.first().and_then(Amf0Value::as_str) {
                    if matches!(name, "closeStream" | "deleteStream" | "FCUnpublish") {
                        self.ended = true;
                    }
                }
                Ok(None)
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
            MSG_ACK | MSG_USER_CONTROL | MSG_WINDOW_ACK_SIZE | MSG_SET_PEER_BANDWIDTH => {
                // Informational — silently accept.
                Ok(None)
            }
            _ => {
                // Unknown / unhandled — swallow and keep going.
                Ok(None)
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Protocol driver: handshake → connect → createStream → publish
// ---------------------------------------------------------------------------

fn drive_until_publish(
    stream: TcpStream,
    peer_addr: SocketAddr,
    server_caps: &ConnectCapabilities,
) -> Result<PublishRequest> {
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
            MSG_COMMAND_AMF0 => {
                let values = amf::decode_all(&msg.payload)?;
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

    // Handle releaseStream / FCPublish / createStream / publish until
    // we see publish.
    let mut next_stream_id: u32 = 1;
    loop {
        let msg = reader.read_message()?;
        match msg.msg_type_id {
            MSG_SET_CHUNK_SIZE => {
                let size = read_u32_be(&msg.payload)? & 0x7FFF_FFFF;
                reader.set_chunk_size(size as usize);
                continue;
            }
            MSG_COMMAND_AMF0 => {
                let values = amf::decode_all(&msg.payload)?;
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
                        return Ok(PublishRequest {
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
                        });
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
fn metadata_object(values: &[Amf0Value]) -> Option<Amf0Value> {
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
