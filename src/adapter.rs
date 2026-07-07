//! `PacketSource` adapter wrapping an [`RtmpSession`].
//!
//! Bridges this crate's protocol-native [`StreamPacket`] (FLV-style
//! audio + video tags, plus `onMetaData` AMF0 objects) into the
//! workspace's [`oxideav_core::Packet`] shape so a
//! [`oxideav_core::registry::SourceRegistry`] can dispatch
//! `rtmp://` URIs into the standard pipeline executor.
//!
//! The adapter is **listen-style** by URL convention:
//!
//! ```text
//! rtmp://0.0.0.0:1935/live/secret-key
//!         ^^^^^^^^^^^^^ local TCP bind
//!                       ^^^^ ^^^^^^^^^^ app + stream-name the
//!                                       publisher must announce
//! ```
//!
//! The opener binds on the URL's `host:port`, accepts **one**
//! publisher, verifies that the announced `app` + `stream_name`
//! match the URL path (rejecting otherwise), then returns a
//! [`RtmpPacketSource`] the executor pumps via `next_packet()`.
//! The historical [`RtmpServer`](crate::RtmpServer) /
//! [`RtmpClient`](crate::RtmpClient) API is unchanged — this
//! adapter is purely additive.
//!
//! # Stream layout
//!
//! Always exactly two streams, both opened with TimeBase
//! 1/1_000_000_000 (nanoseconds). RTMP chunks carry a millisecond
//! `timestamp` and Enhanced-RTMP-v2 ModEx `TimestampOffsetNano`
//! entries add sub-millisecond precision; the adapter folds both
//! into the same nanosecond-resolution timeline so a downstream
//! consumer reads a single uniform clock:
//!
//! * **Stream 0 — audio.** Codec id derived lazily from the first
//!   audio tag the publisher sends (`aac` for AAC, `mp3` for MP3,
//!   etc; see [`audio_codec_id`]). If the publisher never sends
//!   audio the stream stays present but emits no packets.
//! * **Stream 1 — video.** Codec id from the first video tag
//!   (`h264` for AVC, `h263`, `vp6`, `vp6a`, screen-codec ids;
//!   see [`video_codec_id`]).
//!
//! The opener buffers up to [`PROBE_LIMIT`] packets after the
//! handshake completes so it can observe at least one of each
//! kind before returning. Buffered packets are replayed in order
//! by `next_packet()` before any new reads. If the publisher
//! disconnects during probing, whichever streams were observed
//! are reported.
//!
//! # Timestamps
//!
//! RTMP carries a single 32-bit `timestamp` per chunk, expressed
//! in milliseconds. Enhanced RTMP v2 lets a sender prepend a
//! `TimestampOffsetNano` ModEx entry (`enhanced-rtmp-v2.pdf`
//! §"ExVideoTagHeader" / §"ExAudioTagHeader") carrying a 0..=999_999
//! ns offset to be added to the *presentation* time of the current
//! media message **without altering the core RTMP timestamp**. The
//! adapter folds these per-message offsets into the nanosecond
//! [`Packet`] timeline:
//!
//! * Audio — `pts = dts = timestamp_ms * 1_000_000 + nano_offset`.
//! * Video AVC / NALU-FourCC — `dts = timestamp_ms * 1_000_000`
//!   (decode time, unmodified per spec); `pts = (timestamp_ms +
//!   composition_time_ms) * 1_000_000 + nano_offset`. The
//!   composition-time offset stays in milliseconds because that is
//!   the wire field; the nanosecond modifier rides on top.
//! * Non-NALU video without CTS — `pts == dts == timestamp_ms *
//!   1_000_000 + nano_offset`.
//!
//! Multiple `TimestampOffsetNano` entries on the same tag are
//! summed via [`VideoTag::timestamp_offset_nano`] /
//! [`AudioTag::timestamp_offset_nano`] before folding, matching
//! the per-tag accessor on those types.
//!
//! # Metadata variants
//!
//! `StreamPacket::Metadata(_)` carries an AMF0 `onMetaData`
//! object — useful to publishers but not a media packet. We
//! swallow it after recording its contents into
//! [`PacketSource::metadata`] (string-flattening any scalar
//! key/value pairs) and continue reading.

use std::collections::VecDeque;
use std::net::ToSocketAddrs;
use std::time::Duration;

use oxideav_core::{
    BytesSource, CodecId, CodecParameters, Error as CoreError, Packet, PacketSource,
    Result as CoreResult, SourceRegistry, StreamInfo, TimeBase,
};

use crate::amf::Amf0Value;
use crate::error::{Error as RtmpError, Result as RtmpResult};
use crate::flv::{
    self, AudioTag, VideoTag, AAC_PACKET_TYPE_SEQUENCE_HEADER, AUDIO_FORMAT_AAC,
    AUDIO_FORMAT_ADPCM, AUDIO_FORMAT_G711_ALAW, AUDIO_FORMAT_G711_MULAW, AUDIO_FORMAT_MP3,
    AUDIO_FORMAT_NELLYMOSER, AUDIO_FORMAT_NELLYMOSER_16K_MONO, AUDIO_FORMAT_NELLYMOSER_8K_MONO,
    AUDIO_FORMAT_PCM_LE, AUDIO_FORMAT_PCM_LE_8BIT, AUDIO_FORMAT_SPEEX, VIDEO_CODEC_AVC,
    VIDEO_CODEC_H263, VIDEO_CODEC_SCREEN, VIDEO_CODEC_SCREEN_V2, VIDEO_CODEC_VP6, VIDEO_CODEC_VP6A,
};
use crate::player::{PlayOptions, PlayerPacket, RtmpPlayer};
use crate::server::{RtmpServer, RtmpSession, StreamPacket};

/// Stream index for the audio output of an [`RtmpPacketSource`].
pub const AUDIO_STREAM_INDEX: u32 = 0;
/// Stream index for the video output of an [`RtmpPacketSource`].
pub const VIDEO_STREAM_INDEX: u32 = 1;
/// Time base used for both streams: 1/1_000_000_000 (nanoseconds).
///
/// RTMP chunks carry a 32-bit millisecond `timestamp` while
/// Enhanced RTMP v2 ModEx `TimestampOffsetNano` entries add
/// 0..=999_999 ns of sub-millisecond precision. A nanosecond
/// timeline lets [`audio_to_packet`] / [`video_to_packet`] fold
/// both into a single uniform `Packet::pts` / `Packet::dts`
/// without losing precision (per `enhanced-rtmp-v2.pdf`
/// §"ExVideoTagHeader" / §"ExAudioTagHeader").
pub const RTMP_TIME_BASE: TimeBase = TimeBase::new(1, 1_000_000_000);

/// Multiplier converting an RTMP millisecond timestamp into the
/// nanosecond [`RTMP_TIME_BASE`] timeline.
pub const RTMP_MS_TO_NS: i64 = 1_000_000;

/// Serial-number unwrapping of RTMP's 32-bit millisecond timestamps
/// onto a monotonic 64-bit timeline.
///
/// Per the RTMP specification §4 (Definitions / byte order —
/// December 2012): "Because timestamps are 32 bits long, they roll
/// over every 49 days, 17 hours, 2 minutes and 47.296 seconds.
/// Because streams are allowed to run continuously, potentially for
/// years on end, an RTMP application SHOULD use serial number
/// arithmetic \[RFC1982\] when processing timestamps, and SHOULD be
/// capable of handling wraparound", with adjacent timestamps assumed
/// within 2^31 − 1 ms of each other — "10000 comes after 4000000000,
/// and 3000000000 comes before 4000000000".
///
/// [`unwrap_ms`](Self::unwrap_ms) interprets each successive raw
/// timestamp's wrapping distance from the previous one as a signed
/// 32-bit step and accumulates it, so the extended timeline keeps
/// increasing across a rollover (and still follows legitimate small
/// backward steps, e.g. B-frame-less dts jitter after a seek).
#[derive(Debug, Default, Clone)]
pub struct TimestampUnwrapper {
    last: Option<(u32, i64)>,
}

impl TimestampUnwrapper {
    pub fn new() -> Self {
        Self::default()
    }

    /// Fold the next raw 32-bit timestamp onto the extended timeline
    /// (milliseconds). The first observation anchors the timeline at
    /// its own value.
    pub fn unwrap_ms(&mut self, raw: u32) -> i64 {
        let ext = match self.last {
            None => raw as i64,
            Some((prev_raw, prev_ext)) => {
                // RFC 1982-style: the wrapping u32 difference,
                // reinterpreted as i32, is the signed step. A forward
                // step across 0xFFFFFFFF→0 stays positive; a small
                // backward step stays negative.
                prev_ext + (raw.wrapping_sub(prev_raw) as i32 as i64)
            }
        };
        self.last = Some((raw, ext));
        ext
    }
}

/// Maximum number of packets to buffer during stream-codec probing
/// before giving up and returning whatever we have.
pub const PROBE_LIMIT: usize = 32;

/// Default read timeout applied to the underlying TCP socket
/// during probing. Without this an absent publisher would block
/// the opener forever waiting for the second stream type.
pub const PROBE_READ_TIMEOUT: Duration = Duration::from_secs(10);

/// Pre-buffered packet emitted by the probing phase, paired with
/// its target stream index so we can replay in arrival order.
struct BufferedPacket {
    packet: Packet,
    /// True for audio (stream 0), false for video (stream 1).
    /// Kept as a flag to keep [`Packet`] free of out-of-band info.
    #[allow(dead_code)]
    is_audio: bool,
}

/// Unified media event shared by the two protocol pumps — the
/// server-side [`RtmpSession`] (accepted publisher) and the
/// client-side [`RtmpPlayer`] (pulled play stream) both reduce to
/// this shape for the `PacketSource` bridge.
enum MediaSourceEvent {
    Audio {
        timestamp: u32,
        tag: AudioTag,
    },
    Video {
        timestamp: u32,
        tag: VideoTag,
    },
    Metadata(Amf0Value),
    /// Non-media event consumed by the bridge (NetStream control
    /// command, onStatus, user-control notification, …).
    Skip,
    /// Clean end of stream.
    End,
}

