//! What a session step can end in besides its reply: the transport, the
//! window, or the server saying no — an `ERROR` frame, typed.

use std::io;

use gsb_protocol::base::{self, ErrorCode};
use prost::Message;

/// A server `ERROR` frame, decoded.
///
/// `code` follows `base.proto`'s forward-compatibility rule: a number
/// this build's `ErrorCode` does not know (and the never-sent `0`) reads
/// as [`ErrorCode::Unspecified`], to be handled like `Other`; `raw` keeps
/// the number as sent, so it can still be reported. Whether the
/// connection survives is NOT inferred from the code — the transport
/// reports that (`Recv::Closed`, or silence on rUDP).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerError {
    pub code: ErrorCode,
    pub raw: i32,
    pub message: String,
}

impl ServerError {
    /// Decode an `ERROR` payload.
    pub fn decode(payload: &[u8]) -> Result<Self, prost::DecodeError> {
        base::Error::decode(payload).map(Self::from)
    }

    /// Decode an `ERROR` payload; an undecodable one reads as an empty
    /// `Unspecified` error (still an error — a client never ignores it).
    pub fn decode_lossy(payload: &[u8]) -> Self {
        Self::decode(payload).unwrap_or_else(|_| base::Error::default().into())
    }
}

impl From<base::Error> for ServerError {
    fn from(e: base::Error) -> Self {
        Self {
            // prost's accessor: an unknown number reads as the default,
            // `Unspecified` — exactly the rule above.
            code: e.code(),
            raw: e.code,
            message: e.message,
        }
    }
}

impl std::fmt::Display for ServerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "server error {} ({}): {}",
            self.raw,
            self.code.as_str_name(),
            self.message
        )
    }
}

/// Why a session step did not get its reply.
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    /// The transport failed (or refused a frame: `InvalidData`).
    #[error("transport: {0}")]
    Io(#[from] io::Error),
    /// The stream ended (EOF) before the reply.
    #[error("the connection ended before the reply")]
    Closed,
    /// No reply within the window.
    #[error("no reply within the window")]
    TimedOut,
    /// The server answered with an `ERROR` frame.
    #[error("{0}")]
    Server(ServerError),
    /// `AUTH_RESULT` came back with `ok = false`.
    #[error("auth refused: {0}")]
    AuthRefused(String),
    /// A base reply whose payload does not decode.
    #[error("undecodable reply (op {op}): {source}")]
    Decode { op: u16, source: prost::DecodeError },
}

impl ClientError {
    /// The server's error, when that is what this is.
    pub fn server(&self) -> Option<&ServerError> {
        match self {
            Self::Server(e) => Some(e),
            _ => None,
        }
    }
}
