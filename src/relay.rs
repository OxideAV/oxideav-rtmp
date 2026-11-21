//! Late-join support for publisher → subscriber relays.
//!
//! A subscriber that starts playing a live stream part-way through
//! cannot use the packets as they come: the decoder configuration
//! went out once at the start of the publish, the stream metadata
//! likewise, and coded video is only decodable from a key frame on.
//! [`LateJoinCache`] watches the publisher's packets and keeps exactly
//! what a newcomer needs first:
//!
//! * the stored data frames — `onMetaData` and anything else set via
//!   `@setDataFrame`, minus what `@clearDataFrame` revoked (the state
//!   the data-frame convention says a server replays to new
//!   subscribers);
//! * the current decoder configuration of every audio / video track —
//!   legacy AVC / AAC sequence headers, Enhanced RTMP `SequenceStart`
//!   (and MPEG-2 TS sequence start), `MultichannelConfig`, and the
//!   latest video `Metadata` (`colorInfo`) frame;
//! * optionally, everything since the most recent video key frame, so
//!   playback starts immediately instead of at the next key frame.
//!
//! [`LateJoinCache::join`] returns those packets, restamped to the
//! join point, together with a [`KeyframeGate`] that the relay runs
//! every later live packet through for that subscriber: it holds back
//! coded frames until the first video key frame.
//!
//! ```no_run
//! use oxideav_rtmp::relay::LateJoinCache;
//! # fn demo(
//! #     publish: &mut oxideav_rtmp::RtmpSession,
//! #     mut late: oxideav_rtmp::PlaySession,
//! # ) -> oxideav_rtmp::Result<()> {
//! let mut cache = LateJoinCache::new();
//! // … for every packet read from the publisher: cache.observe(&pkt) …
//! # while let Some(pkt) = publish.next_packet()? { cache.observe(&pkt); break; }
//! // A subscriber arrives mid-stream:
//! let (start, mut gate) = cache.join();
//! for pkt in &start {
//!     late.forward(pkt)?;
//! }
//! while let Some(pkt) = publish.next_packet()? {
//!     cache.observe(&pkt);
//!     if gate.admit(&pkt) {
//!         late.forward(&pkt)?;
//!     }
//! }
//! # Ok(())
//! # }
//! ```

use crate::amf::Amf0Value;
use crate::flv::{
    AAC_PACKET_TYPE_SEQUENCE_HEADER, AUDIO_FORMAT_AAC, AUDIO_PACKET_TYPE_MULTICHANNEL_CONFIG,
    AUDIO_PACKET_TYPE_SEQUENCE_START, AVC_PACKET_TYPE_SEQUENCE_HEADER, AudioTag,
    EX_PACKET_TYPE_CODED_FRAMES, EX_PACKET_TYPE_CODED_FRAMES_X, EX_PACKET_TYPE_METADATA,
    EX_PACKET_TYPE_MPEG2TS_SEQUENCE_START, EX_PACKET_TYPE_SEQUENCE_START, VIDEO_CODEC_AVC,
    VIDEO_FRAME_COMMAND, VIDEO_FRAME_KEYFRAME, VideoTag,
};
use crate::server::StreamPacket;

/// What part a media packet plays for a decoder joining late.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Role {
    /// Decoder configuration that stays valid until replaced.
    Config(ConfigKind),
    /// Coded frames (or anything else that is stream content).
    Coded,
    /// Neither: seek-command frames, sequence-end markers.
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConfigKind {
    SequenceStart,
    ChannelConfig,
    ColorMetadata,
}

/// Identity of one cached configuration packet: a newer packet with
/// the same identity replaces the older one.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ConfigKey {
    kind: ConfigKind,
    fourcc: Option<[u8; 4]>,
    legacy_codec: u8,
    /// Track ids (with per-track FourCC) of a Multitrack message;
    /// empty for a single-track packet.
    tracks: Vec<(u8, Option<[u8; 4]>)>,
}

