//! RTMP play client: pull (subscribe to) a stream from a remote RTMP
//! server — the inverse of [`RtmpClient`](crate::RtmpClient)'s publish
//! direction.
//!
//! ```text
//!   let mut player = RtmpPlayer::connect("rtmp://remote/live/key")?;
//!   while let Some(pkt) = player.next_packet()? {
//!       match pkt {
//!           PlayerPacket::Video { timestamp, tag } => { /* AVC bytes */ }
//!           PlayerPacket::Audio { timestamp, tag } => { /* AAC bytes */ }
//!           PlayerPacket::Metadata(meta)          => { /* onMetaData */ }
//!           PlayerPacket::Status { code, .. }     => { /* NetStream.* */ }
//!           PlayerPacket::Control(_)              => { /* UCM events */ }
//!       }
//!   }
//! ```
//!
//! The setup sequence follows RTMP 1.0 Commands-Messages §4.2.1
//! Figure 5: handshake → `connect` → `createStream` → (optional §3.7
//! `SetBufferLength`) → `play`, then the connect driver waits for the
//! server's `onStatus(NetStream.Play.Start)` before handing control
//! to [`RtmpPlayer::next_packet`]. Mid-stream the player can `pause` /
//! `resume` (§4.2.8), `seek` (§4.2.7), and toggle `receiveAudio` /
//! `receiveVideo` (§4.2.4 / §4.2.5); the server's status replies
//! (`NetStream.Pause.Notify`, `NetStream.Seek.Notify`, …) surface as
//! [`PlayerPacket::Status`] values.

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::{Shutdown, TcpStream, ToSocketAddrs};
use std::time::Duration;

use crate::aggregate::parse_aggregate;
use crate::amf::{self, Amf0Value};
use crate::amf3;
use crate::caps::ConnectCapabilities;
use crate::chunk::{ChunkReader, ChunkWriter, Message};
use crate::client::{
    extract_server_caps, read_u32_be, wait_for_create_stream_result, RtmpUrl, CLIENT_CHUNK_SIZE,
    FLASH_VER,
};
use crate::error::{Error, Result};
use crate::flv::{parse_audio, parse_video, AudioTag, VideoTag};
use crate::message::*;
use crate::server::metadata_object;

/// Options for [`RtmpPlayer::connect_with_options`].
///
/// The three `play`-argument fields mirror the §4.2.1 optional
/// trailing arguments; `None` omits the argument from the wire and
/// lets the server apply the spec default (Start −2, Duration −1,
/// Reset false).
#[derive(Debug, Clone, Default)]
pub struct PlayOptions {
    /// §4.2.1 Start argument in seconds: −2 (default) = try the live
    /// stream first, then the recorded one; −1 = live only; ≥ 0 =
    /// play the recorded stream from that offset.
    pub start: Option<f64>,
    /// §4.2.1 Duration argument in seconds (−1 = until the stream
    /// ends).
    pub duration: Option<f64>,
    /// §4.2.1 Reset flag — flush any queued playlist first.
    pub reset: Option<bool>,
    /// §3.7 `SetBufferLength` in milliseconds, sent on the freshly
    /// created stream *before* the `play` command per spec ("this
    /// event is sent before the server starts processing the
    /// stream"). `None` skips the event.
    pub buffer_length_ms: Option<u32>,
    /// Enhanced RTMP v1+v2 capability block to advertise in the
    /// `connect` Command Object. Default = empty (legacy byte layout).
    pub capabilities: ConnectCapabilities,
}

/// One event delivered to a playing client by
/// [`RtmpPlayer::next_packet`].
#[derive(Debug, Clone)]
pub enum PlayerPacket {
    /// An audio message on the play stream, parsed through
    /// [`crate::flv::parse_audio`].
    Audio { timestamp: u32, tag: AudioTag },
    /// A video message on the play stream, parsed through
    /// [`crate::flv::parse_video`].
    Video { timestamp: u32, tag: VideoTag },
    /// An `onMetaData` data message (AMF0 or AMF3, bridged to AMF0).
    Metadata(Amf0Value),
    /// An `onStatus` command from the server — e.g.
    /// `NetStream.Play.Reset`, `NetStream.Pause.Notify`,
    /// `NetStream.Seek.Notify`, `NetStream.Play.StreamNotFound`.
    Status {
        level: String,
        code: String,
        description: String,
    },
    /// A §3.7 User Control event that isn't consumed internally —
    /// `StreamBegin`, `StreamDry`, `StreamIsRecorded`,
    /// `PingResponse`, or a reserved / future event type.
    /// (`PingRequest` is auto-replied; `StreamEOF` ends the stream and
    /// surfaces as `Ok(None)` instead.)
    Control(UserControlEvent),
}

