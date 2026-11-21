//! Single error enum covering every way RTMP goes wrong.

use std::fmt;
use std::io;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug)]
#[non_exhaustive]
pub enum Error {
    /// Underlying TCP / read / write I/O failure.
    Io(io::Error),
    /// Peer closed the connection mid-protocol.
    UnexpectedEof,
    /// Handshake byte 0 was not the expected version 3.
    UnsupportedHandshakeVersion(u8),
    /// Saw bytes that don't match any valid AMF0 marker.
    InvalidAmf0(String),
    /// Saw a chunk message header shape or reserved value we don't
    /// know how to interpret.
    InvalidChunk(String),
    /// Command message arrived without the expected name / transaction
    /// id / args (e.g. `connect` missing its command object).
    InvalidCommand(String),
    /// A sub-protocol field (window ack size, chunk size, peer
    /// bandwidth) carries a value outside its legal range.
    ProtocolViolation(String),
    /// Consumer rejected a publish attempt via `PublishRequest::reject`.
    /// The string is the reason passed to `onStatus`.
    Rejected(String),
    /// We waited for a specific message and the deadline elapsed.
    Timeout,
    /// Generic bucket for situations that don't deserve a dedicated
    /// variant (e.g. malformed AVC sequence header payload).
    Other(String),
}

/// Why a session's receive side stopped. Every packet pump in the
/// crate ([`RtmpSession::next_packet`](crate::RtmpSession::next_packet),
/// [`PlaySession::next_event`](crate::PlaySession::next_event),
/// [`RtmpPlayer::next_packet`](crate::RtmpPlayer::next_packet),
/// [`RtmpClient::poll_event`](crate::RtmpClient::poll_event)) reports
/// the end of the stream as `Ok(None)`; the matching `end_reason()`
/// accessor then tells an orderly end from a lost connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum StreamEnd {
    /// The peer ended the stream at the protocol level: a
    /// `closeStream` / `deleteStream` / `FCUnpublish` / NetConnection
    /// `close` command, or a User Control `StreamEOF` event.
    Signalled,
    /// The peer closed the TCP connection between two messages
    /// without a protocol-level goodbye.
    PeerClosed,
    /// The TCP connection closed in the middle of a chunk or a
    /// multi-chunk message: the stream is incomplete.
    Truncated,
    /// The connection was reset or aborted; data the peer sent last
    /// may never have arrived.
    ConnectionReset,
}

impl StreamEnd {
    /// `true` when everything the peer sent before the end was
    /// received ([`Signalled`](Self::Signalled) or
    /// [`PeerClosed`](Self::PeerClosed)).
    pub fn is_complete(self) -> bool {
        matches!(self, StreamEnd::Signalled | StreamEnd::PeerClosed)
    }
}

/// Classify a read error that ends a session's receive side. `idle`
/// is the chunk reader's message-boundary flag at the time of the
/// error. `None` for errors that are not an end of stream.
pub(crate) fn read_end(e: &Error, idle: bool) -> Option<StreamEnd> {
    match e {
        Error::UnexpectedEof => Some(if idle {
            StreamEnd::PeerClosed
        } else {
            StreamEnd::Truncated
        }),
        Error::Io(io) => match io.kind() {
            io::ErrorKind::UnexpectedEof => Some(if idle {
                StreamEnd::PeerClosed
            } else {
                StreamEnd::Truncated
            }),
            io::ErrorKind::ConnectionReset | io::ErrorKind::ConnectionAborted => {
                Some(StreamEnd::ConnectionReset)
            }
            _ => None,
        },
        _ => None,
    }
}

/// Map a socket read / write timeout onto [`Error::Timeout`].
pub(crate) fn timeout_to_error(e: Error) -> Error {
    match e {
        Error::Io(io)
            if matches!(
                io.kind(),
                io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
            ) =>
        {
            Error::Timeout
        }
        other => other,
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "rtmp i/o: {e}"),
            Error::UnexpectedEof => write!(f, "rtmp: peer closed connection"),
            Error::UnsupportedHandshakeVersion(v) => {
                write!(f, "rtmp handshake: unsupported version byte {v:#x}")
            }
            Error::InvalidAmf0(m) => write!(f, "rtmp amf0: {m}"),
            Error::InvalidChunk(m) => write!(f, "rtmp chunk: {m}"),
            Error::InvalidCommand(m) => write!(f, "rtmp command: {m}"),
            Error::ProtocolViolation(m) => write!(f, "rtmp protocol: {m}"),
            Error::Rejected(reason) => write!(f, "rtmp: publish rejected: {reason}"),
            Error::Timeout => write!(f, "rtmp: timeout"),
            Error::Other(m) => write!(f, "rtmp: {m}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Error::Io(e)
    }
}
