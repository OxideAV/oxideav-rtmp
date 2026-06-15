//! Pure-Rust RTMP for oxideav — ingest + push.
//!
//! This crate lets callers:
//!
//! * **Accept** an incoming publisher ([`RtmpServer`]). One TCP
//!   connection per publisher, blocking-thread-per-connection. Server
//!   drives the RTMP handshake + connect + createStream + publish
//!   handshake, then hands the consumer a [`PublishRequest`] carrying
//!   the `app`, `stream_name`, `tc_url`, and `peer_addr` so the
//!   consumer can verify the stream key / do whatever auth they want
//!   before accepting. Accepted publishers become an [`RtmpSession`]
//!   emitting audio / video / metadata packets.
//!
//! * **Push** an encoded stream out to a remote RTMP server
//!   ([`RtmpClient`]). Dial an `rtmp://host[:port]/app/stream_name`
//!   URL, run the client-side publish handshake, then call
//!   `send_video` / `send_audio` / `send_metadata` with H.264 /
//!   AAC payloads.
//!
//! The protocol pieces are reusable on their own —
//! [`amf`], [`chunk`], [`flv`], [`message`], [`handshake`] — for
//! callers who need an RTMP primitive the high-level API doesn't
//! expose yet.
//!
//! AMF support:
//!
//! * [`amf`] — AMF0 wire format (the default for RTMP command +
//!   metadata messages). Used by every commodity ingest endpoint.
//! * [`amf3`] — AMF3 wire format, the Flash Player 9+ binary
//!   serialization. RTMP can switch a channel to AMF3 via the AMF0
//!   `avmplus-object-marker` (0x11) or by using message type IDs 15
//!   (Data), 16 (Shared Object) and 17 (Command). The encoder + decoder
//!   handle all thirteen markers plus the three reference tables
//!   (strings / objects / traits).
//!
//! Out of scope for this release:
//!
//! * RTMPS (TLS). Consumers who need it can wrap our `Read + Write`
//!   with rustls. We may add an `rtmps` feature later.
//! * RTMP play (downstream subscriber / upstream pull). Only publish
//!   direction is implemented.
//! * Shared objects, RTMFP, RTMP Encrypted, and the Adobe
//!   digest-verified handshake variant.

pub mod adapter;
pub mod aggregate;
pub mod amf;
pub mod amf3;
pub mod caps;
pub mod chunk;
pub mod client;
pub mod error;
pub mod flv;
pub mod flv_file;
pub mod handshake;
pub mod message;
pub mod server;

pub use adapter::{
    audio_codec_id, audio_codec_id_for_tag, audio_fourcc_codec_id, audio_to_packet, open_rtmp,
    register, video_codec_id, video_codec_id_for_tag, video_fourcc_codec_id, video_to_packet,
    RtmpPacketSource, AUDIO_STREAM_INDEX, RTMP_MS_TO_NS, RTMP_TIME_BASE, VIDEO_STREAM_INDEX,
};
pub use aggregate::{build_aggregate, parse_aggregate};
pub use amf::Amf0Value;
pub use amf3::Amf3Value;
pub use caps::{
    ConnectCapabilities, FourCcInfoMap, CAPS_EX_MOD_EX, CAPS_EX_MULTITRACK, CAPS_EX_RECONNECT,
    CAPS_EX_TIMESTAMP_NANO_OFFSET, FOURCC_INFO_CAN_DECODE, FOURCC_INFO_CAN_ENCODE,
    FOURCC_INFO_CAN_FORWARD, FOURCC_WILDCARD, OBJECT_ENCODING_AMF0, OBJECT_ENCODING_AMF3,
};
pub use chunk::{Message, MessageStreamKind};
pub use client::{resolve_tc_url, ClientEvent, RtmpClient, RtmpUrl};
pub use error::{Error, Result};
pub use flv::{
    AudioTag, ColorConfig, ColorInfo, HdrCll, HdrMdcv, ModEx, MultichannelConfig,
    MultichannelConfigOrder, Multitrack, MultitrackTrack, VideoTag,
};
pub use flv_file::{FlvHeaderFlags, FlvReader, FlvTag, FlvWriter};
pub use message::{UserControlEvent, RECONNECT_REQUEST_CODE};
pub use server::{PublishRequest, RtmpServer, RtmpSession, StreamPacket};