fn video_role(tag: &VideoTag) -> Role {
    if tag.frame_type == VIDEO_FRAME_COMMAND && tag.ex_packet_type.is_none() {
        return Role::Other;
    }
    match tag.ex_packet_type {
        Some(EX_PACKET_TYPE_SEQUENCE_START) | Some(EX_PACKET_TYPE_MPEG2TS_SEQUENCE_START) => {
            Role::Config(ConfigKind::SequenceStart)
        }
        Some(EX_PACKET_TYPE_METADATA) => Role::Config(ConfigKind::ColorMetadata),
        Some(EX_PACKET_TYPE_CODED_FRAMES) | Some(EX_PACKET_TYPE_CODED_FRAMES_X) => {
            if tag.frame_type == VIDEO_FRAME_COMMAND {
                Role::Other
            } else {
                Role::Coded
            }
        }
        Some(_) => Role::Other,
        None => {
            if tag.codec_id == VIDEO_CODEC_AVC {
                match tag.avc_packet_type {
                    Some(AVC_PACKET_TYPE_SEQUENCE_HEADER) => {
                        Role::Config(ConfigKind::SequenceStart)
                    }
                    Some(crate::flv::AVC_PACKET_TYPE_NALU) => Role::Coded,
                    _ => Role::Other,
                }
            } else {
                Role::Coded
            }
        }
    }
}

fn audio_role(tag: &AudioTag) -> Role {
    match tag.ex_packet_type {
        Some(AUDIO_PACKET_TYPE_SEQUENCE_START) => Role::Config(ConfigKind::SequenceStart),
        Some(AUDIO_PACKET_TYPE_MULTICHANNEL_CONFIG) => Role::Config(ConfigKind::ChannelConfig),
        Some(crate::flv::AUDIO_PACKET_TYPE_CODED_FRAMES) => Role::Coded,
        Some(_) => Role::Other,
        None => {
            if tag.sound_format == AUDIO_FORMAT_AAC
                && tag.aac_packet_type == Some(AAC_PACKET_TYPE_SEQUENCE_HEADER)
            {
                Role::Config(ConfigKind::SequenceStart)
            } else {
                Role::Coded
            }
        }
    }
}

fn video_key(tag: &VideoTag, kind: ConfigKind) -> ConfigKey {
    ConfigKey {
        kind,
        fourcc: tag.fourcc,
        legacy_codec: tag.codec_id,
        tracks: tag
            .multitrack
            .as_ref()
            .map(|m| m.tracks.iter().map(|t| (t.track_id, t.fourcc)).collect())
            .unwrap_or_default(),
    }
}

fn audio_key(tag: &AudioTag, kind: ConfigKind) -> ConfigKey {
    ConfigKey {
        kind,
        fourcc: tag.audio_fourcc,
        legacy_codec: tag.sound_format,
        tracks: tag
            .multitrack
            .as_ref()
            .map(|m| m.tracks.iter().map(|t| (t.track_id, t.fourcc)).collect())
            .unwrap_or_default(),
    }
}

fn is_video_keyframe(tag: &VideoTag) -> bool {
    tag.frame_type == VIDEO_FRAME_KEYFRAME && video_role(tag) == Role::Coded
}

fn packet_bytes(pkt: &StreamPacket) -> usize {
    match pkt {
        StreamPacket::Audio { tag, .. } => tag.body.len() + 16,
        StreamPacket::Video { tag, .. } => tag.body.len() + 16,
        _ => 0,
    }
}

/// Publisher-side state a relay replays to subscribers that join a
/// live stream after it started. See the [module docs](self).
#[derive(Debug, Clone, Default)]
pub struct LateJoinCache {
    data_frames: Vec<(String, Amf0Value)>,
    video_config: Vec<(ConfigKey, VideoTag)>,
    audio_config: Vec<(ConfigKey, AudioTag)>,
    /// Packets since the most recent video key frame (inclusive),
    /// when the group cache is enabled and within its byte budget.
    group: Vec<StreamPacket>,
    group_bytes: usize,
    group_limit: usize,
    /// The current group outgrew the budget; nothing is cached until
    /// the next key frame.
    group_overflow: bool,
    seen_video: bool,
    last_timestamp: u32,
}

impl LateJoinCache {
    /// A cache holding metadata and decoder configuration only: a late
    /// subscriber starts at the next video key frame.
    pub fn new() -> Self {
        Self::default()
    }

    /// Also keep every packet since the most recent video key frame,
    /// up to `max_bytes` of payload, so a late subscriber starts
    /// playing at once from that key frame. A group of pictures larger
    /// than the budget is simply not cached (the subscriber then waits
    /// for the next key frame, as without the cache). `0` disables the
    /// group cache.
    pub fn with_group_cache(mut self, max_bytes: usize) -> Self {
        self.group_limit = max_bytes;
        self
    }