/// Internal abstraction over "something that yields RTMP media
/// events" so the probing loop and the steady-state `next_packet`
/// pump are written once for both directions.
trait MediaSource {
    fn next_media(&mut self) -> RtmpResult<MediaSourceEvent>;
    fn set_timeout(&mut self, d: Option<Duration>);
}

impl MediaSource for RtmpSession {
    fn next_media(&mut self) -> RtmpResult<MediaSourceEvent> {
        Ok(match self.next_packet()? {
            Some(StreamPacket::Audio { timestamp, tag }) => {
                MediaSourceEvent::Audio { timestamp, tag }
            }
            Some(StreamPacket::Video { timestamp, tag }) => {
                MediaSourceEvent::Video { timestamp, tag }
            }
            Some(StreamPacket::Metadata(value)) => MediaSourceEvent::Metadata(value),
            // NetStream control commands / NetConnection RPCs aren't
            // media packets.
            Some(StreamPacket::Command(_))
            | Some(StreamPacket::Call(_))
            | Some(StreamPacket::CallReply { .. }) => MediaSourceEvent::Skip,
            None => MediaSourceEvent::End,
        })
    }

    fn set_timeout(&mut self, d: Option<Duration>) {
        let _ = self.set_read_timeout(d);
    }
}

impl MediaSource for RtmpPlayer {
    fn next_media(&mut self) -> RtmpResult<MediaSourceEvent> {
        Ok(match self.next_packet()? {
            Some(PlayerPacket::Audio { timestamp, tag }) => {
                MediaSourceEvent::Audio { timestamp, tag }
            }
            Some(PlayerPacket::Video { timestamp, tag }) => {
                MediaSourceEvent::Video { timestamp, tag }
            }
            Some(PlayerPacket::Metadata(value)) => MediaSourceEvent::Metadata(value),
            // onStatus / user-control / RPC traffic isn't media.
            Some(PlayerPacket::Status { .. })
            | Some(PlayerPacket::Control(_))
            | Some(PlayerPacket::Call(_))
            | Some(PlayerPacket::CallReply { .. }) => MediaSourceEvent::Skip,
            None => MediaSourceEvent::End,
        })
    }

    fn set_timeout(&mut self, d: Option<Duration>) {
        let _ = self.set_read_timeout(d);
    }
}

/// Result of the shared probing phase.
struct ProbeState {
    streams: Vec<StreamInfo>,
    metadata: Vec<(String, String)>,
    buffered: VecDeque<BufferedPacket>,
    ended: bool,
    /// §4 timestamp unwrapping state, carried into the source so the
    /// steady phase continues the probe phase's extended timeline.
    ts: TimestampUnwrapper,
}

/// Shared probe loop: read up to [`PROBE_LIMIT`] events, note the
/// first audio + video codec parameters, and buffer every media
/// packet for in-order replay. A read timeout bounds each read so a
/// peer that only ever sends one stream type doesn't stall the
/// opener; on `WouldBlock` / `TimedOut` we accept whatever was
/// observed so far.
fn probe_source<S: MediaSource>(
    src: &mut S,
    read_timeout: Option<Duration>,
) -> RtmpResult<ProbeState> {
    if read_timeout.is_some() {
        src.set_timeout(read_timeout);
    }
    let mut state = ProbeState {
        streams: Vec::new(),
        metadata: Vec::new(),
        buffered: VecDeque::new(),
        ended: false,
        ts: TimestampUnwrapper::new(),
    };
    let mut have_audio = false;
    let mut have_video = false;

    for _ in 0..PROBE_LIMIT {
        if have_audio && have_video {
            break;
        }
        let next = match src.next_media() {
            Ok(ev) => ev,
            Err(RtmpError::Io(e))
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                // No more packets within the deadline — accept
                // whatever we have and bail out of probing.
                break;
            }
            Err(e) => return Err(e),
        };
        match next {
            MediaSourceEvent::Audio { timestamp, tag } => {
                if !have_audio {
                    state.streams.push(StreamInfo {
                        index: AUDIO_STREAM_INDEX,
                        time_base: RTMP_TIME_BASE,
                        duration: None,
                        start_time: None,
                        params: audio_codec_params(&tag),
                    });
                    have_audio = true;
                }
                state.buffered.push_back(BufferedPacket {
                    packet: audio_to_packet(state.ts.unwrap_ms(timestamp), &tag),
                    is_audio: true,
                });
            }
            MediaSourceEvent::Video { timestamp, tag } => {
                if !have_video {
                    state.streams.push(StreamInfo {
                        index: VIDEO_STREAM_INDEX,
                        time_base: RTMP_TIME_BASE,
                        duration: None,
                        start_time: None,
                        params: video_codec_params(&tag),
                    });
                    have_video = true;
                }
                state.buffered.push_back(BufferedPacket {
                    packet: video_to_packet(state.ts.unwrap_ms(timestamp), &tag),
                    is_audio: false,
                });
            }
            MediaSourceEvent::Metadata(value) => {
                flatten_metadata(&value, &mut state.metadata);
            }
            MediaSourceEvent::Skip => {}
            MediaSourceEvent::End => {
                state.ended = true;
                break;
            }
        }
    }

    // Restore blocking mode for the steady-state phase: we want
    // long-lived peers to block on read, not poll.
    src.set_timeout(None);

    // Stable order: audio (index 0) before video (index 1).
    state.streams.sort_by_key(|s| s.index);
    Ok(state)
}

/// Shared steady-state pump: read media events until one converts to
/// a [`Packet`], lazily registering a stream the probe phase never
/// observed, recording metadata, and latching `ended` on clean EOS.
fn steady_next<S: MediaSource>(
    src: &mut S,
    streams: &mut Vec<StreamInfo>,
    metadata: &mut Vec<(String, String)>,
    ended: &mut bool,
    ts: &mut TimestampUnwrapper,
) -> CoreResult<Packet> {
    loop {
        let event = src.next_media().map_err(rtmp_to_core_err)?;
        match event {
            MediaSourceEvent::Audio { timestamp, tag } => {
                if streams.iter().all(|s| s.index != AUDIO_STREAM_INDEX) {
                    streams.push(StreamInfo {
                        index: AUDIO_STREAM_INDEX,
                        time_base: RTMP_TIME_BASE,
                        duration: None,
                        start_time: None,
                        params: audio_codec_params(&tag),
                    });
                    streams.sort_by_key(|s| s.index);
                }
                return Ok(audio_to_packet(ts.unwrap_ms(timestamp), &tag));
            }
            MediaSourceEvent::Video { timestamp, tag } => {
                if streams.iter().all(|s| s.index != VIDEO_STREAM_INDEX) {
                    streams.push(StreamInfo {
                        index: VIDEO_STREAM_INDEX,
                        time_base: RTMP_TIME_BASE,
                        duration: None,
                        start_time: None,
                        params: video_codec_params(&tag),
                    });
                    streams.sort_by_key(|s| s.index);
                }
                return Ok(video_to_packet(ts.unwrap_ms(timestamp), &tag));
            }
            MediaSourceEvent::Metadata(value) => {
                flatten_metadata(&value, metadata);
                // Loop again — metadata isn't a media packet.
            }
            MediaSourceEvent::Skip => {}
            MediaSourceEvent::End => {
                *ended = true;
                return Err(CoreError::Eof);
            }
        }
    }
}

/// `PacketSource` wrapping an [`RtmpSession`].
///
/// Constructed by [`open_rtmp`] (the registry opener) or directly
/// from a session via [`RtmpPacketSource::from_session`] — the
/// latter is useful for callers driving their own
/// [`RtmpServer::accept`] loop who want the typed-packet
/// conversion without the listen-and-validate flow.
pub struct RtmpPacketSource {
    session: RtmpSession,
    streams: Vec<StreamInfo>,
    metadata: Vec<(String, String)>,
    buffered: VecDeque<BufferedPacket>,
    /// True when the underlying session has reported clean EOS
    /// (peer sent `closeStream` / `deleteStream` / `FCUnpublish`,
    /// or the TCP socket closed). After this `next_packet` keeps
    /// returning [`CoreError::Eof`].
    ended: bool,
    /// §4 serial-number timestamp unwrapping across the 49.7-day
    /// 32-bit rollover.
    ts: TimestampUnwrapper,
}

impl RtmpPacketSource {
    /// Wrap a freshly-accepted [`RtmpSession`] without any
    /// probing — `streams()` will be empty until the first audio
    /// / video packet flows. Suitable for callers who already
    /// know the stream shape (e.g. they are also the publisher).
    pub fn from_session(session: RtmpSession) -> Self {
        Self {
            session,
            streams: Vec::new(),
            metadata: Vec::new(),
            buffered: VecDeque::new(),
            ended: false,
            ts: TimestampUnwrapper::new(),
        }
    }

    /// Wrap a session and run the probing loop now: read up to
    /// [`PROBE_LIMIT`] packets, populate `streams()` with the
    /// observed audio + video codec ids, and buffer those packets
    /// for later [`next_packet`](Self::next_packet) calls.
    ///
    /// `read_timeout` bounds individual reads so a publisher that
    /// only ever sends one stream-type doesn't stall probing
    /// indefinitely. `None` keeps the socket blocking with no
    /// timeout — only safe when the caller knows the publisher
    /// will eventually send both kinds.
    pub fn from_session_with_probe(
        mut session: RtmpSession,
        read_timeout: Option<Duration>,
    ) -> RtmpResult<Self> {
        let state = probe_source(&mut session, read_timeout)?;
        Ok(Self {
            session,
            streams: state.streams,
            metadata: state.metadata,
            buffered: state.buffered,
            ended: state.ended,
            ts: state.ts,
        })
    }

