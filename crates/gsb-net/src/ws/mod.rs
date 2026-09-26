//! The WebSocket transport (docs/ROADMAP "WebSocket taşıması"): RFC 6455
//! server-side, hand-rolled over the same TCP accept path as
//! [`crate::tcp`], feeding the SAME reader/writer pumps.
//!
//! # Wire contract mapping (the design decision)
//!
//! Every WS **binary** message carries exactly ONE length-prefixed game
//! frame `[u32 LE len][u16 LE op][payload]` — the same envelope
//! `crate::framed` puts on raw TCP. WS message boundaries already delimit
//! payloads, so the inner prefix is technically redundant for framing; it is
//! kept deliberately for *validation symmetry* with tcp.rs: the reader
//! checks the declared length against the actual message (a mismatched or
//! short envelope is a 1007 close, an oversized one a 1009), exactly like
//! the TCP codec rejects bad prefixes. One game frame per message also means
//! a fragmented TCP segment can never silently split a game frame across two
//! WS messages. Text messages are NOT part of the gsb wire contract and are
//! rejected with 1003 (unsupported data) — unless one arrives inside an
//! open fragmented message, where it is first of all an interleaving
//! violation (RFC 6455 §5.4) and fails with 1002 like any data frame there.
//!
//! # Why no `AsyncRead`/`AsyncWrite` adapter (adapter choice)
//!
//! WebSocket is message-oriented; squeezing it through byte-stream traits
//! would need an internal re-buffering layer anyway. Instead the adapter
//! speaks directly at the seam the pump layer already defines
//! (`Stream<Item = io::Result<FrameBody>>` + `Sink<FrameBody>`):
//!
//! ```text
//! [reader pump] ──drives──▶ WsReader ────parses WS frames off OwnedReadHalf,
//!                            │            reassembles messages, maps binary
//!                            │            messages to game frames; pings are
//!                            │            answered by pushing pong onto q_tx
//!                            ▼ q_tx
//!                     ONE bounded mpsc queue ──▶ ws_writer_task owns the
//!                            ▲                   OwnedWriteHalf: the ONLY
//! [writer pump] ─ WsWriter ─┘                   task that writes; sends each
//!   (Sink<FrameBody>)                           queued item as one unmasked
//!                                                FIN WS frame
//! ```
//!
//! Control replies generated on the read path (pongs, close echoes,
//! protocol-failure closes) reach the wire through the same single queue the
//! writer pump feeds — the writer task awaits exactly one source (`recv`),
//! so no multiplexing primitive is needed anywhere. Cost vs tcp.rs: one
//! extra small task per connection (the socket-writer); in exchange the
//! pump layer, framing rules and idle-timeout semantics are reused verbatim.
//!
//! # Handshake
//!
//! `accept` performs the HTTP/1.1 Upgrade itself (8 KiB request-head cap,
//! bounded by [`WS_HANDSHAKE_TIMEOUT`] like tls.rs): GET + `Upgrade:
//! websocket` + `Connection: upgrade` + non-empty `Sec-WebSocket-Key`
//! required; `Sec-WebSocket-Version`, when present, must be 13. Anything
//! else gets an HTTP 400 and a closed socket, mirroring the TLS listener's
//! "failed handshake ⇒ accept error" behavior.

mod frame;
mod handshake;
mod reader;
mod transport;
mod writer;

#[cfg(test)]
mod tests;

pub use transport::{DEFAULT_MAX_MESSAGE_BYTES, WsMessageMapping, WsTransport};

// Re-homed internals, named here so every child reaches them by one path.
use frame::{RawFrame, apply_mask, encode_game_envelope, encode_server_frame};
use handshake::perform_upgrade;
use reader::WsReader;
use writer::{WsOut, WsWriter, spawn_socket_writer};

use std::time::Duration;

/// The RFC 6455 §1.3 magic GUID appended to the client key before hashing.
const MAGIC_GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

/// Maximum HTTP upgrade request head we are willing to buffer (headers
/// included). A browser handshake is a few hundred bytes; anything past
/// this is hostile or lost — reject with 400.
pub const MAX_REQUEST_HEAD_BYTES: usize = 8 * 1024;

/// How long a client may spend in the WS upgrade before the server drops
/// the socket (same rationale as tls.rs's handshake cap: a connection flood
/// of silent clients must not pin accept slots forever).
pub const WS_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// Capacity of the internal outbound queue between the pumps' sink/reader
/// and the single socket-writer task. Deep enough that the writer pump's
/// batch bursts never block on the socket-writer in practice.
const OUT_QUEUE_CAPACITY: usize = 64;

/// Per-read chunk for the WS parser (and the handshake head reader).
const READ_CHUNK: usize = 8 * 1024;

/// RFC 6455 §5.5: control frames carry at most 125 payload bytes.
const MAX_CONTROL_PAYLOAD: usize = 125;

/// RFC 6455 §7.4.1 status 1001 "Going Away": the close code of the
/// server's own teardown close (the connection actor ended the session).
/// The read path's failure closes keep their own codes (1002, 1003,
/// 1007, 1009).
const CLOSE_GOING_AWAY: u16 = 1001;

// Frame opcodes (RFC 6455 §5.2).
const OP_CONT: u8 = 0x0;
const OP_TEXT: u8 = 0x1;
const OP_BIN: u8 = 0x2;
const OP_CLOSE: u8 = 0x8;
const OP_PING: u8 = 0x9;
const OP_PONG: u8 = 0xA;