    /// Feed every packet read from the publisher, in order.
    pub fn observe(&mut self, pkt: &StreamPacket) {
        match pkt {
            StreamPacket::Metadata(value) => self.store_data_frame("onMetaData", value),
            StreamPacket::DataFrame { handler, value } => self.store_data_frame(handler, value),
            StreamPacket::DataFrameCleared { handler } => {
                self.data_frames.retain(|(h, _)| h != handler);
            }
            StreamPacket::Video { timestamp, tag } => {
                self.last_timestamp = *timestamp;
                match video_role(tag) {
                    Role::Config(kind) => {
                        let key = video_key(tag, kind);
                        match self.video_config.iter_mut().find(|(k, _)| *k == key) {
                            Some((_, stored)) => *stored = tag.clone(),
                            None => self.video_config.push((key, tag.clone())),
                        }
                    }
                    Role::Coded => {
                        self.seen_video = true;
                        if is_video_keyframe(tag) {
                            self.group.clear();
                            self.group_bytes = 0;
                            self.group_overflow = false;
                            self.push_group(pkt);
                        } else if !self.group.is_empty() {
                            self.push_group(pkt);
                        }
                    }
                    Role::Other => {}
                }
            }
            StreamPacket::Audio { timestamp, tag } => {
                self.last_timestamp = *timestamp;
                match audio_role(tag) {
                    Role::Config(kind) => {
                        let key = audio_key(tag, kind);
                        match self.audio_config.iter_mut().find(|(k, _)| *k == key) {
                            Some((_, stored)) => *stored = tag.clone(),
                            None => self.audio_config.push((key, tag.clone())),
                        }
                    }
                    Role::Coded => {
                        if !self.group.is_empty() {
                            self.push_group(pkt);
                        }
                    }
                    Role::Other => {}
                }
            }
            _ => {}
        }
    }

    fn store_data_frame(&mut self, handler: &str, value: &Amf0Value) {
        match self.data_frames.iter_mut().find(|(h, _)| h == handler) {
            Some((_, stored)) => *stored = value.clone(),
            None => self.data_frames.push((handler.to_owned(), value.clone())),
        }
    }

    fn push_group(&mut self, pkt: &StreamPacket) {
        if self.group_limit == 0 || self.group_overflow {
            return;
        }
        let bytes = packet_bytes(pkt);
        if self.group_bytes + bytes > self.group_limit {
            self.group.clear();
            self.group_bytes = 0;
            self.group_overflow = true;
            return;
        }
        self.group_bytes += bytes;
        self.group.push(pkt.clone());
    }

    /// `true` once the publisher has sent coded video: a subscriber
    /// then has to start on a key frame.
    pub fn has_video(&self) -> bool {
        self.seen_video
    }

    /// Timestamp of the most recent media packet observed.
    pub fn last_timestamp(&self) -> u32 {
        self.last_timestamp
    }

    /// The stored data frames (`onMetaData` first-seen order).
    pub fn data_frames(&self) -> &[(String, Amf0Value)] {
        &self.data_frames
    }

    /// Number of cached packets of the current group of pictures.
    pub fn group_len(&self) -> usize {
        self.group.len()
    }

    /// What to send a subscriber that joins now, in order — stored
    /// data frames, then every track's decoder configuration, then the
    /// cached group of pictures if there is one — and the gate to run
    /// that subscriber's later live packets through.
    ///
    /// Configuration packets are stamped with the timestamp playback
    /// starts at (the cached key frame, or the latest packet seen), so
    /// the subscriber's timeline never steps backwards.
    pub fn join(&self) -> (Vec<StreamPacket>, KeyframeGate) {
        let start_ts = match self.group.first() {
            Some(StreamPacket::Video { timestamp, .. }) => *timestamp,
            _ => self.last_timestamp,
        };
        let mut out = Vec::with_capacity(
            self.data_frames.len()
                + self.video_config.len()
                + self.audio_config.len()
                + self.group.len(),
        );
        for (handler, value) in &self.data_frames {
            if handler == "onMetaData" {
                out.push(StreamPacket::Metadata(value.clone()));
            } else {
                out.push(StreamPacket::DataFrame {
                    handler: handler.clone(),
                    value: value.clone(),
                });
            }
        }
        for (_, tag) in &self.video_config {
            out.push(StreamPacket::Video {
                timestamp: start_ts,
                tag: tag.clone(),
            });
        }
        for (_, tag) in &self.audio_config {
            out.push(StreamPacket::Audio {
                timestamp: start_ts,
                tag: tag.clone(),
            });
        }
        out.extend(self.group.iter().cloned());
        let open = !self.seen_video || !self.group.is_empty();
        (out, KeyframeGate { open })
    }
}