    /// Borrow the wrapped session for advanced operations
    /// (`set_read_timeout`, `peer_addr`, …). Reading directly
    /// would interfere with the `PacketSource` machinery; prefer
    /// inspection-only methods.
    pub fn session(&self) -> &RtmpSession {
        &self.session
    }
}

impl PacketSource for RtmpPacketSource {
    fn streams(&self) -> &[StreamInfo] {
        &self.streams
    }

    fn next_packet(&mut self) -> CoreResult<Packet> {
        if let Some(buf) = self.buffered.pop_front() {
            return Ok(buf.packet);
        }
        if self.ended {
            return Err(CoreError::Eof);
        }
        steady_next(
            &mut self.session,
            &mut self.streams,
            &mut self.metadata,
            &mut self.ended,
            &mut self.ts,
        )
    }

    fn metadata(&self) -> &[(String, String)] {
        &self.metadata
    }

    fn duration_micros(&self) -> Option<i64> {
        // Live RTMP push has no a-priori duration.
        None
    }
}

/// `PacketSource` wrapping an [`RtmpPlayer`] — the pull (subscribe)
/// counterpart of [`RtmpPacketSource`].
///
/// Constructed by [`open_rtmp_play`] (the `rtmp-play://` registry
/// opener) or directly from a connected player via
/// [`RtmpPlayerPacketSource::from_player`] /
/// [`from_player_with_probe`](Self::from_player_with_probe). Stream
/// layout, time base, and timestamp folding match
/// [`RtmpPacketSource`] exactly — a downstream consumer cannot tell
/// whether the packets were pushed by a publisher or pulled from a
/// remote server.
pub struct RtmpPlayerPacketSource {
    player: RtmpPlayer,
    streams: Vec<StreamInfo>,
    metadata: Vec<(String, String)>,
    buffered: VecDeque<BufferedPacket>,
    /// True when the server ended playback (`UserControl StreamEOF`
    /// per §7.1.7, or TCP EOF). After this `next_packet` keeps
    /// returning [`CoreError::Eof`].
    ended: bool,
    /// §4 serial-number timestamp unwrapping across the 49.7-day
    /// 32-bit rollover.
    ts: TimestampUnwrapper,
}

impl RtmpPlayerPacketSource {
    /// Wrap a connected [`RtmpPlayer`] without probing — `streams()`
    /// stays empty until the first audio / video packet flows.
    pub fn from_player(player: RtmpPlayer) -> Self {
        Self {
            player,
            streams: Vec::new(),
            metadata: Vec::new(),
            buffered: VecDeque::new(),
            ended: false,
            ts: TimestampUnwrapper::new(),
        }
    }

    /// Wrap a player and probe now: read up to [`PROBE_LIMIT`]
    /// packets, populate `streams()` with the observed audio + video
    /// codec ids, and buffer those packets for later
    /// [`next_packet`](PacketSource::next_packet) calls. Semantics
    /// mirror [`RtmpPacketSource::from_session_with_probe`].
    pub fn from_player_with_probe(
        mut player: RtmpPlayer,
        read_timeout: Option<Duration>,
    ) -> RtmpResult<Self> {
        let state = probe_source(&mut player, read_timeout)?;
        Ok(Self {
            player,
            streams: state.streams,
            metadata: state.metadata,
            buffered: state.buffered,
            ended: state.ended,
            ts: state.ts,
        })
    }

    /// Borrow the wrapped player for inspection (`stream_id`,
    /// `is_recorded`, `server_capabilities`, …).
    pub fn player(&self) -> &RtmpPlayer {
        &self.player
    }
}

impl PacketSource for RtmpPlayerPacketSource {
    fn streams(&self) -> &[StreamInfo] {
        &self.streams
    }

    fn next_packet(&mut self) -> CoreResult<Packet> {
        if let Some(buf) = self.buffered.pop_front() {
            return Ok(buf.packet);
        }
        if self.ended {
            return Err(CoreError::Eof);
        }
        steady_next(
            &mut self.player,
            &mut self.streams,
            &mut self.metadata,
            &mut self.ended,
            &mut self.ts,
        )
    }

    fn metadata(&self) -> &[(String, String)] {
        &self.metadata
    }

    fn duration_micros(&self) -> Option<i64> {
        // A pulled RTMP stream reports no a-priori duration either
        // (a recorded stream's duration arrives via onMetaData,
        // surfaced through `metadata()` as a flat string).
        None
    }
}

// ────────────────────────── conversion helpers ──────────────────────────

/// Map an FLV audio tag into a [`Packet`] on stream 0.
///
/// The FLV tag header byte (sound-format / rate / size / stereo)
/// is stripped — downstream decoders consume the codec body
/// directly. For legacy AAC the 1-byte AAC packet-type marker is
/// retained ahead of the body so the decoder can distinguish
/// `AudioSpecificConfig` from raw frames; non-AAC legacy payloads
/// emit just `tag.body`.
///
/// For Enhanced RTMP v2 tags (`tag.audio_fourcc.is_some()`) the
/// full ExHeader + FourCC framing is stripped and the body is
/// passed through unmodified — the downstream codec consumes
/// the raw codec bytes (`OpusCodedData` / `Ac3CodedData` /
/// `Mp3CodedData` / `FlacCodedData` / `AacCodedData` / their
/// per-codec sequence-header bodies). `PacketTypeSequenceStart`
/// tags carry `flags.header = true`; `PacketTypeSequenceEnd`
/// surfaces as an empty `data` with the `header` flag set so a
/// consumer can route it to an end-of-stream signal.
///
/// `pts` and `dts` are emitted on the nanosecond [`RTMP_TIME_BASE`]
/// timeline: `timestamp_ms * 1_000_000`, plus any
/// `TimestampOffsetNano` ModEx contributions reported by
/// [`AudioTag::timestamp_offset_nano`] folded onto the presentation
/// time (audio has no separate decode time, so both `pts` and `dts`
/// receive the offset).
pub fn audio_to_packet(timestamp_ms: i64, tag: &AudioTag) -> Packet {
    let ts_ns = timestamp_ms * RTMP_MS_TO_NS;
    let nano_offset = tag.timestamp_offset_nano() as i64;
    let presentation_ns = ts_ns + nano_offset;
    let (data, is_header) = if tag.audio_fourcc.is_some() {
        // Enhanced RTMP v2: body is the codec's data verbatim
        // (per `ExAudioTagBody`). No AAC marker — that's a legacy
        // discriminator. SequenceStart is the header signal; the
        // SequenceEnd variant gets `header = true` too so the
        // downstream can recognise a flush boundary.
        let header = matches!(
            tag.ex_packet_type,
            Some(flv::AUDIO_PACKET_TYPE_SEQUENCE_START) | Some(flv::AUDIO_PACKET_TYPE_SEQUENCE_END)
        );
        (tag.body.clone(), header)
    } else {
        let mut data = Vec::with_capacity(tag.body.len() + 1);
        if tag.sound_format == AUDIO_FORMAT_AAC {
            data.push(tag.aac_packet_type.unwrap_or(flv::AAC_PACKET_TYPE_RAW));
        }
        data.extend_from_slice(&tag.body);
        let header = tag.sound_format == AUDIO_FORMAT_AAC
            && tag.aac_packet_type == Some(AAC_PACKET_TYPE_SEQUENCE_HEADER);
        (data, header)
    };
    let flags = oxideav_core::packet::PacketFlags {
        header: is_header,
        ..Default::default()
    };
    Packet {
        stream_index: AUDIO_STREAM_INDEX,
        time_base: RTMP_TIME_BASE,
        pts: Some(presentation_ns),
        dts: Some(presentation_ns),
        duration: None,
        flags,
        data,
    }
}

/// Map an FLV video tag into a [`Packet`] on stream 1.
///
/// For AVC, `pts = timestamp + composition_time` and
/// `dts = timestamp`. The 5-byte FLV/AVC header (frame-type +
/// codec-id + AVC-packet-type + 24-bit composition_time) is
/// stripped from `data`. Non-AVC video keeps its body as-is.
/// The keyframe flag is propagated from the FLV frame-type
/// nibble; sequence-header packets are flagged `header`.
///
/// `pts` and `dts` are emitted on the nanosecond
/// [`RTMP_TIME_BASE`] timeline. The core RTMP millisecond
/// `timestamp` is preserved verbatim as `dts =
/// timestamp_ms * 1_000_000`; the per-message
/// `TimestampOffsetNano` ModEx sum reported by
/// [`VideoTag::timestamp_offset_nano`] is added to `pts` only —
/// per `enhanced-rtmp-v2.pdf` the nanosecond offset adjusts the
/// *presentation* time of the current media message without
/// altering the core (decode) timestamp.
pub fn video_to_packet(timestamp_ms: i64, tag: &VideoTag) -> Packet {
    let dts_ns = timestamp_ms * RTMP_MS_TO_NS;
    // CTS lives in two places on the wire — AVC's 3-byte
    // SI24 (legacy), and the three NALU-based Enhanced-RTMP
    // FourCC variants paired with `CodedFrames`: HEVC (v1),
    // AVC and VVC (added v2). `parse_video` normalises all of
    // them into `tag.composition_time`; the non-NALU FourCCs
    // (`av01`, `vp09`, `vp08`) and the SequenceStart /
    // SequenceEnd / Metadata / CodedFramesX shapes leave it
    // zero (per §"ExVideoTagBody" "compositionTimeOffset is
    // implied to equal zero" — equivalent to "no offset").
    let has_cts =
        tag.codec_id == VIDEO_CODEC_AVC || (tag.fourcc.is_some() && tag.composition_time != 0);
    let cts_ns = if has_cts {
        (tag.composition_time as i64) * RTMP_MS_TO_NS
    } else {
        0
    };
    // ModEx `TimestampOffsetNano` (sub-millisecond, 0..=999_999 ns
    // per spec but the typed accessor returns up to ~16 M as the
    // raw bytesToUI24 sum across multiple entries) folds onto the
    // presentation time only.
    let nano_offset = tag.timestamp_offset_nano() as i64;
    let pts_ns = dts_ns + cts_ns + nano_offset;
    // `header` is set for *both* legacy AVC sequence headers and
    // Enhanced-RTMP `PacketTypeSequenceStart` tags — downstream
    // consumers can stash the body as `CodecParameters.extradata`
    // regardless of codec.
    let is_header = tag.is_avc_sequence_header() || tag.is_ex_sequence_header();
    // `PacketTypeMetadata` (Enhanced RTMP `colorInfo` etc.) is
    // not real frame data — surface it as a header-flagged
    // packet so a downstream consumer can route it to the
    // codec-parameters / HDR-metadata path instead of the
    // decoder. Per spec, FrameType bits are ignored when this
    // is set, so suppress the keyframe flag too.
    let is_metadata = tag.is_ex_metadata();
    let flags = oxideav_core::packet::PacketFlags {
        keyframe: !is_metadata && tag.is_keyframe(),
        header: is_header || is_metadata,
        ..Default::default()
    };
    Packet {
        stream_index: VIDEO_STREAM_INDEX,
        time_base: RTMP_TIME_BASE,
        pts: Some(pts_ns),
        dts: Some(dts_ns),
        duration: None,
        flags,
        data: tag.body.clone(),
    }
}

