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

use std::io::{Read, Write};
use std::net::{Shutdown, TcpStream, ToSocketAddrs};
use std::time::Duration;

use crate::amf::{self, Amf0Value};
use crate::amf3;
use crate::chunk::{ChunkReader, ChunkWriter, Message};
use crate::error::{Error, Result};
use crate::flv::{self, AudioTag, VideoTag};
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
    /// The server emitted `onStatus(...)` carrying NetStream state.
    /// `level` is typically `"status"` / `"warning"` / `"error"`;
    /// `code` is e.g. `"NetStream.Publish.Start"` /
    /// `"NetStream.Unpublish.Success"` / `"NetStream.Publish.BadName"`.
    OnStatus {
        level: String,
        code: String,
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
    /// Any other server-originated message (ping, ack, set-chunk-size,
    /// bandwidth — most of which the client handles transparently
    /// inside [`RtmpClient::poll_event`] before this variant ever fires).
    /// The variant exists so the caller's `match` arm can keep going.
    Other,
}

const CLIENT_CHUNK_SIZE: u32 = 4096;
const FLASH_VER: &str = "FMLE/3.0 (compatible; oxideav-rtmp)";

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
        Self::connect_parsed(&parsed, "live")
    }

    /// Same as [`connect`](Self::connect) but lets the caller pick the
    /// RTMP `publish` type (typically `"live"`, `"record"`, or
    /// `"append"`).
    pub fn connect_with_type(url: &str, publish_type: &str) -> Result<Self> {
        let parsed = RtmpUrl::parse(url)?;
        Self::connect_parsed(&parsed, publish_type)
    }

    fn connect_parsed(u: &RtmpUrl, publish_type: &str) -> Result<Self> {
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
        crate::handshake::client_handshake(&mut hs)?;

        let mut reader = ChunkReader::new(stream.try_clone()?);
        let mut writer = ChunkWriter::new(stream.try_clone()?);

        // We bump chunk size immediately — FFmpeg / OBS all do this
        // too. Saves a bunch of chunk headers over the A/V path.
        writer.write_message(
            CSID_PROTOCOL_CONTROL,
            &build_set_chunk_size(CLIENT_CHUNK_SIZE),
        )?;
        writer.set_chunk_size(CLIENT_CHUNK_SIZE as usize);

        // Send connect.
        let tx = 1.0;
        writer.write_message(
            CSID_COMMAND,
            &build_connect(tx, &u.app, &u.tc_url, FLASH_VER),
        )?;
        writer.flush()?;

        // Drain until we see the _result for connect.
        wait_for_result(&mut reader, &mut writer, tx)?;

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

    /// Send `onMetaData` as an AMF3-encoded data message (RTMP message
    /// type 15) instead of the AMF0 default.
    ///
    /// The body is framed per AMF 3 spec §4.1 / AMF 0 spec §3.1: the
    /// outer NetConnection message structure is AMF0, and each value
    /// switches to AMF3 by prefixing it with the `avmplus-object-marker`
    /// (`0x11`). Most ingest endpoints stay on AMF0, so prefer
    /// [`send_metadata`](Self::send_metadata); this exists for peers that
    /// negotiated an AMF3 channel.
    pub fn send_metadata_amf3(&mut self, metadata: amf3::Amf3Value) -> Result<()> {
        let mut payload = Vec::new();
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
        if self.read_eof {
            return Ok(None);
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
        match msg.msg_type_id {
            MSG_SET_CHUNK_SIZE => {
                let size = read_u32_be(&msg.payload)? & 0x7FFF_FFFF;
                self.reader.set_chunk_size(size as usize);
                Ok(Some(ClientEvent::Other))
            }
            MSG_ACK | MSG_WINDOW_ACK_SIZE | MSG_SET_PEER_BANDWIDTH => {
                // Informational. Spec mandates an ack reply once we
                // exceed the negotiated window, but for a publish-only
                // client of typical bitrate we leave that as a future
                // refinement — the server's own ack window resets per
                // session.
                Ok(Some(ClientEvent::Other))
            }
            MSG_USER_CONTROL => {
                let (event_type, event_data) = parse_user_control(&msg.payload)?;
                match event_type {
                    USR_STREAM_BEGIN => {
                        let sid = ucm_stream_id(event_data)?;
                        Ok(Some(ClientEvent::StreamBegin { stream_id: sid }))
                    }
                    USR_STREAM_EOF => {
                        let sid = ucm_stream_id(event_data)?;
                        // Don't latch here: the server typically sends
                        // a trailing onStatus / Unpublish.Success after
                        // StreamEOF, then half-closes; we let the
                        // subsequent read drain those and report EOF
                        // naturally.
                        Ok(Some(ClientEvent::StreamEof { stream_id: sid }))
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
                        Ok(Some(ClientEvent::Other))
                    }
                    _ => {
                        // StreamDry / SetBufferLength / StreamIsRecorded /
                        // PingResponse — surface as Other; the publisher
                        // doesn't act on them.
                        Ok(Some(ClientEvent::Other))
                    }
                }
            }
            MSG_COMMAND_AMF0 => {
                let values = amf::decode_all(&msg.payload)?;
                Ok(Some(classify_command(values)))
            }
            MSG_COMMAND_AMF3 => {
                let values: Vec<Amf0Value> = amf3::decode_data_message(&msg.payload)?
                    .iter()
                    .map(amf3::Amf3Value::to_amf0)
                    .collect();
                Ok(Some(classify_command(values)))
            }
            _ => Ok(Some(ClientEvent::Other)),
        }
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
        Ok(())
    }

    pub fn inner_mut(&mut self) -> &mut TcpStream {
        &mut self.stream
    }
}

/// Consume messages from `reader` until we see a command named
/// `_result` for `expected_tx`. Forward relevant protocol-control
/// updates (SetChunkSize) to the reader.
fn wait_for_result<R: Read, W: Write>(
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

fn wait_for_create_stream_result<R: Read, W: Write>(
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

fn read_u32_be(buf: &[u8]) -> Result<u32> {
    if buf.len() < 4 {
        return Err(Error::ProtocolViolation("need 4 bytes for u32be".into()));
    }
    Ok(u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]))
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
        _ => ClientEvent::Other,
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
