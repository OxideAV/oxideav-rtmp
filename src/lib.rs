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
//! Out of scope for this release:
//!
//! * RTMPS (TLS). Consumers who need it can wrap our `Read + Write`
//!   with rustls. We may add an `rtmps` feature later.
//! * RTMP play (downstream subscriber / upstream pull). Only publish
//!   direction is implemented.
//! * AMF3, shared objects, RTMFP, RTMP Encrypted, and the Adobe
//!   digest-verified handshake variant.

pub mod adapter;
pub mod amf;
pub mod chunk;
pub mod client;
pub mod error;
pub mod flv;
pub mod handshake;
pub mod message;
pub mod server;

pub use adapter::{
    audio_codec_id, audio_to_packet, open_rtmp, register, video_codec_id, video_to_packet,
    RtmpPacketSource, AUDIO_STREAM_INDEX, RTMP_TIME_BASE, VIDEO_STREAM_INDEX,
};
pub use client::{RtmpClient, RtmpUrl};
pub use error::{Error, Result};
pub use flv::{AudioTag, VideoTag};
pub use server::{PublishRequest, RtmpServer, RtmpSession, StreamPacket};