/// RTMP play (subscribe) client. See the [module docs](self) for the
/// protocol flow.
pub struct RtmpPlayer {
    stream: TcpStream,
    reader: ChunkReader<TcpStream>,
    writer: ChunkWriter<TcpStream>,
    stream_id: u32,
    tc_url: String,
    stream_name: String,
    server_caps: ConnectCapabilities,
    /// Latched once the server signals end-of-stream (`UserControl
    /// StreamEOF` per §7.1.7, or TCP EOF).
    ended: bool,
    /// True once the server has announced `StreamIsRecorded` (§3.7
    /// UCM 4) for the play stream.
    is_recorded: bool,
    /// Sub-messages decomposed out of a server-side Aggregate Message
    /// (RTMP 1.0 §7.1.6) awaiting dispatch.
    pending_subs: VecDeque<Message>,
}

impl RtmpPlayer {
    /// Dial `rtmp://host[:port]/app/stream_name`, run the full
    /// handshake + connect + createStream + play sequence with spec
    /// defaults, and block until the server's
    /// `onStatus(NetStream.Play.Start)` arrives.
    pub fn connect(url: &str) -> Result<Self> {
        Self::connect_with_options(url, &PlayOptions::default())
    }

    /// Same as [`connect`](Self::connect) with explicit §4.2.1 play
    /// arguments, an optional §3.7 `SetBufferLength`, and an Enhanced
    /// RTMP capability advertisement.
    pub fn connect_with_options(url: &str, opts: &PlayOptions) -> Result<Self> {
        let u = RtmpUrl::parse(url)?;
        let sock_addr = (u.host.as_str(), u.port)
            .to_socket_addrs()
            .map_err(Error::from)?
            .next()
            .ok_or_else(|| Error::Other(format!("resolved no addresses for {}", u.host)))?;
        let stream = TcpStream::connect_timeout(&sock_addr, Duration::from_secs(15))?;
        let _ = stream.set_nodelay(true);

        let mut hs = stream.try_clone()?;
        crate::handshake::client_handshake(&mut hs)?;

        let mut reader = ChunkReader::new(stream.try_clone()?);
        let mut writer = ChunkWriter::new(stream.try_clone()?);

        writer.write_message(
            CSID_PROTOCOL_CONTROL,
            &build_set_chunk_size(CLIENT_CHUNK_SIZE),
        )?;
        writer.set_chunk_size(CLIENT_CHUNK_SIZE as usize);

        // connect → _result (capturing the server's capability block).
        let tx = 1.0;
        writer.write_message(
            CSID_COMMAND,
            &build_connect_with_caps(tx, &u.app, &u.tc_url, FLASH_VER, &opts.capabilities),
        )?;
        writer.flush()?;
        let connect_result = crate::client::wait_for_result(&mut reader, &mut writer, tx)?;
        let server_caps = extract_server_caps(&connect_result);

        // createStream → stream id. (No releaseStream / FCPublish —
        // those are publish-direction advisories.)
        let tx_cs = 2.0;
        writer.write_message(CSID_COMMAND, &build_create_stream(tx_cs))?;
        writer.flush()?;
        let stream_id = wait_for_create_stream_result(&mut reader, &mut writer, tx_cs)?;

        // §3.7: SetBufferLength is sent before the server starts
        // processing the stream.
        if let Some(ms) = opts.buffer_length_ms {
            writer.write_message(
                CSID_PROTOCOL_CONTROL,
                &build_user_control_set_buffer_length(stream_id, ms),
            )?;
        }

        // play (§4.2.1).
        let play = NetStreamCommand::Play {
            stream_name: u.stream_name.clone(),
            start: opts.start,
            duration: opts.duration,
            reset: opts.reset,
        };
        writer.write_message(CSID_COMMAND, &play.to_message(stream_id))?;
        writer.flush()?;

        // Figure 5 tail: SetChunkSize + StreamIsRecorded? + StreamBegin
        // + onStatus(Play.Reset)? + onStatus(Play.Start).
        let is_recorded = wait_for_play_start(&mut reader, &mut writer)?;

        Ok(Self {
            stream,
            reader,
            writer,
            stream_id,
            tc_url: u.tc_url.clone(),
            stream_name: u.stream_name,
            server_caps,
            ended: false,
            is_recorded,
            pending_subs: VecDeque::new(),
        })
    }