/// Map an FLV `sound_format` to an oxideav [`CodecId`]. Returns
/// `"unknown"` for codecs the workspace doesn't yet name —
/// downstream the decoder factory will fail to find a match,
/// which is the right outcome for "FLV says some legacy codec".
/// Enhanced RTMP v2 FourCC tags go through
/// [`audio_fourcc_codec_id`] / [`audio_codec_id_for_tag`]; this
/// helper only handles the legacy single-byte SoundFormat
/// nibble and returns `"unknown"` for the `ExHeader = 9`
/// sentinel.
pub fn audio_codec_id(sound_format: u8) -> CodecId {
    let s = match sound_format {
        AUDIO_FORMAT_PCM_LE => "pcm_s16le",
        AUDIO_FORMAT_ADPCM => "adpcm_swf",
        AUDIO_FORMAT_MP3 => "mp3",
        AUDIO_FORMAT_PCM_LE_8BIT => "pcm_u8",
        AUDIO_FORMAT_NELLYMOSER_16K_MONO => "nellymoser",
        AUDIO_FORMAT_NELLYMOSER_8K_MONO => "nellymoser",
        AUDIO_FORMAT_NELLYMOSER => "nellymoser",
        AUDIO_FORMAT_G711_ALAW => "pcm_alaw",
        AUDIO_FORMAT_G711_MULAW => "pcm_mulaw",
        AUDIO_FORMAT_AAC => "aac",
        AUDIO_FORMAT_SPEEX => "speex",
        _ => "unknown",
    };
    CodecId::new(s)
}

/// Map an Enhanced RTMP v2 FourCC audio tag (`b"Opus"` /
/// `b"fLaC"` / `b"ac-3"` / `b"ec-3"` / `b".mp3"` / `b"mp4a"`)
/// to an oxideav [`CodecId`]. Unknown FourCCs collapse to
/// `"unknown"`, matching the legacy [`audio_codec_id`] policy.
pub fn audio_fourcc_codec_id(fourcc: [u8; 4]) -> CodecId {
    let s = match &fourcc {
        b"Opus" => "opus",
        b"fLaC" => "flac",
        b"ac-3" => "ac3",
        b"ec-3" => "eac3",
        b".mp3" => "mp3",
        b"mp4a" => "aac",
        _ => "unknown",
    };
    CodecId::new(s)
}

/// Dispatch [`audio_codec_id`] / [`audio_fourcc_codec_id`] off a
/// parsed [`AudioTag`]: Enhanced RTMP v2 (FourCC) wins when set,
/// otherwise the legacy single-byte `sound_format` is consulted.
pub fn audio_codec_id_for_tag(tag: &AudioTag) -> CodecId {
    if let Some(fcc) = tag.audio_fourcc {
        audio_fourcc_codec_id(fcc)
    } else {
        audio_codec_id(tag.sound_format)
    }
}

/// Map an FLV `codec_id` (low nibble of the first video-tag
/// byte) to an oxideav [`CodecId`]. Legacy single-byte codec IDs
/// only — Enhanced RTMP FourCCs go through
/// [`video_codec_id_for_tag`].
pub fn video_codec_id(codec_id: u8) -> CodecId {
    let s = match codec_id {
        VIDEO_CODEC_H263 => "h263",
        VIDEO_CODEC_SCREEN => "flashsv",
        VIDEO_CODEC_VP6 => "vp6f",
        VIDEO_CODEC_VP6A => "vp6a",
        VIDEO_CODEC_SCREEN_V2 => "flashsv2",
        VIDEO_CODEC_AVC => "h264",
        _ => "unknown",
    };
    CodecId::new(s)
}

/// Map an Enhanced-RTMP FourCC video tag to an oxideav
/// [`CodecId`]. Covers the v1 set (`b"av01"` / `b"vp09"` /
/// `b"hvc1"`) and the v2 additions (`b"vp08"` / `b"avc1"` /
/// `b"vvc1"`). Unknown FourCCs (the spec leaves room for future
/// codecs) collapse to `"unknown"`, matching the legacy
/// [`video_codec_id`] policy.
pub fn video_fourcc_codec_id(fourcc: [u8; 4]) -> CodecId {
    let s = match &fourcc {
        b"av01" => "av1",
        b"vp09" => "vp9",
        b"hvc1" => "hevc",
        // Enhanced RTMP v2 (Veovera 2026) §"Enhanced Video".
        b"vp08" => "vp8",
        b"avc1" => "h264",
        b"vvc1" => "vvc",
        _ => "unknown",
    };
    CodecId::new(s)
}

/// Dispatch [`video_codec_id`] / [`video_fourcc_codec_id`] off a
/// parsed [`VideoTag`]: Enhanced RTMP (FourCC) wins when set,
/// otherwise the legacy single-byte `codec_id` is consulted.
pub fn video_codec_id_for_tag(tag: &VideoTag) -> CodecId {
    if let Some(fcc) = tag.fourcc {
        video_fourcc_codec_id(fcc)
    } else {
        video_codec_id(tag.codec_id)
    }
}

/// Build a [`CodecParameters`] for an audio stream from the
/// first observed FLV audio tag. Sample-rate / channel-count
/// hints from the tag header are populated when the FLV header
/// bits are meaningful (per spec they aren't for AAC — the
/// payload's `AudioSpecificConfig` carries the truth — so we
/// leave those fields `None` for AAC and let the decoder fill
/// them in).
///
/// For Enhanced RTMP v2 tags (`tag.audio_fourcc.is_some()`) the
/// codec id is resolved through the FourCC dispatcher and the
/// legacy SoundRate/Stereo hints are skipped (the spec mandates
/// they're not interpreted in ExHeader mode). For
/// `PacketTypeSequenceStart` we copy the codec's sequence-header
/// body into `extradata` — that's `OpusHead` for Opus,
/// `fLaC + STREAMINFO` for FLAC, and the AAC `AudioSpecificConfig`
/// for FourCC-AAC. AC-3 / E-AC-3 / MP3 have no SequenceStart
/// shape in v2, so `extradata` stays empty for them.
fn audio_codec_params(tag: &AudioTag) -> CodecParameters {
    let mut p = CodecParameters::audio(audio_codec_id_for_tag(tag));
    if tag.audio_fourcc.is_some() {
        if tag.ex_packet_type == Some(flv::AUDIO_PACKET_TYPE_SEQUENCE_START) {
            p.extradata = tag.body.clone();
        }
        return p;
    }
    if tag.sound_format != AUDIO_FORMAT_AAC {
        // FLV sound_rate: 0=5.5k 1=11k 2=22k 3=44.1k.
        let rate = match tag.sound_rate {
            0 => 5_512,
            1 => 11_025,
            2 => 22_050,
            _ => 44_100,
        };
        p.sample_rate = Some(rate);
        p.channels = Some(if tag.stereo { 2 } else { 1 });
    }
    if tag.sound_format == AUDIO_FORMAT_AAC
        && tag.aac_packet_type == Some(AAC_PACKET_TYPE_SEQUENCE_HEADER)
    {
        p.extradata = tag.body.clone();
    }
    p
}

/// Build a [`CodecParameters`] for a video stream from the first
/// observed FLV video tag. For AVC sequence headers we copy the
/// `AVCDecoderConfigurationRecord` into `extradata` so a
/// downstream H.264 decoder can find the SPS/PPS without
/// re-parsing the packet.
fn video_codec_params(tag: &VideoTag) -> CodecParameters {
    let mut p = CodecParameters::video(video_codec_id_for_tag(tag));
    // Legacy AVC: extradata is the `AVCDecoderConfigurationRecord`.
    // Enhanced RTMP `PacketTypeSequenceStart`: extradata is the
    // codec's configuration record per Table 4 — `HEVCDecoder
    // ConfigurationRecord` for `hvc1`, `AV1CodecConfigurationRecord`
    // for `av01`, `VPCodecConfigurationRecord` for `vp09`. In all
    // three cases the body is exactly what a downstream
    // ISO-BMFF-style decoder would expect to receive as
    // extradata, so we copy it through unmodified.
    if tag.is_avc_sequence_header() || tag.is_ex_sequence_header() {
        p.extradata = tag.body.clone();
    }
    p
}