/// Per-subscriber filter for the live packets that follow a
/// [`LateJoinCache::join`]: coded audio and video are held back until
/// the first video key frame, so the subscriber's decoder never sees
/// a frame it cannot decode. Metadata, data frames and decoder
/// configuration always pass. The gate starts open when the stream
/// has no video or the join already replayed a group of pictures.
#[derive(Debug, Clone)]
pub struct KeyframeGate {
    open: bool,
}

impl KeyframeGate {
    /// A closed gate for a stream known to carry video.
    pub fn closed() -> Self {
        KeyframeGate { open: false }
    }

    /// `true` once coded frames are flowing.
    pub fn is_open(&self) -> bool {
        self.open
    }

    /// Whether `pkt` should be forwarded to this subscriber.
    pub fn admit(&mut self, pkt: &StreamPacket) -> bool {
        if self.open {
            return true;
        }
        match pkt {
            StreamPacket::Video { tag, .. } => match video_role(tag) {
                Role::Config(_) => true,
                Role::Coded => {
                    if is_video_keyframe(tag) {
                        self.open = true;
                    }
                    self.open
                }
                Role::Other => false,
            },
            StreamPacket::Audio { tag, .. } => matches!(audio_role(tag), Role::Config(_)),
            _ => true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn avc(ts: u32, frame_type: u8, packet_type: u8, body: &[u8]) -> StreamPacket {
        StreamPacket::Video {
            timestamp: ts,
            tag: VideoTag::default()
                .with_frame_type(frame_type)
                .with_codec_id(VIDEO_CODEC_AVC)
                .with_avc_packet_type(Some(packet_type))
                .with_body(body.to_vec()),
        }
    }

    fn aac(ts: u32, packet_type: u8, body: &[u8]) -> StreamPacket {
        StreamPacket::Audio {
            timestamp: ts,
            tag: AudioTag::default()
                .with_sound_format(AUDIO_FORMAT_AAC)
                .with_sound_rate(3)
                .with_sound_size_16bit(true)
                .with_stereo(true)
                .with_aac_packet_type(Some(packet_type))
                .with_body(body.to_vec()),
        }
    }

    fn shape(pkts: &[StreamPacket]) -> Vec<String> {
        pkts.iter()
            .map(|p| match p {
                StreamPacket::Metadata(_) => "meta".to_string(),
                StreamPacket::DataFrame { handler, .. } => format!("data:{handler}"),
                StreamPacket::Video { timestamp, tag } => {
                    format!("v{}@{}:{:?}", tag.frame_type, timestamp, tag.body)
                }
                StreamPacket::Audio { timestamp, tag } => {
                    format!("a@{}:{:?}", timestamp, tag.body)
                }
                _ => "other".to_string(),
            })
            .collect()
    }

    fn feed(cache: &mut LateJoinCache) {
        let meta = Amf0Value::EcmaArray(vec![("width".into(), Amf0Value::Number(320.0))]);
        cache.observe(&StreamPacket::Metadata(meta));
        cache.observe(&avc(0, 1, 0, &[1, 100]));
        cache.observe(&aac(0, 0, &[0x12, 0x10]));
        cache.observe(&avc(0, 1, 1, &[10]));
        cache.observe(&aac(0, 1, &[20]));
        cache.observe(&avc(40, 2, 1, &[11]));
        cache.observe(&aac(23, 1, &[21]));
        cache.observe(&avc(80, 1, 1, &[12]));
        cache.observe(&aac(46, 1, &[22]));
        cache.observe(&avc(120, 2, 1, &[13]));
    }

    #[test]
    fn join_without_group_cache_sends_config_and_waits_for_a_keyframe() {
        let mut cache = LateJoinCache::new();
        feed(&mut cache);
        assert!(cache.has_video());
        let (start, mut gate) = cache.join();
        assert_eq!(
            shape(&start),
            vec!["meta", "v1@120:[1, 100]", "a@120:[18, 16]"],
            "metadata, then both sequence headers stamped at the join point"
        );
        assert!(!gate.is_open());
        // Live packets after the join: inter frames and audio are held
        // back, a fresh sequence header passes, the key frame opens.
        assert!(!gate.admit(&avc(160, 2, 1, &[14])));
        assert!(!gate.admit(&aac(69, 1, &[23])));
        assert!(gate.admit(&avc(200, 1, 0, &[1, 101])));
        assert!(gate.admit(&StreamPacket::Metadata(Amf0Value::Null)));
        assert!(gate.admit(&avc(200, 1, 1, &[15])));
        assert!(gate.is_open());
        assert!(gate.admit(&avc(240, 2, 1, &[16])));
        assert!(gate.admit(&aac(92, 1, &[24])));
    }

    #[test]
    fn join_with_group_cache_replays_from_the_last_keyframe() {
        let mut cache = LateJoinCache::new().with_group_cache(1 << 20);
        feed(&mut cache);
        let (start, gate) = cache.join();
        assert_eq!(
            shape(&start),
            vec![
                "meta",
                "v1@80:[1, 100]",
                "a@80:[18, 16]",
                "v1@80:[12]",
                "a@46:[22]",
                "v2@120:[13]",
            ]
        );
        assert!(gate.is_open(), "the replayed group starts on a key frame");
    }

    #[test]
    fn oversize_group_is_not_cached() {
        let mut cache = LateJoinCache::new().with_group_cache(64);
        cache.observe(&avc(0, 1, 0, &[1]));
        cache.observe(&avc(0, 1, 1, &[0; 20]));
        assert_eq!(cache.group_len(), 1);
        cache.observe(&avc(40, 2, 1, &[0; 40]));
        assert_eq!(cache.group_len(), 0, "budget crossed: group dropped");
        cache.observe(&avc(80, 2, 1, &[0; 1]));
        assert_eq!(cache.group_len(), 0, "stays empty until the next key frame");
        let (start, gate) = cache.join();
        assert_eq!(start.len(), 1);
        assert!(!gate.is_open());
        cache.observe(&avc(120, 1, 1, &[0; 8]));
        assert_eq!(cache.group_len(), 1);
    }

    #[test]
    fn newer_configuration_replaces_older_and_cleared_frames_vanish() {
        let mut cache = LateJoinCache::new();
        cache.observe(&avc(0, 1, 0, &[1]));
        cache.observe(&avc(500, 1, 0, &[2]));
        cache.observe(&StreamPacket::DataFrame {
            handler: "onCuePoint".into(),
            value: Amf0Value::Number(1.0),
        });
        cache.observe(&StreamPacket::DataFrameCleared {
            handler: "onCuePoint".into(),
        });
        let (start, _) = cache.join();
        assert_eq!(shape(&start), vec!["v1@500:[2]"]);
        assert!(cache.data_frames().is_empty());
    }

    #[test]
    fn audio_only_stream_needs_no_keyframe() {
        let mut cache = LateJoinCache::new();
        cache.observe(&aac(0, 0, &[0x12, 0x10]));
        cache.observe(&aac(0, 1, &[1]));
        cache.observe(&aac(23, 1, &[2]));
        let (start, mut gate) = cache.join();
        assert_eq!(shape(&start), vec!["a@23:[18, 16]"]);
        assert!(gate.is_open());
        assert!(gate.admit(&aac(46, 1, &[3])));
    }

    #[test]
    fn enhanced_sequence_start_and_color_metadata_are_configuration() {
        let seq = VideoTag::default()
            .with_frame_type(1)
            .with_ex_packet_type(Some(EX_PACKET_TYPE_SEQUENCE_START))
            .with_fourcc(Some(*b"hvc1"))
            .with_body(vec![9, 9]);
        let color = VideoTag::default()
            .with_frame_type(5)
            .with_ex_packet_type(Some(EX_PACKET_TYPE_METADATA))
            .with_fourcc(Some(*b"hvc1"))
            .with_body(vec![7]);
        let key = VideoTag::default()
            .with_frame_type(1)
            .with_ex_packet_type(Some(EX_PACKET_TYPE_CODED_FRAMES))
            .with_fourcc(Some(*b"hvc1"))
            .with_body(vec![1]);
        let inter = key.clone().with_frame_type(2);
        assert_eq!(video_role(&seq), Role::Config(ConfigKind::SequenceStart));
        assert_eq!(video_role(&color), Role::Config(ConfigKind::ColorMetadata));
        assert!(is_video_keyframe(&key));
        assert!(!is_video_keyframe(&inter));
        assert!(!is_video_keyframe(&seq));
        let mut cache = LateJoinCache::new();
        for (ts, tag) in [(0, &seq), (0, &color), (0, &key), (33, &inter)] {
            cache.observe(&StreamPacket::Video {
                timestamp: ts,
                tag: tag.clone(),
            });
        }
        let (start, gate) = cache.join();
        assert_eq!(start.len(), 2, "sequence start + colour metadata");
        assert!(!gate.is_open());
    }
}