    /// NetStream message stream id the server allocated in
    /// `_result(createStream)`.
    pub fn stream_id(&self) -> u32 {
        self.stream_id
    }

    /// The `tcUrl` this player dialled (`rtmp://host[:port]/app`).
    pub fn tc_url(&self) -> &str {
        &self.tc_url
    }

    /// Stream name requested in the §4.2.1 `play` command.
    pub fn stream_name(&self) -> &str {
        &self.stream_name
    }

    /// Capability block the server advertised in `_result(connect)`.
    pub fn server_capabilities(&self) -> &ConnectCapabilities {
        &self.server_caps
    }

    /// True once the server has announced the stream as recorded
    /// (§3.7 `StreamIsRecorded`) — either during play setup or later.
    pub fn is_recorded(&self) -> bool {
        self.is_recorded
    }

    /// Apply a `recv` timeout to the socket clone
    /// [`next_packet`](Self::next_packet) blocks on.
    pub fn set_read_timeout(&mut self, d: Option<Duration>) -> Result<()> {
        self.reader.inner_mut().set_read_timeout(d)?;
        let _ = self.stream.set_read_timeout(d);
        Ok(())
    }

    /// Read the next audio / video / metadata / status event from the
    /// server.
    ///
    /// Protocol control (Set Chunk Size, Window Ack Size, Set Peer
    /// Bandwidth, Acknowledgement), §5.3 ack emission, `PingRequest`
    /// auto-reply, and §7.1.6 aggregate decomposition are all handled
    /// internally. Returns `Ok(None)` once the server ends playback —
    /// `UserControl StreamEOF` ("the playback of data is over as
    /// requested on this stream", §7.1.7) or TCP EOF — after which the
    /// player should be finished with [`close`](Self::close).
    pub fn next_packet(&mut self) -> Result<Option<PlayerPacket>> {
        while !self.ended {
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
                    self.ended = true;
                    return Ok(None);
                }
                Err(Error::UnexpectedEof) => {
                    self.ended = true;
                    return Ok(None);
                }
                Err(e) => return Err(e),
            };
            // §5.3: a playing client receives the bulk of the byte
            // flow, so the ack obligation matters here even more than
            // in the publish direction.
            self.maybe_send_ack()?;
            if let Some(pkt) = self.handle_message(msg)? {
                return Ok(Some(pkt));
            }
        }
        Ok(None)
    }

    fn handle_message(&mut self, msg: Message) -> Result<Option<PlayerPacket>> {
        match msg.msg_type_id {
            MSG_AUDIO => {
                let tag = parse_audio(&msg.payload)?;
                Ok(Some(PlayerPacket::Audio {
                    timestamp: msg.timestamp,
                    tag,
                }))
            }
            MSG_VIDEO => {
                let tag = parse_video(&msg.payload)?;
                Ok(Some(PlayerPacket::Video {
                    timestamp: msg.timestamp,
                    tag,
                }))
            }
            MSG_DATA_AMF0 => {
                let values = amf::decode_all(&msg.payload)?;
                Ok(metadata_object(&values).map(PlayerPacket::Metadata))
            }
            MSG_DATA_AMF3 => {
                let values = amf3::decode_message_to_amf0(&msg.payload)?;
                Ok(metadata_object(&values).map(PlayerPacket::Metadata))
            }
            MSG_COMMAND_AMF0 => {
                let values = amf::decode_all(&msg.payload)?;
                Ok(classify_status(&values))
            }
            MSG_COMMAND_AMF3 => {
                let values = amf3::decode_message_to_amf0(&msg.payload)?;
                Ok(classify_status(&values))
            }
            MSG_USER_CONTROL => match UserControlEvent::parse(&msg.payload)? {
                UserControlEvent::StreamEof { .. } => {
                    // §7.1.7: playback is over as requested. Latch and
                    // report the clean end.
                    self.ended = true;
                    Ok(None)
                }
                UserControlEvent::PingRequest { timestamp_ms } => {
                    self.writer.write_message(
                        CSID_PROTOCOL_CONTROL,
                        &build_user_control_ping_response(timestamp_ms),
                    )?;
                    self.writer.flush()?;
                    Ok(None)
                }
                ev @ UserControlEvent::StreamIsRecorded { .. } => {
                    self.is_recorded = true;
                    Ok(Some(PlayerPacket::Control(ev)))
                }
                ev => Ok(Some(PlayerPacket::Control(ev))),
            },
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

    fn maybe_send_ack(&mut self) -> Result<()> {
        if let Some(seq) = self.reader.ack_due() {
            self.writer
                .write_message(CSID_PROTOCOL_CONTROL, &build_ack(seq))?;
            self.writer.flush()?;
        }
        Ok(())
    }

    /// §4.2.8 `pause(true, ms)` — ask the server to pause playback at
    /// the given current stream time in milliseconds. The server
    /// replies with `onStatus(NetStream.Pause.Notify)` (surfaced as
    /// [`PlayerPacket::Status`]).
    pub fn pause(&mut self, milliseconds: f64) -> Result<()> {
        self.send_netstream(&NetStreamCommand::Pause {
            pause: true,
            milliseconds,
        })
    }

    /// §4.2.8 `pause(false, ms)` — resume playback. Per spec, "when
    /// the playback is resumed, the server will only send messages
    /// with timestamps greater than this value." The server replies
    /// with `onStatus(NetStream.Unpause.Notify)`.
    pub fn resume(&mut self, milliseconds: f64) -> Result<()> {
        self.send_netstream(&NetStreamCommand::Pause {
            pause: false,
            milliseconds,
        })
    }

    /// §4.2.7 `seek(ms)` — seek the given offset (in milliseconds)
    /// within a media file or playlist. On success the server replies
    /// with `onStatus(NetStream.Seek.Notify)`; on failure, `_error`.
    pub fn seek(&mut self, milliseconds: f64) -> Result<()> {
        self.send_netstream(&NetStreamCommand::Seek { milliseconds })
    }

    /// §4.2.4 `receiveAudio(flag)` — tell the server whether to send
    /// audio. Per spec the server stays silent for `false`; for `true`
    /// it replies with `NetStream.Seek.Notify` + `NetStream.Play.Start`.
    pub fn set_receive_audio(&mut self, flag: bool) -> Result<()> {
        self.send_netstream(&NetStreamCommand::ReceiveAudio(flag))
    }

    /// §4.2.5 `receiveVideo(flag)` — tell the server whether to send
    /// video. Same response contract as
    /// [`set_receive_audio`](Self::set_receive_audio).
    pub fn set_receive_video(&mut self, flag: bool) -> Result<()> {
        self.send_netstream(&NetStreamCommand::ReceiveVideo(flag))
    }

    /// Issue a further §4.2.1 `play` on the same NetStream — the
    /// spec's dynamic-playlist mechanism: "a playlist can also be
    /// created using this command multiple times. If you want to
    /// create a dynamic playlist that switches among different live
    /// or recorded streams, call play more than once and pass false
    /// for reset each time. Conversely, if you want to play the
    /// specified stream immediately, clearing any other streams that
    /// are queued for play, pass true for reset."
    ///
    /// Unlike the connect-time play this does not block waiting for a
    /// status reply; the server's `NetStream.Play.*` notifications
    /// surface through [`next_packet`](Self::next_packet) as
    /// [`PlayerPacket::Status`]. The player's
    /// [`stream_name`](Self::stream_name) is updated to the new name.
    pub fn play(
        &mut self,
        stream_name: &str,
        start: Option<f64>,
        duration: Option<f64>,
        reset: Option<bool>,
    ) -> Result<()> {
        self.send_netstream(&NetStreamCommand::Play {
            stream_name: stream_name.to_owned(),
            start,
            duration,
            reset,
        })?;
        self.stream_name = stream_name.to_owned();
        Ok(())
    }

    /// §4.2.2 `play2` — "unlike the play command, play2 can switch to
    /// a different bit rate stream without changing the timeline of
    /// the content played." The single AMF parameter object is passed
    /// through verbatim (typically carrying `streamName`, `start`,
    /// `len`, `offset`, and `transition` properties); the server's
    /// status replies surface through
    /// [`next_packet`](Self::next_packet).
    pub fn play2(&mut self, params: Amf0Value) -> Result<()> {
        self.send_netstream(&NetStreamCommand::Play2(params))
    }

    /// §3.7 `SetBufferLength` — (re-)announce the buffer depth in
    /// milliseconds this client keeps filled for the play stream. May
    /// be sent again mid-stream (e.g. after a pause).
    pub fn set_buffer_length(&mut self, buffer_ms: u32) -> Result<()> {
        self.writer.write_message(
            CSID_PROTOCOL_CONTROL,
            &build_user_control_set_buffer_length(self.stream_id, buffer_ms),
        )?;
        self.writer.flush()?;
        Ok(())
    }

    fn send_netstream(&mut self, cmd: &NetStreamCommand) -> Result<()> {
        self.writer
            .write_message(CSID_COMMAND, &cmd.to_message(self.stream_id))?;
        self.writer.flush()?;
        Ok(())
    }

    /// Tear the play stream down: send `deleteStream(stream_id)` —
    /// §4.2.3, "NetStream sends the deleteStream command when the
    /// NetStream object is getting destroyed"; the server does not
    /// send any response — then half-close the write side.
    pub fn close(mut self) -> Result<()> {
        let payload = amf::encode_command(
            "deleteStream",
            0.0,
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
}

/// Classify a decoded command frame into a [`PlayerPacket`]. Only
/// `onStatus` produces an event; `_result` / `_error` for the setup
/// transactions were consumed during connect, and any later ones are
/// swallowed (a §4.2 NetStream control command carries transaction id
/// 0 and expects onStatus replies, not `_result`).
fn classify_status(values: &[Amf0Value]) -> Option<PlayerPacket> {
    let name = values.first().and_then(Amf0Value::as_str)?;
    if name != "onStatus" {
        return None;
    }
    let info = values.get(3)?;
    let field = |key: &str| {
        info.get(key)
            .and_then(Amf0Value::as_str)
            .unwrap_or("")
            .to_owned()
    };
    Some(PlayerPacket::Status {
        level: field("level"),
        code: field("code"),
        description: field("description"),
    })
}

/// Drive the post-`play` Figure 5 tail until
/// `onStatus(NetStream.Play.Start)`. Returns whether the server
/// announced `StreamIsRecorded` along the way.
///
/// Per §4.2.1, a successful play yields `NetStream.Play.Start`
/// (preceded by `NetStream.Play.Reset` when the reset flag was set);
/// a missing stream yields `NetStream.Play.StreamNotFound`, and any
/// error-level status or `_error` reply refuses the play.
fn wait_for_play_start<R: Read, W: Write>(
    reader: &mut ChunkReader<R>,
    writer: &mut ChunkWriter<W>,
) -> Result<bool> {
    let mut is_recorded = false;
    for _ in 0..50 {
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
            MSG_USER_CONTROL => {
                if let Ok(ev) = UserControlEvent::parse(&msg.payload) {
                    match ev {
                        UserControlEvent::StreamIsRecorded { .. } => is_recorded = true,
                        UserControlEvent::PingRequest { timestamp_ms } => {
                            writer.write_message(
                                CSID_PROTOCOL_CONTROL,
                                &build_user_control_ping_response(timestamp_ms),
                            )?;
                            writer.flush()?;
                        }
                        _ => {}
                    }
                }
            }
            MSG_COMMAND_AMF0 => {
                let values = amf::decode_all(&msg.payload)?;
                let name = values.first().and_then(Amf0Value::as_str).unwrap_or("");
                match name {
                    "onStatus" => {
                        let info = values.get(3);
                        let get = |key: &str| {
                            info.and_then(|i| i.get(key))
                                .and_then(Amf0Value::as_str)
                                .unwrap_or("")
                                .to_owned()
                        };
                        let code = get("code");
                        let level = get("level");
                        if code == STATUS_PLAY_START {
                            return Ok(is_recorded);
                        }
                        if code == STATUS_PLAY_STREAM_NOT_FOUND || level == "error" {
                            return Err(Error::Rejected(format!(
                                "play refused: {code}: {}",
                                get("description")
                            )));
                        }
                        // NetStream.Play.Reset and any other
                        // status-level notification — keep waiting for
                        // Play.Start.
                    }
                    "_error" => {
                        return Err(Error::Other(format!(
                            "RTMP _error from server: {:?}",
                            values.get(3)
                        )));
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
    Err(Error::Timeout)
}