/// Convert a few well-known scalar fields out of an `onMetaData`
/// AMF0 object into flat string pairs. Anything more elaborate is
/// dropped — `metadata()` is best-effort, callers needing the full
/// structure should use [`RtmpServer::accept`] directly.
fn flatten_metadata(value: &Amf0Value, out: &mut Vec<(String, String)>) {
    let pairs: &[(String, Amf0Value)] = match value {
        Amf0Value::Object(p) => p.as_slice(),
        Amf0Value::EcmaArray(p) => p.as_slice(),
        _ => return,
    };
    for (k, v) in pairs {
        let s = match v {
            Amf0Value::Number(n) => format!("{n}"),
            Amf0Value::Boolean(b) => b.to_string(),
            Amf0Value::String(s) => s.clone(),
            // Skip nested objects / arrays / null / undefined —
            // the metadata() surface is intentionally flat.
            _ => continue,
        };
        out.push((k.clone(), s));
    }
}

/// Lift a crate-local [`crate::Error`] into the workspace
/// [`oxideav_core::Error`]. Only the few variants we may
/// surface during steady-state pumping are mapped specially —
/// everything else collapses to `Error::Other`.
pub(crate) fn rtmp_to_core_err(e: RtmpError) -> CoreError {
    match e {
        RtmpError::Io(io) => CoreError::Io(io),
        RtmpError::UnexpectedEof => CoreError::Eof,
        RtmpError::Timeout => CoreError::Other("rtmp: timeout".to_string()),
        RtmpError::Rejected(r) => CoreError::Other(format!("rtmp: rejected: {r}")),
        RtmpError::ProtocolViolation(m) => CoreError::InvalidData(format!("rtmp protocol: {m}")),
        RtmpError::InvalidAmf0(m) => CoreError::InvalidData(format!("rtmp amf0: {m}")),
        RtmpError::InvalidChunk(m) => CoreError::InvalidData(format!("rtmp chunk: {m}")),
        RtmpError::InvalidCommand(m) => CoreError::InvalidData(format!("rtmp command: {m}")),
        RtmpError::UnsupportedHandshakeVersion(v) => {
            CoreError::Unsupported(format!("rtmp handshake version 0x{v:02x}"))
        }
        RtmpError::Other(m) => CoreError::Other(format!("rtmp: {m}")),
    }
}

// ───────────────────────────── opener ─────────────────────────────

/// Parsed view of an `rtmp://host:port/app[/stream_name]` URL,
/// reused for the registry-listen flow. Mirrors
/// [`crate::client::RtmpUrl`] but is parsed independently so we
/// don't accidentally couple the client and adapter parsers.
#[derive(Debug, Clone)]
struct ListenUrl {
    bind_addr: String,
    expected_app: String,
    expected_stream: String,
}

impl ListenUrl {
    fn parse(uri: &str) -> CoreResult<Self> {
        let s = uri
            .strip_prefix("rtmp://")
            .ok_or_else(|| CoreError::InvalidData(format!("not an rtmp:// URL: {uri}")))?;
        let slash = s
            .find('/')
            .ok_or_else(|| CoreError::InvalidData(format!("rtmp URL missing /app: {uri}")))?;
        let authority = &s[..slash];
        let path = &s[slash + 1..];
        let (host, port_str) = match authority.rsplit_once(':') {
            Some((h, p)) => (h, p),
            None => (authority, "1935"),
        };
        let port: u16 = port_str
            .parse()
            .map_err(|e| CoreError::InvalidData(format!("rtmp URL bad port {port_str:?}: {e}")))?;
        let bind_host = if host.is_empty() { "0.0.0.0" } else { host };
        let bind_addr = format!("{bind_host}:{port}");
        let (app, stream_name) = match path.find('/') {
            Some(i) => (path[..i].to_owned(), path[i + 1..].to_owned()),
            None => (path.to_owned(), String::new()),
        };
        Ok(Self {
            bind_addr,
            expected_app: app,
            expected_stream: stream_name,
        })
    }
}

/// `SourceRegistry` opener for the `rtmp://` scheme.
///
/// Listens on the URL's `host:port`, accepts the first incoming
/// publisher, and returns it as a [`PacketSource`]. The
/// publisher's announced `app` / `stream_name` must match the URL
/// path or the publish is politely rejected and the listener
/// closes (the opener returns [`CoreError::InvalidData`]).
///
/// Subsequent publishers cannot share the same opener call: each
/// `SourceRegistry::open("rtmp://…")` is a one-shot accept. To
/// service many publishers, use [`RtmpServer::serve`] directly.
pub fn open_rtmp(uri: &str) -> CoreResult<Box<dyn PacketSource>> {
    let url = ListenUrl::parse(uri)?;
    // Resolve once — if a hostname doesn't resolve we want a
    // clean error rather than a confused TcpListener::bind panic.
    let resolved = url
        .bind_addr
        .to_socket_addrs()
        .map_err(CoreError::Io)?
        .next()
        .ok_or_else(|| {
            CoreError::InvalidData(format!("rtmp URL resolved no addresses: {}", url.bind_addr))
        })?;
    let server = RtmpServer::bind(resolved).map_err(rtmp_to_core_err)?;
    let req = server.accept().map_err(rtmp_to_core_err)?;
    if !url.expected_app.is_empty() && req.app != url.expected_app {
        let actual = req.app.clone();
        let expected = url.expected_app.clone();
        let _ = req.reject("unexpected app");
        return Err(CoreError::InvalidData(format!(
            "rtmp publisher app mismatch: expected {expected:?}, got {actual:?}"
        )));
    }
    if !url.expected_stream.is_empty() && req.stream_name != url.expected_stream {
        let actual = req.stream_name.clone();
        let expected = url.expected_stream.clone();
        let _ = req.reject("unexpected stream key");
        return Err(CoreError::InvalidData(format!(
            "rtmp publisher stream-name mismatch: expected {expected:?}, got {actual:?}"
        )));
    }
    let session = req.accept().map_err(rtmp_to_core_err)?;
    let source = RtmpPacketSource::from_session_with_probe(session, Some(PROBE_READ_TIMEOUT))
        .map_err(rtmp_to_core_err)?;
    Ok(Box::new(source))
}

/// `SourceRegistry` opener for the `rtmp-play://` scheme — the
/// **pull** direction.
///
/// Where [`open_rtmp`] listens for an incoming publisher,
/// `open_rtmp_play` dials the named remote server as a §4.2.1 play
/// client (`rtmp-play://host[:port]/app/stream-name` →
/// `rtmp://host[:port]/app/stream-name` on the wire), waits for
/// `onStatus(NetStream.Play.Start)`, probes the stream shape, and
/// returns the same two-stream [`PacketSource`] layout as the listen
/// flow. The distinct scheme keeps the two opposite network roles —
/// bind-and-accept vs. connect-and-request — unambiguous at the URI
/// level.
pub fn open_rtmp_play(uri: &str) -> CoreResult<Box<dyn PacketSource>> {
    let rest = uri
        .strip_prefix("rtmp-play://")
        .ok_or_else(|| CoreError::InvalidData(format!("not an rtmp-play:// URL: {uri}")))?;
    let dial = format!("rtmp://{rest}");
    let player = RtmpPlayer::connect_with_options(&dial, &PlayOptions::default())
        .map_err(rtmp_to_core_err)?;
    let source = RtmpPlayerPacketSource::from_player_with_probe(player, Some(PROBE_READ_TIMEOUT))
        .map_err(rtmp_to_core_err)?;
    Ok(Box::new(source))
}

/// Install the `rtmp://` (listen / accept-a-publisher) and
/// `rtmp-play://` (dial / pull-a-stream) schemes on the given
/// [`SourceRegistry`].
///
/// * `rtmp://host:port/app/stream-name` — one-shot listener that
///   accepts a single publisher ([`open_rtmp`]).
/// * `rtmp-play://host:port/app/stream-name` — §4.2.1 play client
///   that pulls the named stream from a remote server
///   ([`open_rtmp_play`]).
///
/// Idempotent: re-registering replaces the prior openers.
pub fn register(registry: &mut SourceRegistry) {
    registry.register_packets("rtmp", open_rtmp);
    registry.register_packets("rtmp-play", open_rtmp_play);
}

