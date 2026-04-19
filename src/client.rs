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
use crate::chunk::{ChunkReader, ChunkWriter, Message};
use crate::error::{Error, Result};
use crate::flv::{self, AudioTag, VideoTag};
use crate::message::*;

const CLIENT_CHUNK_SIZE: u32 = 4096;
const FLASH_VER: &str = "FMLE/3.0 (compatible; oxideav-rtmp)";

pub struct RtmpClient {
    stream: TcpStream,
    /// Kept around so `recv` helpers (ack, onStatus replies) have
    /// somewhere to drain the server's side. Not read on the normal
    /// publish-only path.
    #[allow(dead_code)]
    reader: ChunkReader<TcpStream>,
    writer: ChunkWriter<TcpStream>,
    stream_id: u32,
    /// Monotonic counter used for AMF command transaction ids.
    next_tx: f64,
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
        })
    }

    /// Send the AVC sequence header (`AVCDecoderConfigurationRecord`
    /// aka avcC). Must be called once before any NALU-carrying
    /// [`send_video`](Self::send_video).
    pub fn send_video_sequence_header(&mut self, avc_c: &[u8]) -> Result<()> {
        let tag = VideoTag {
            frame_type: flv::VIDEO_FRAME_KEYFRAME,
            codec_id: flv::VIDEO_CODEC_AVC,
            avc_packet_type: Some(flv::AVC_PACKET_TYPE_SEQUENCE_HEADER),
            composition_time: 0,
            body: avc_c.to_vec(),
        };
        self.send_video_tag(0, &tag)
    }

    /// Send one video access unit. `body` is the AVCC-formatted
    /// content (one or more `[u32 length BE][NALU bytes]` pairs).
    /// `is_keyframe` drives the FLV frame_type bits.
    pub fn send_video(&mut self, timestamp_ms: u32, is_keyframe: bool, body: &[u8]) -> Result<()> {
        let tag = VideoTag {
            frame_type: if is_keyframe {
                flv::VIDEO_FRAME_KEYFRAME
            } else {
                flv::VIDEO_FRAME_INTER
            },
            codec_id: flv::VIDEO_CODEC_AVC,
            avc_packet_type: Some(flv::AVC_PACKET_TYPE_NALU),
            composition_time: 0,
            body: body.to_vec(),
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
            sound_format: flv::AUDIO_FORMAT_AAC,
            sound_rate: 3,
            sound_size_16bit: true,
            stereo: true,
            aac_packet_type: Some(flv::AAC_PACKET_TYPE_SEQUENCE_HEADER),
            body: asc.to_vec(),
        };
        self.send_audio_tag(0, &tag)
    }

    /// Send one raw AAC frame.
    pub fn send_audio(&mut self, timestamp_ms: u32, aac_frame: &[u8]) -> Result<()> {
        let tag = AudioTag {
            sound_format: flv::AUDIO_FORMAT_AAC,
            sound_rate: 3,
            sound_size_16bit: true,
            stereo: true,
            aac_packet_type: Some(flv::AAC_PACKET_TYPE_RAW),
            body: aac_frame.to_vec(),
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
        let _ = self.stream.shutdown(Shutdown::Both);
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
