//! Single error enum covering every way RTMP goes wrong.

use std::fmt;
use std::io;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug)]
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