// Suppress dead_code on the BytesSource re-export — it's needed
// only for documentation cross-references in this module's docs.
#[allow(dead_code)]
fn _bytes_source_anchor(_: Box<dyn BytesSource>) {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::flv::{
        AAC_PACKET_TYPE_RAW, AUDIO_FORMAT_EX_HEADER, AVC_PACKET_TYPE_NALU,
        AVC_PACKET_TYPE_SEQUENCE_HEADER, EX_PACKET_TYPE_CODED_FRAMES, EX_PACKET_TYPE_METADATA,
        EX_PACKET_TYPE_SEQUENCE_START, FOURCC_AV1, FOURCC_HEVC, FOURCC_VP9, VIDEO_FRAME_INTER,
        VIDEO_FRAME_KEYFRAME,
    };

    #[test]
    fn audio_codec_id_maps_aac_and_mp3() {
        assert_eq!(audio_codec_id(AUDIO_FORMAT_AAC).as_str(), "aac");
        assert_eq!(audio_codec_id(AUDIO_FORMAT_MP3).as_str(), "mp3");
        assert_eq!(audio_codec_id(AUDIO_FORMAT_PCM_LE).as_str(), "pcm_s16le");
        // Anything we don't model returns "unknown" — registry will
        // surface the gap rather than silently mis-decode.
        assert_eq!(audio_codec_id(0xFF).as_str(), "unknown");
    }

    #[test]
    fn video_codec_id_maps_avc_and_h263() {
        assert_eq!(video_codec_id(VIDEO_CODEC_AVC).as_str(), "h264");
        assert_eq!(video_codec_id(VIDEO_CODEC_H263).as_str(), "h263");
        assert_eq!(video_codec_id(VIDEO_CODEC_VP6).as_str(), "vp6f");
        assert_eq!(video_codec_id(0xFF).as_str(), "unknown");
    }

    #[test]
    fn audio_aac_seq_header_packet_carries_marker_and_header_flag() {
        let tag = AudioTag {
            mod_ex: Vec::new(),
            sound_format: AUDIO_FORMAT_AAC,
            sound_rate: 3,
            sound_size_16bit: true,
            stereo: true,
            aac_packet_type: Some(AAC_PACKET_TYPE_SEQUENCE_HEADER),
            body: vec![0x12, 0x10],
            ex_packet_type: None,
            audio_fourcc: None,

            multitrack: None,
        };
        let pkt = audio_to_packet(0, &tag);
        assert_eq!(pkt.stream_index, AUDIO_STREAM_INDEX);
        assert_eq!(pkt.time_base, RTMP_TIME_BASE);
        assert_eq!(pkt.pts, Some(0));
        assert_eq!(pkt.dts, Some(0));
        assert!(pkt.flags.header);
        // packet type byte (0 = seq header) + body
        assert_eq!(pkt.data, vec![0x00, 0x12, 0x10]);
    }

    #[test]
    fn timestamp_unwrapper_anchors_at_first_value() {
        let mut u = TimestampUnwrapper::new();
        assert_eq!(u.unwrap_ms(5000), 5000);
        assert_eq!(u.unwrap_ms(6000), 6000);
    }

    #[test]
    fn timestamp_unwrapper_spec_examples() {
        // §4: "10000 comes after 4000000000" — the step across the
        // rollover is positive.
        let mut u = TimestampUnwrapper::new();
        let a = u.unwrap_ms(4_000_000_000);
        let b = u.unwrap_ms(10_000);
        assert!(b > a, "10000 must come after 4000000000 ({b} vs {a})");
        // Exact step: (2^32 - 4000000000) + 10000.
        assert_eq!(b - a, (1i64 << 32) - 4_000_000_000 + 10_000);

        // "3000000000 comes before 4000000000" — a backward step.
        let mut u = TimestampUnwrapper::new();
        let a = u.unwrap_ms(4_000_000_000);
        let b = u.unwrap_ms(3_000_000_000);
        assert!(b < a);
        assert_eq!(a - b, 1_000_000_000);
    }

    #[test]
    fn timestamp_unwrapper_survives_multiple_rollovers() {
        // A stream running for "years on end": step by 2^31 - 1 (the
        // largest unambiguous forward step) repeatedly and confirm the
        // extended timeline keeps growing past several 32-bit wraps.
        let mut u = TimestampUnwrapper::new();
        let step = (1u32 << 31) - 1;
        let mut raw = 0u32;
        let mut prev = u.unwrap_ms(raw);
        for _ in 0..10 {
            raw = raw.wrapping_add(step);
            let ext = u.unwrap_ms(raw);
            assert_eq!(ext - prev, step as i64);
            prev = ext;
        }
        assert!(
            prev > u64::from(u32::MAX) as i64,
            "timeline must exceed 32 bits"
        );
    }

    #[test]
    fn unwrapped_timestamp_reaches_packet_timeline_past_32_bits() {
        let tag = AudioTag {
            mod_ex: Vec::new(),
            sound_format: AUDIO_FORMAT_AAC,
            sound_rate: 3,
            sound_size_16bit: true,
            stereo: true,
            aac_packet_type: Some(AAC_PACKET_TYPE_RAW),
            body: vec![0x01],
            ex_packet_type: None,
            audio_fourcc: None,
            multitrack: None,
        };
        // 50 days in ms — beyond the u32 rollover point.
        let fifty_days_ms: i64 = 50 * 24 * 3600 * 1000;
        let pkt = audio_to_packet(fifty_days_ms, &tag);
        assert_eq!(pkt.pts, Some(fifty_days_ms * RTMP_MS_TO_NS));
    }

    #[test]
    fn audio_aac_raw_packet_keeps_packet_type_byte() {
        let tag = AudioTag {
            mod_ex: Vec::new(),
            sound_format: AUDIO_FORMAT_AAC,
            sound_rate: 3,
            sound_size_16bit: true,
            stereo: true,
            aac_packet_type: Some(AAC_PACKET_TYPE_RAW),
            body: vec![0xAB, 0xCD, 0xEF],
            ex_packet_type: None,
            audio_fourcc: None,

            multitrack: None,
        };
        let pkt = audio_to_packet(123, &tag);
        // 123 ms → 123_000_000 ns on the RTMP_TIME_BASE timeline.
        assert_eq!(pkt.pts, Some(123 * RTMP_MS_TO_NS));
        assert_eq!(pkt.dts, Some(123 * RTMP_MS_TO_NS));
        assert!(!pkt.flags.header);
        assert_eq!(pkt.data, vec![0x01, 0xAB, 0xCD, 0xEF]);
    }

    #[test]
    fn audio_mp3_packet_strips_flv_header_only() {
        let tag = AudioTag {
            mod_ex: Vec::new(),
            sound_format: AUDIO_FORMAT_MP3,
            sound_rate: 3,
            sound_size_16bit: true,
            stereo: true,
            aac_packet_type: None,
            body: vec![0xFF, 0xFB, 0x90, 0x00],
            ex_packet_type: None,
            audio_fourcc: None,

            multitrack: None,
        };
        let pkt = audio_to_packet(40, &tag);
        // No AAC marker prepended for non-AAC.
        assert_eq!(pkt.data, vec![0xFF, 0xFB, 0x90, 0x00]);
        assert_eq!(pkt.pts, Some(40 * RTMP_MS_TO_NS));
    }

    // ------- Enhanced RTMP v2 audio dispatch into Packet -------

    #[test]
    fn audio_codec_id_for_tag_dispatches_legacy_vs_fourcc() {
        let legacy_aac = AudioTag {
            mod_ex: Vec::new(),
            sound_format: AUDIO_FORMAT_AAC,
            sound_rate: 3,
            sound_size_16bit: true,
            stereo: true,
            aac_packet_type: Some(AAC_PACKET_TYPE_SEQUENCE_HEADER),
            body: vec![],
            ex_packet_type: None,
            audio_fourcc: None,

            multitrack: None,
        };
        assert_eq!(audio_codec_id_for_tag(&legacy_aac).as_str(), "aac");
        for (fcc, expected) in [
            (flv::FOURCC_OPUS, "opus"),
            (flv::FOURCC_FLAC, "flac"),
            (flv::FOURCC_AC3, "ac3"),
            (flv::FOURCC_EAC3, "eac3"),
            (flv::FOURCC_MP3, "mp3"),
            (flv::FOURCC_AAC, "aac"),
        ] {
            let t = AudioTag {
                mod_ex: Vec::new(),
                sound_format: AUDIO_FORMAT_EX_HEADER,
                sound_rate: 0,
                sound_size_16bit: false,
                stereo: false,
                aac_packet_type: None,
                ex_packet_type: Some(flv::AUDIO_PACKET_TYPE_CODED_FRAMES),
                audio_fourcc: Some(fcc),
                body: vec![],

                multitrack: None,
            };
            assert_eq!(audio_codec_id_for_tag(&t).as_str(), expected);
        }
    }

    #[test]
    fn audio_fourcc_codec_id_unknown_collapses() {
        // Forward-compatible fallback for codecs the workspace
        // doesn't yet name.
        assert_eq!(audio_fourcc_codec_id(*b"xxxx").as_str(), "unknown");
    }

    #[test]
    fn ex_opus_sequence_start_packet_sets_header_flag_and_strips_fourcc() {
        let tag = AudioTag {
            mod_ex: Vec::new(),
            sound_format: AUDIO_FORMAT_EX_HEADER,
            sound_rate: 0,
            sound_size_16bit: false,
            stereo: false,
            aac_packet_type: None,
            ex_packet_type: Some(flv::AUDIO_PACKET_TYPE_SEQUENCE_START),
            audio_fourcc: Some(flv::FOURCC_OPUS),
            body: b"OpusHead\x01\x02\x38\x01\x80\xbb\x00\x00\x00\x00\x00".to_vec(),

            multitrack: None,
        };
        let pkt = audio_to_packet(0, &tag);
        assert_eq!(pkt.stream_index, AUDIO_STREAM_INDEX);
        assert!(pkt.flags.header);
        // The Opus ID-header bytes pass through unchanged — no
        // legacy AAC packet-type marker is prepended in Enhanced
        // mode.
        assert_eq!(
            pkt.data,
            b"OpusHead\x01\x02\x38\x01\x80\xbb\x00\x00\x00\x00\x00".to_vec()
        );
    }

    #[test]
    fn ex_ac3_coded_frames_packet_strips_fourcc_and_keeps_body() {
        let tag = AudioTag {
            mod_ex: Vec::new(),
            sound_format: AUDIO_FORMAT_EX_HEADER,
            sound_rate: 0,
            sound_size_16bit: false,
            stereo: false,
            aac_packet_type: None,
            ex_packet_type: Some(flv::AUDIO_PACKET_TYPE_CODED_FRAMES),
            audio_fourcc: Some(flv::FOURCC_AC3),
            body: vec![0x0B, 0x77, 0xAB, 0xCD, 0xEF],

            multitrack: None,
        };
        let pkt = audio_to_packet(200, &tag);
        assert!(!pkt.flags.header);
        assert_eq!(pkt.dts, Some(200 * RTMP_MS_TO_NS));
        assert_eq!(pkt.pts, Some(200 * RTMP_MS_TO_NS));
        // Raw AC-3 frame bytes — no marker, no header.
        assert_eq!(pkt.data, vec![0x0B, 0x77, 0xAB, 0xCD, 0xEF]);
    }

    #[test]
    fn ex_audio_sequence_end_packet_flagged_header_with_empty_body() {
        let tag = AudioTag {
            mod_ex: Vec::new(),
            sound_format: AUDIO_FORMAT_EX_HEADER,
            sound_rate: 0,
            sound_size_16bit: false,
            stereo: false,
            aac_packet_type: None,
            ex_packet_type: Some(flv::AUDIO_PACKET_TYPE_SEQUENCE_END),
            audio_fourcc: Some(flv::FOURCC_OPUS),
            body: vec![],

            multitrack: None,
        };
        let pkt = audio_to_packet(999, &tag);
        // SequenceEnd is a flush boundary — header flag lets the
        // consumer route it without trying to decode an empty
        // frame.
        assert!(pkt.flags.header);
        assert!(pkt.data.is_empty());
        assert_eq!(pkt.dts, Some(999 * RTMP_MS_TO_NS));
    }

    #[test]
    fn ex_audio_codec_params_copies_sequence_start_to_extradata() {
        let tag = AudioTag {
            mod_ex: Vec::new(),
            sound_format: AUDIO_FORMAT_EX_HEADER,
            sound_rate: 0,
            sound_size_16bit: false,
            stereo: false,
            aac_packet_type: None,
            ex_packet_type: Some(flv::AUDIO_PACKET_TYPE_SEQUENCE_START),
            audio_fourcc: Some(flv::FOURCC_FLAC),
            body: b"fLaC\x80\x00\x00\x22streaminfo-body-bytes".to_vec(),

            multitrack: None,
        };
        let p = audio_codec_params(&tag);
        assert_eq!(p.codec_id.as_str(), "flac");
        assert_eq!(p.extradata, tag.body);
        // Legacy SoundRate/Stereo hints are skipped — the spec
        // says the bit-field "are not interpreted" in ExHeader
        // mode and codec-internal headers carry the truth.
        assert_eq!(p.sample_rate, None);
        assert_eq!(p.channels, None);
    }

    #[test]
    fn ex_audio_codec_params_ac3_coded_frames_leaves_extradata_empty() {
        // AC-3 has no SequenceStart shape defined in v2 — only
        // CodedFrames carries data. Ensure `audio_codec_params`
        // doesn't accidentally treat a CodedFrames body as
        // extradata.
        let tag = AudioTag {
            mod_ex: Vec::new(),
            sound_format: AUDIO_FORMAT_EX_HEADER,
            sound_rate: 0,
            sound_size_16bit: false,
            stereo: false,
            aac_packet_type: None,
            ex_packet_type: Some(flv::AUDIO_PACKET_TYPE_CODED_FRAMES),
            audio_fourcc: Some(flv::FOURCC_AC3),
            body: vec![0x0B, 0x77, 0xAB, 0xCD],

            multitrack: None,
        };
        let p = audio_codec_params(&tag);
        assert_eq!(p.codec_id.as_str(), "ac3");
        assert!(p.extradata.is_empty());
    }

    #[test]
    fn video_avc_keyframe_packet_keyframe_flag_and_no_pts_offset() {
        let tag = VideoTag {
            mod_ex: Vec::new(),
            frame_type: VIDEO_FRAME_KEYFRAME,
            codec_id: VIDEO_CODEC_AVC,
            avc_packet_type: Some(AVC_PACKET_TYPE_NALU),
            composition_time: 0,
            body: b"\x00\x00\x00\x05hello".to_vec(),
            ex_packet_type: None,
            fourcc: None,

            multitrack: None,
        };
        let pkt = video_to_packet(33, &tag);
        assert_eq!(pkt.stream_index, VIDEO_STREAM_INDEX);
        assert!(pkt.flags.keyframe);
        assert!(!pkt.flags.header);
        assert_eq!(pkt.pts, Some(33 * RTMP_MS_TO_NS));
        assert_eq!(pkt.dts, Some(33 * RTMP_MS_TO_NS));
        assert_eq!(pkt.data, b"\x00\x00\x00\x05hello".to_vec());
    }

    #[test]
    fn video_avc_inter_packet_with_negative_cts_offsets_pts() {
        let tag = VideoTag {
            mod_ex: Vec::new(),
            frame_type: VIDEO_FRAME_INTER,
            codec_id: VIDEO_CODEC_AVC,
            avc_packet_type: Some(AVC_PACKET_TYPE_NALU),
            composition_time: -10,
            body: vec![1, 2, 3],
            ex_packet_type: None,
            fourcc: None,

            multitrack: None,
        };
        let pkt = video_to_packet(100, &tag);
        assert!(!pkt.flags.keyframe);
        assert_eq!(pkt.dts, Some(100 * RTMP_MS_TO_NS));
        assert_eq!(pkt.pts, Some(90 * RTMP_MS_TO_NS));
    }

    #[test]
    fn video_avc_seq_header_marks_header_flag() {
        let tag = VideoTag {
            mod_ex: Vec::new(),
            frame_type: VIDEO_FRAME_KEYFRAME,
            codec_id: VIDEO_CODEC_AVC,
            avc_packet_type: Some(AVC_PACKET_TYPE_SEQUENCE_HEADER),
            composition_time: 0,
            body: b"\x01\x42\x80\x1e".to_vec(),
            ex_packet_type: None,
            fourcc: None,

            multitrack: None,
        };
        let pkt = video_to_packet(0, &tag);
        assert!(pkt.flags.keyframe);
        assert!(pkt.flags.header);
        assert_eq!(pkt.data, b"\x01\x42\x80\x1e".to_vec());
    }

    #[test]
    fn video_h263_packet_keeps_body_and_pts_eq_dts() {
        let tag = VideoTag {
            mod_ex: Vec::new(),
            frame_type: VIDEO_FRAME_INTER,
            codec_id: VIDEO_CODEC_H263,
            avc_packet_type: None,
            composition_time: 0,
            body: vec![0xAA, 0xBB, 0xCC],
            ex_packet_type: None,
            fourcc: None,

            multitrack: None,
        };
        let pkt = video_to_packet(50, &tag);
        assert_eq!(pkt.pts, pkt.dts);
        assert_eq!(pkt.data, vec![0xAA, 0xBB, 0xCC]);
    }

    // ------- Enhanced RTMP v1 dispatch into Packet -------

    #[test]
    fn video_codec_id_for_tag_dispatches_legacy_vs_fourcc() {
        let avc = VideoTag {
            mod_ex: Vec::new(),
            frame_type: VIDEO_FRAME_KEYFRAME,
            codec_id: VIDEO_CODEC_AVC,
            avc_packet_type: Some(AVC_PACKET_TYPE_NALU),
            composition_time: 0,
            body: vec![],
            ex_packet_type: None,
            fourcc: None,

            multitrack: None,
        };
        assert_eq!(video_codec_id_for_tag(&avc).as_str(), "h264");
        for (fcc, expected) in [
            (FOURCC_HEVC, "hevc"),
            (FOURCC_AV1, "av1"),
            (FOURCC_VP9, "vp9"),
        ] {
            let t = VideoTag {
                mod_ex: Vec::new(),
                frame_type: VIDEO_FRAME_KEYFRAME,
                codec_id: 0,
                avc_packet_type: None,
                composition_time: 0,
                body: vec![],
                ex_packet_type: Some(EX_PACKET_TYPE_CODED_FRAMES),
                fourcc: Some(fcc),

                multitrack: None,
            };
            assert_eq!(video_codec_id_for_tag(&t).as_str(), expected);
        }
    }

    #[test]
    fn ex_hevc_sequence_start_packet_sets_header_flag() {
        // Enhanced RTMP `PacketTypeSequenceStart` body is the
        // `HEVCDecoderConfigurationRecord`. We surface it just
        // like AVC's `avcC` — `flags.header == true`, body in
        // `pkt.data` for downstream extradata harvesting.
        let tag = VideoTag {
            mod_ex: Vec::new(),
            frame_type: VIDEO_FRAME_KEYFRAME,
            codec_id: 0,
            avc_packet_type: None,
            composition_time: 0,
            body: b"\x01hvcc-stub".to_vec(),
            ex_packet_type: Some(EX_PACKET_TYPE_SEQUENCE_START),
            fourcc: Some(FOURCC_HEVC),

            multitrack: None,
        };
        let pkt = video_to_packet(0, &tag);
        assert!(pkt.flags.header);
        assert!(pkt.flags.keyframe);
        assert_eq!(pkt.dts, Some(0));
        assert_eq!(pkt.pts, Some(0));
        assert_eq!(pkt.time_base, RTMP_TIME_BASE);
        assert_eq!(pkt.data, b"\x01hvcc-stub".to_vec());
    }

    #[test]
    fn ex_hevc_coded_frames_with_cts_offsets_pts() {
        // Only HEVC × CodedFrames carries CTS on the wire in
        // Enhanced RTMP — exercise the CTS-propagation branch.
        let tag = VideoTag {
            mod_ex: Vec::new(),
            frame_type: VIDEO_FRAME_INTER,
            codec_id: 0,
            avc_packet_type: None,
            composition_time: 17,
            body: b"\x00\x00\x00\x04NALU".to_vec(),
            ex_packet_type: Some(EX_PACKET_TYPE_CODED_FRAMES),
            fourcc: Some(FOURCC_HEVC),

            multitrack: None,
        };
        let pkt = video_to_packet(200, &tag);
        assert!(!pkt.flags.keyframe);
        assert!(!pkt.flags.header);
        assert_eq!(pkt.dts, Some(200 * RTMP_MS_TO_NS));
        assert_eq!(pkt.pts, Some(217 * RTMP_MS_TO_NS));
    }

    #[test]
    fn ex_av1_coded_frames_no_cts_offset() {
        // AV1 / VP9 leave CTS implied-zero — `pts == dts`.
        let tag = VideoTag {
            mod_ex: Vec::new(),
            frame_type: VIDEO_FRAME_KEYFRAME,
            codec_id: 0,
            avc_packet_type: None,
            composition_time: 0,
            body: vec![0x0a, 0x0b],
            ex_packet_type: Some(EX_PACKET_TYPE_CODED_FRAMES),
            fourcc: Some(FOURCC_AV1),

            multitrack: None,
        };
        let pkt = video_to_packet(500, &tag);
        assert!(pkt.flags.keyframe);
        assert!(!pkt.flags.header);
        assert_eq!(pkt.dts, pkt.pts);
        assert_eq!(pkt.dts, Some(500 * RTMP_MS_TO_NS));
    }

    #[test]
    fn ex_metadata_packet_ignores_frame_type_flags() {
        // Per spec the FrameType bits MUST be ignored for
        // PacketTypeMetadata; we suppress `keyframe` and set
        // `header` so the consumer routes it to its sideband
        // (HDR `colorInfo` etc.) instead of the decoder.
        let tag = VideoTag {
            mod_ex: Vec::new(),
            frame_type: VIDEO_FRAME_KEYFRAME, // would normally → keyframe = true
            codec_id: 0,
            avc_packet_type: None,
            composition_time: 0,
            body: b"amf-payload".to_vec(),
            ex_packet_type: Some(EX_PACKET_TYPE_METADATA),
            fourcc: Some(FOURCC_HEVC),

            multitrack: None,
        };
        let pkt = video_to_packet(123, &tag);
        assert!(!pkt.flags.keyframe);
        assert!(pkt.flags.header);
        assert_eq!(pkt.data, b"amf-payload".to_vec());
    }

    // ------- TimestampOffsetNano fold into the ns Packet timeline -------

    #[test]
    fn audio_timestamp_offset_nano_folds_into_presentation_time() {
        // `enhanced-rtmp-v2.pdf` §"ExAudioTagHeader" defines a
        // `TimestampOffsetNano` ModEx subtype carrying a 0..=999_999
        // ns offset added to the *presentation* time of the current
        // media message without altering the core RTMP timestamp.
        // For audio pts == dts (no separate decode time) so the
        // offset rides on both.
        let tag = AudioTag {
            mod_ex: vec![crate::flv::ModEx::timestamp_offset_nano_entry(750_000)],
            sound_format: AUDIO_FORMAT_EX_HEADER,
            sound_rate: 0,
            sound_size_16bit: false,
            stereo: false,
            aac_packet_type: None,
            ex_packet_type: Some(flv::AUDIO_PACKET_TYPE_CODED_FRAMES),
            audio_fourcc: Some(flv::FOURCC_OPUS),
            body: vec![0x12, 0x34, 0x56],

            multitrack: None,
        };
        let pkt = audio_to_packet(40, &tag);
        // 40 ms * 1e6 ns/ms + 750_000 ns = 40_750_000 ns.
        assert_eq!(pkt.pts, Some(40_750_000));
        assert_eq!(pkt.dts, Some(40_750_000));
        assert_eq!(pkt.time_base, RTMP_TIME_BASE);
    }

    #[test]
    fn video_timestamp_offset_nano_folds_into_pts_only() {
        // Per spec the nanosecond offset adjusts the presentation
        // time; for video that's PTS. DTS (core decode timestamp)
        // is preserved as the raw ms value scaled to ns.
        let tag = VideoTag {
            mod_ex: vec![crate::flv::ModEx::timestamp_offset_nano_entry(123_456)],
            frame_type: VIDEO_FRAME_INTER,
            codec_id: 0,
            avc_packet_type: None,
            composition_time: 0,
            body: b"\x00\x00\x00\x04NALU".to_vec(),
            ex_packet_type: Some(EX_PACKET_TYPE_CODED_FRAMES),
            fourcc: Some(FOURCC_AV1),

            multitrack: None,
        };
        let pkt = video_to_packet(60, &tag);
        // DTS stays on the raw ms grid (in ns units) — 60 ms = 60_000_000 ns.
        assert_eq!(pkt.dts, Some(60_000_000));
        // PTS = DTS + nano_offset (AV1 carries no CTS).
        assert_eq!(pkt.pts, Some(60_123_456));
        assert_eq!(pkt.time_base, RTMP_TIME_BASE);
    }

    #[test]
    fn video_timestamp_offset_nano_stacks_on_cts_and_dts_unchanged() {
        // HEVC × CodedFrames pair carries CTS on the wire — make
        // sure the ns offset stacks on top of (CTS * 1e6) without
        // perturbing DTS.
        let tag = VideoTag {
            mod_ex: vec![crate::flv::ModEx::timestamp_offset_nano_entry(500_000)],
            frame_type: VIDEO_FRAME_INTER,
            codec_id: 0,
            avc_packet_type: None,
            composition_time: 17,
            body: b"\x00\x00\x00\x04NALU".to_vec(),
            ex_packet_type: Some(EX_PACKET_TYPE_CODED_FRAMES),
            fourcc: Some(FOURCC_HEVC),

            multitrack: None,
        };
        let pkt = video_to_packet(200, &tag);
        // DTS = 200 ms * 1e6 ns/ms — no offset.
        assert_eq!(pkt.dts, Some(200_000_000));
        // PTS = (200 + 17) ms * 1e6 + 500_000 ns = 217_500_000 ns.
        assert_eq!(pkt.pts, Some(217_500_000));
    }

    #[test]
    fn video_timestamp_offset_nano_sums_multiple_modex_entries() {
        // The accessor sums every `TimestampOffsetNano` entry in
        // the chain. A non-`TimestampOffsetNano` entry in the middle
        // does not perturb the sum.
        let tag = VideoTag {
            mod_ex: vec![
                crate::flv::ModEx::timestamp_offset_nano_entry(200_000),
                // Unknown / reserved ModEx type — must not feed the sum.
                crate::flv::ModEx {
                    mod_ex_type: 0x0F,
                    data: vec![0xAA, 0xBB, 0xCC],
                },
                crate::flv::ModEx::timestamp_offset_nano_entry(300_000),
            ],
            frame_type: VIDEO_FRAME_KEYFRAME,
            codec_id: 0,
            avc_packet_type: None,
            composition_time: 0,
            body: vec![0x0A],
            ex_packet_type: Some(EX_PACKET_TYPE_CODED_FRAMES),
            fourcc: Some(FOURCC_VP9),

            multitrack: None,
        };
        let pkt = video_to_packet(10, &tag);
        // 10 ms * 1e6 + (200_000 + 300_000) ns = 10_500_000 ns.
        assert_eq!(pkt.pts, Some(10_500_000));
        assert_eq!(pkt.dts, Some(10_000_000));
    }

    #[test]
    fn time_base_is_nanoseconds() {
        // The whole RTMP adapter timeline is 1/1_000_000_000.
        assert_eq!(RTMP_TIME_BASE, TimeBase::new(1, 1_000_000_000));
        assert_eq!(RTMP_MS_TO_NS, 1_000_000);
    }

    #[test]
    fn legacy_avc_seq_header_constant_still_referenced() {
        // Sanity test ensures the `AVC_PACKET_TYPE_SEQUENCE_HEADER`
        // import isn't dropped by a future refactor — the symbol
        // is part of the public re-export chain for downstream
        // codec adapters that hand-craft VideoTag literals.
        let _ = AVC_PACKET_TYPE_SEQUENCE_HEADER;
    }

    #[test]
    fn listen_url_parses_host_port_app_key() {
        let u = ListenUrl::parse("rtmp://127.0.0.1:1935/live/secret").expect("parse");
        assert_eq!(u.bind_addr, "127.0.0.1:1935");
        assert_eq!(u.expected_app, "live");
        assert_eq!(u.expected_stream, "secret");
    }

    #[test]
    fn listen_url_default_port_is_1935() {
        let u = ListenUrl::parse("rtmp://0.0.0.0/live/key").expect("parse");
        assert_eq!(u.bind_addr, "0.0.0.0:1935");
    }

    #[test]
    fn listen_url_accepts_app_only_path() {
        let u = ListenUrl::parse("rtmp://127.0.0.1:1935/live").expect("parse");
        assert_eq!(u.expected_app, "live");
        assert_eq!(u.expected_stream, "");
    }

    #[test]
    fn listen_url_rejects_non_rtmp_scheme() {
        assert!(ListenUrl::parse("http://x/y").is_err());
    }

    #[test]
    fn listen_url_rejects_missing_path() {
        assert!(ListenUrl::parse("rtmp://127.0.0.1:1935").is_err());
    }

    #[test]
    fn flatten_metadata_keeps_scalars_and_drops_objects() {
        let v = Amf0Value::Object(vec![
            ("width".into(), Amf0Value::Number(1280.0)),
            ("height".into(), Amf0Value::Number(720.0)),
            ("encoder".into(), Amf0Value::String("oxideav".into())),
            ("vhost".into(), Amf0Value::Object(vec![])),
            ("live".into(), Amf0Value::Boolean(true)),
        ]);
        let mut out = Vec::new();
        flatten_metadata(&v, &mut out);
        assert_eq!(
            out,
            vec![
                ("width".to_string(), "1280".to_string()),
                ("height".to_string(), "720".to_string()),
                ("encoder".to_string(), "oxideav".to_string()),
                // "vhost" object dropped.
                ("live".to_string(), "true".to_string()),
            ]
        );
    }

    #[test]
    fn rtmp_to_core_err_maps_unexpected_eof_to_eof() {
        let core = rtmp_to_core_err(RtmpError::UnexpectedEof);
        assert!(matches!(core, CoreError::Eof));
    }

    #[test]
    fn rtmp_to_core_err_maps_protocol_violation_to_invalid_data() {
        let core = rtmp_to_core_err(RtmpError::ProtocolViolation("bad chunk size".into()));
        match core {
            CoreError::InvalidData(s) => assert!(s.contains("bad chunk size")),
            _ => panic!("expected InvalidData"),
        }
    }
}
