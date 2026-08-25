//! The WebSocket transport (docs/ROADMAP "WebSocket taşıması"): RFC 6455
//! server-side, hand-rolled over the same TCP accept path as
//! [`crate::tcp`], feeding the SAME reader/writer pumps.
//!
//! # Wire contract mapping (the design decision)
//!
//! Every WS **binary** message carries exactly ONE length-prefixed game
//! frame `[u32 LE len][u16 LE op][payload]` — the same envelope
//! [`crate::framed`] puts on raw TCP. WS message boundaries already delimit
//! payloads, so the inner prefix is technically redundant for framing; it is
//! kept deliberately for *validation symmetry* with tcp.rs: the reader
//! checks the declared length against the actual message (a mismatched or
//! short envelope is a 1007 close, an oversized one a 1009), exactly like
//! the TCP codec rejects bad prefixes. One game frame per message also means
//! a fragmented TCP segment can never silently split a game frame across two
//! WS messages. Text messages are NOT part of the gsb wire contract and are
//! rejected with 1003 (unsupported data).
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

use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::task::Context;
use std::task::Poll;
use std::task::ready;
use std::time::Duration;

use bytes::Bytes;
use bytes::BytesMut;
use futures::Sink;
use futures::Stream;
use sha1::Digest;
use sha1::Sha1;
use tokio::io::AsyncRead;
use tokio::io::AsyncWriteExt;
use tokio::io::ReadBuf;
use tokio::net::TcpListener;
use tokio::net::TcpStream;
use tokio::net::tcp::OwnedReadHalf;
use tokio::net::tcp::OwnedWriteHalf;
use tokio::sync::mpsc;
use tracing::debug;
use tracing::warn;

use gsb_core::channel::FrameBatch;
use gsb_core::channel::Inbox;
use gsb_core::channel::Mailbox;
use gsb_core::conn::ConnIn;
use gsb_core::id::ConnectionId;
use gsb_protocol::FrameBody;

use crate::pump::spawn_pumps;
use crate::transport::BoxFuture;
use crate::transport::Endpoint;
use crate::transport::Listener;
use crate::transport::Transport;

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

// Frame opcodes (RFC 6455 §5.2).
const OP_CONT: u8 = 0x0;
const OP_TEXT: u8 = 0x1;
const OP_BIN: u8 = 0x2;
const OP_CLOSE: u8 = 0x8;
const OP_PING: u8 = 0x9;
const OP_PONG: u8 = 0xA;

// ── base64 + accept key ─────────────────────────────────────────────────

/// Standard-alphabet base64 with padding, hand-rolled (~15 lines) so the
/// handshake needs no extra dependency. Only used for the 20-byte SHA-1
/// digest and unit-tested against the RFC 4648 vectors.
fn base64_encode(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b1 = chunk[0] as u32;
        let b2 = *chunk.get(1).unwrap_or(&0) as u32;
        let b3 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b1 << 16) | (b2 << 8) | b3;
        out.push(ALPHABET[(n >> 18 & 63) as usize] as char);
        out.push(ALPHABET[(n >> 12 & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(n >> 6 & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

/// `Sec-WebSocket-Accept` = base64(SHA-1(key + MAGIC_GUID)) (RFC 6455 §4.2.2).
fn accept_key(client_key: &[u8]) -> String {
    let mut hasher = Sha1::new();
    hasher.update(client_key);
    hasher.update(MAGIC_GUID.as_bytes());
    base64_encode(&hasher.finalize())
}

// ── HTTP upgrade handshake ───────────────────────────────────────────────

/// Perform the server side of the RFC 6455 opening handshake on `stream`.
///
/// On success the stream is positioned right after the 101 response: only
/// WebSocket frames follow. On any failure a best-effort HTTP 400 has been
/// written and the error returned (the caller drops the socket, like the
/// TLS listener does for failed handshakes).
async fn perform_upgrade(mut stream: TcpStream) -> io::Result<TcpStream> {
    let head = match read_request_head(&mut stream).await {
        Ok(head) => head,
        Err(e) => return reject_and_fail(stream, e).await,
    };
    let key = match parse_upgrade_request(&head) {
        Ok(key) => key,
        Err(why) => {
            return reject_and_fail(stream, io::Error::new(io::ErrorKind::InvalidData, why)).await;
        }
    };
    let response = format!(
        "HTTP/1.1 101 Switching Protocols\r\n\
         Upgrade: websocket\r\n\
         Connection: Upgrade\r\n\
         Sec-WebSocket-Accept: {}\r\n\
         \r\n",
        accept_key(key.as_bytes())
    );
    stream.write_all(response.as_bytes()).await?;
    stream.flush().await?;
    Ok(stream)
}

/// Write an HTTP 400 with a short plain-text reason, then surface `e`.
async fn reject_and_fail(mut stream: TcpStream, e: io::Error) -> io::Result<TcpStream> {
    let body = format!("WebSocket handshake rejected: {}\n", e);
    let response = format!(
        "HTTP/1.1 400 Bad Request\r\n\
         Connection: close\r\n\
         Content-Type: text/plain; charset=utf-8\r\n\
         Content-Length: {}\r\n\
         \r\n{}",
        body.len(),
        body
    );
    // Best effort: the diagnostic matters more than write errors here.
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.flush().await;
    Err(e)
}

/// Read bytes until the `\r\n\r\n` end-of-head marker (or the cap).
async fn read_request_head(stream: &mut TcpStream) -> io::Result<String> {
    let mut buf = BytesMut::with_capacity(1024);
    loop {
        if let Some(end) = find_head_end(&buf) {
            return Ok(String::from_utf8_lossy(&buf[..end]).into_owned());
        }
        if buf.len() >= MAX_REQUEST_HEAD_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("request head exceeds {} bytes", MAX_REQUEST_HEAD_BYTES),
            ));
        }
        let n = tokio::io::AsyncReadExt::read_buf(stream, &mut buf).await?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "connection closed before the request head was complete",
            ));
        }
    }
}

/// Offset of the first `\r\n\r\n` in `buf`, if present.
fn find_head_end(buf: &[u8]) -> Option<usize> {
    if buf.len() < 4 {
        return None;
    }
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

/// Validate the upgrade request and return the client's
/// `Sec-WebSocket-Key`. Strictness matches the module docs: method GET,
/// HTTP/1.1, `Upgrade: websocket`, `Connection: upgrade`, a sane key, and
/// version 13 *if the header is sent at all*. Any path ("/") is accepted —
/// path-based routing is not this transport's job.
fn parse_upgrade_request(head: &str) -> Result<String, String> {
    let mut lines = head.split("\r\n");
    let request_line = lines.next().unwrap_or("");
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("");
    let _path = parts.next().unwrap_or("");
    let version = parts.next().unwrap_or("");
    if method != "GET" {
        return Err(format!("expected GET, got `{method}`"));
    }
    if version != "HTTP/1.1" {
        return Err(format!("expected HTTP/1.1, got `{version}`"));
    }

    let mut upgrade: Option<&str> = None;
    let mut connection: Option<&str> = None;
    let mut key: Option<&str> = None;
    let mut ws_version: Option<&str> = None;
    for line in lines {
        if line.is_empty() {
            continue;
        }
        let Some((name, value)) = line.split_once(':') else {
            return Err(format!("malformed header line `{line}`"));
        };
        match name.trim().to_ascii_lowercase().as_str() {
            "upgrade" => upgrade = Some(value.trim()),
            "connection" => connection = Some(value.trim()),
            "sec-websocket-key" => key = Some(value.trim()),
            "sec-websocket-version" => ws_version = Some(value.trim()),
            _ => {}
        }
    }

    let Some(upgrade) = upgrade else {
        return Err("missing Upgrade header".into());
    };
    if !upgrade.eq_ignore_ascii_case("websocket") {
        return Err(format!("Upgrade is `{upgrade}`, want websocket"));
    }
    let Some(connection) = connection else {
        return Err("missing Connection header".into());
    };
    let upgrades_conn = connection
        .split(',')
        .any(|token| token.trim().eq_ignore_ascii_case("upgrade"));
    if !upgrades_conn {
        return Err(format!(
            "Connection is `{connection}`, want the upgrade token"
        ));
    }
    let Some(key) = key else {
        return Err("missing Sec-WebSocket-Key".into());
    };
    if key.is_empty() || key.len() > 128 || !key.bytes().all(|b| b.is_ascii_graphic()) {
        return Err("Sec-WebSocket-Key is empty or malformed".into());
    }
    if let Some(v) = ws_version.filter(|v| v.trim() != "13") {
        return Err(format!("unsupported Sec-WebSocket-Version `{v}`, want 13"));
    }
    Ok(key.to_owned())
}

// ── WS frame primitives ──────────────────────────────────────────────────

/// XOR `payload` in place with the 4-byte mask (used both directions in the
/// tests; on the server read path it unmasks client frames).
fn apply_mask(payload: &mut [u8], key: [u8; 4]) {
    for (i, b) in payload.iter_mut().enumerate() {
        *b ^= key[i & 3];
    }
}

/// Encode ONE complete server-to-client frame: FIN set, never masked
/// (RFC 6455 §5.1: a server MUST NOT mask). Lengths ≥ 126/65536 use the
/// 16-bit / 64-bit extended forms (network byte order).
fn encode_server_frame(opcode: u8, payload: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(10 + payload.len());
    frame.push(0x80 | opcode);
    if payload.len() < 126 {
        frame.push(payload.len() as u8);
    } else if payload.len() <= u16::MAX as usize {
        frame.push(126);
        frame.extend_from_slice(&(payload.len() as u16).to_be_bytes());
    } else {
        frame.push(127);
        frame.extend_from_slice(&(payload.len() as u64).to_be_bytes());
    }
    frame.extend_from_slice(payload);
    frame
}

/// Wrap a game frame into its WS-message body:
/// `[u32 LE len][body]` where body = `[u16 LE op][payload]` — the exact
/// envelope `crate::framed` puts on raw TCP (see the module docs).
fn encode_game_envelope(frame: &FrameBody) -> Bytes {
    let body = frame.encode();
    let mut out = BytesMut::with_capacity(4 + body.len());
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(&body);
    out.freeze()
}

/// A fully received (already unmasked) WS frame from the client.
struct RawFrame {
    fin: bool,
    opcode: u8,
    payload: Vec<u8>,
}

// ── the read half: WS frames → game frames ──────────────────────────────

/// What one [`WsReader::step`] made of the buffered bytes.
enum Step {
    /// Need more bytes off the socket before another frame can be parsed.
    NeedData,
    /// Progress was made (pong sent, fragment accumulated); keep stepping.
    Continue,
    /// One game frame is ready for the pump.
    Yield(FrameBody),
    /// The peer initiated the close handshake (echo already queued).
    Done,
}

/// The pump-facing reader: parses WebSocket frames off the socket's read
/// half and yields game frames. Also generates control replies (pongs,
/// close echoes, protocol-failure closes) by pushing them onto the shared
/// outbound queue — see the module docs for why that queue exists.
struct WsReader {
    sock: OwnedReadHalf,
    scratch: Vec<u8>,
    buf: BytesMut,
    max_message_bytes: usize,
    ctrl: mpsc::Sender<WsOut>,
    /// Set as soon as this connection has queued a close frame, so the
    /// writer pump's teardown never emits a second one.
    closing: Arc<AtomicBool>,
    /// Opcode of the data message being reassembled (`None` = none).
    frag_opcode: Option<u8>,
    frag_data: BytesMut,
}

impl WsReader {
    fn new(
        sock: OwnedReadHalf,
        max_message_bytes: usize,
        ctrl: mpsc::Sender<WsOut>,
        closing: Arc<AtomicBool>,
    ) -> Self {
        Self {
            sock,
            scratch: vec![0u8; READ_CHUNK],
            buf: BytesMut::with_capacity(READ_CHUNK),
            max_message_bytes,
            ctrl,
            closing,
            frag_opcode: None,
            frag_data: BytesMut::new(),
        }
    }

    /// Fail the WebSocket connection: queue a close frame carrying `code`
    /// (best effort — poll context means `try_send`; a full queue drops the
    /// notice, teardown follows regardless), then hand the pump an error.
    fn proto_fail(&mut self, code: u16, why: impl std::fmt::Display) -> io::Error {
        self.closing.store(true, Ordering::SeqCst);
        let _ = self
            .ctrl
            .try_send(WsOut::Control(OP_CLOSE, code.to_be_bytes().to_vec()));
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("websocket protocol violation ({code}): {why}"),
        )
    }

    /// Try to take one complete (unmasked) frame off the front of `buf`.
    /// `Ok(None)` = incomplete, wait for more bytes. Protocol violations
    /// fail the connection immediately — including an oversized declared
    /// length, which is rejected BEFORE its payload is waited for.
    fn next_frame(&mut self) -> io::Result<Option<RawFrame>> {
        if self.buf.len() < 2 {
            return Ok(None);
        }
        let b0 = self.buf[0];
        let b1 = self.buf[1];
        let fin = b0 & 0x80 != 0;
        if b0 & 0x70 != 0 {
            return Err(self.proto_fail(1002, "RSV bits set but no extension was negotiated"));
        }
        let opcode = b0 & 0x0f;
        // Client-to-server frames MUST be masked (RFC 6455 §5.1). Enforced
        // before the length even arrives: fail fast, allocate never.
        if b1 & 0x80 == 0 {
            return Err(self.proto_fail(1002, "client-to-server frame is not masked"));
        }
        let l7 = (b1 & 0x7f) as usize;
        let mut off = 2usize;
        let len = match l7 {
            0x7e => {
                if self.buf.len() < 4 {
                    return Ok(None);
                }
                off = 4;
                u16::from_be_bytes([self.buf[2], self.buf[3]]) as usize
            }
            0x7f => {
                if self.buf.len() < 10 {
                    return Ok(None);
                }
                let mut raw = [0u8; 8];
                raw.copy_from_slice(&self.buf[2..10]);
                off = 10;
                let raw = u64::from_be_bytes(raw);
                if raw > self.max_message_bytes as u64 {
                    return Err(self.proto_fail(
                        1009,
                        format_args!("frame declares {raw} bytes, over the size ceiling"),
                    ));
                }
                raw as usize
            }
            n => n,
        };
        if len > self.max_message_bytes {
            return Err(self.proto_fail(
                1009,
                format_args!("frame declares {len} bytes, over the size ceiling"),
            ));
        }
        let mask_at = off;
        off += 4;
        if self.buf.len() < off + len {
            self.buf
                .reserve((off + len - self.buf.len()).min(READ_CHUNK));
            return Ok(None);
        }
        let mut whole = self.buf.split_to(off + len);
        let key = [
            whole[mask_at],
            whole[mask_at + 1],
            whole[mask_at + 2],
            whole[mask_at + 3],
        ];
        // split_off(off): everything from `off` on is the masked payload.
        let mut payload = whole.split_off(off).to_vec();
        apply_mask(&mut payload, key);
        Ok(Some(RawFrame {
            fin,
            opcode,
            payload,
        }))
    }

    /// Map one assembled data message onto the wire contract: exactly one
    /// length-prefixed game frame per binary message.
    fn deliver(&mut self, msg: Bytes) -> io::Result<Step> {
        if msg.len() < 4 {
            return Err(self.proto_fail(
                1007,
                format_args!(
                    "binary message of {} bytes is shorter than the envelope",
                    msg.len()
                ),
            ));
        }
        let declared = u32::from_le_bytes([msg[0], msg[1], msg[2], msg[3]]) as usize;
        if declared < 2 {
            return Err(self.proto_fail(1007, "envelope body below the 2-byte minimum"));
        }
        if msg.len() != 4 + declared {
            return Err(self.proto_fail(
                1007,
                format_args!(
                    "envelope declares {declared} body bytes in a {}-byte message: \
                     exactly one game frame per message is the contract",
                    msg.len()
                ),
            ));
        }
        let frame = FrameBody::decode(msg.slice(4..))
            .map_err(|e| self.proto_fail(1007, format_args!("bad game frame body: {e}")))?;
        Ok(Step::Yield(frame))
    }

    /// Consume the next complete frame (waiting is the caller's job via
    /// [`Step::NeedData`]) and update assembly/control state.
    fn step(&mut self) -> io::Result<Step> {
        let Some(frame) = self.next_frame()? else {
            return Ok(Step::NeedData);
        };
        let control = matches!(frame.opcode, OP_CLOSE | OP_PING | OP_PONG);
        if control && (!frame.fin || frame.payload.len() > MAX_CONTROL_PAYLOAD) {
            return Err(self.proto_fail(
                1002,
                "control frames must be unfragmented and carry at most 125 bytes",
            ));
        }
        match frame.opcode {
            OP_CONT => {
                if self.frag_opcode.is_none() {
                    return Err(self.proto_fail(1002, "continuation frame with no message open"));
                }
                self.frag_data.extend_from_slice(&frame.payload);
                if self.frag_data.len() > self.max_message_bytes {
                    return Err(
                        self.proto_fail(1009, "reassembled message exceeds the size ceiling")
                    );
                }
                if frame.fin {
                    let data = std::mem::take(&mut self.frag_data);
                    self.frag_opcode = None;
                    self.deliver(data.freeze())
                } else {
                    Ok(Step::Continue)
                }
            }
            // Text is rejected whether fragmented or not: the gsb wire
            // contract has no textual frames (module docs).
            OP_TEXT => {
                Err(self.proto_fail(1003, "text messages are not part of the wire contract"))
            }
            OP_BIN => {
                if frame.fin {
                    self.deliver(Bytes::from(frame.payload))
                } else {
                    self.frag_opcode = Some(OP_BIN);
                    self.frag_data.clear();
                    self.frag_data.extend_from_slice(&frame.payload);
                    Ok(Step::Continue)
                }
            }
            OP_CLOSE => {
                // Echo the peer's status code (empty close echoes empty).
                let echo = match frame.payload.as_slice() {
                    [] => Vec::new(),
                    [hi] => {
                        return Err(self.proto_fail(
                            1002,
                            format_args!(
                                "close payload must be empty or 2 bytes, got 1 ({hi:#04x})"
                            ),
                        ));
                    }
                    [hi, lo, ..] => vec![*hi, *lo],
                };
                self.closing.store(true, Ordering::SeqCst);
                let _ = self.ctrl.try_send(WsOut::Control(OP_CLOSE, echo));
                // RFC 6455 §7.1.1: after echoing, the server closes first —
                // tell the writer task to drop the socket now instead of
                // waiting for the actor layer's teardown.
                let _ = self.ctrl.try_send(WsOut::Shutdown);
                Ok(Step::Done)
            }
            OP_PING => {
                // §5.5.3: pong carries the ping's application data back.
                let _ = self.ctrl.try_send(WsOut::Control(OP_PONG, frame.payload));
                Ok(Step::Continue)
            }
            OP_PONG => Ok(Step::Continue), // unsolicited pongs: ignore
            other => Err(self.proto_fail(1002, format_args!("unknown opcode {other:#04x}"))),
        }
    }

    /// Pull one read's worth of bytes into `buf`; `Ok(false)` = clean EOF
    /// at a frame boundary (stream over), `Err` = truncated mid-frame.
    fn fill(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<bool>> {
        let n = {
            let mut rb = ReadBuf::new(&mut self.scratch);
            match Pin::new(&mut self.sock).poll_read(cx, &mut rb) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                Poll::Ready(Ok(())) => rb.filled().len(),
            }
        };
        if n == 0 {
            let mid_frame =
                !self.buf.is_empty() || self.frag_opcode.is_some() || !self.frag_data.is_empty();
            if mid_frame {
                return Poll::Ready(Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "connection closed mid-frame",
                )));
            }
            return Poll::Ready(Ok(false));
        }
        // Disjoint field borrows; one copy keeps `buf` appendable without
        // any unsafe zeroing dance around `ReadBuf::uninit`.
        self.buf.extend_from_slice(&self.scratch[..n]);
        Poll::Ready(Ok(true))
    }
}

impl Stream for WsReader {
    type Item = io::Result<FrameBody>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        loop {
            match this.step() {
                Ok(Step::NeedData) => {}
                Ok(Step::Continue) => continue,
                Ok(Step::Yield(frame)) => return Poll::Ready(Some(Ok(frame))),
                Ok(Step::Done) => return Poll::Ready(None),
                Err(e) => return Poll::Ready(Some(Err(e))),
            }
            match this.fill(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Ok(true)) => {}
                Poll::Ready(Ok(false)) => return Poll::Ready(None),
                Poll::Ready(Err(e)) => return Poll::Ready(Some(Err(e))),
            }
        }
    }
}

// ── the write half: one queue, one writer task ───────────────────────────

/// Outbound work for the single socket-writer task.
enum WsOut {
    /// One complete game frame's `[u32 LE len][op][payload]` envelope, to
    /// go out as ONE unmasked FIN binary message (the WS frame layer is
    /// applied here, at the only place that touches the socket).
    Game(Bytes),
    /// A control reply from the read path (pong / close / failure close):
    /// `(opcode, payload)`.
    Control(u8, Vec<u8>),
    /// Stop writing and shut the socket down: the peer initiated the close
    /// handshake and we echoed it — RFC 6455 §7.1.1 has the server close
    /// FIRST, not wait for the actor layer's teardown.
    Shutdown,
}

/// The ONLY task that ever writes to the socket. It awaits exactly one
/// source — the queue — which merges the writer pump's game traffic with
/// the reader's control replies (no multiplexing anywhere).
async fn ws_writer_task(mut sock: OwnedWriteHalf, mut rx: mpsc::Receiver<WsOut>) {
    while let Some(out) = rx.recv().await {
        let bytes = match out {
            WsOut::Game(envelope) => Bytes::from(encode_server_frame(OP_BIN, &envelope)),
            WsOut::Control(op, payload) => Bytes::from(encode_server_frame(op, &payload)),
            // Both halves drop here: the peer sees a prompt TCP FIN.
            WsOut::Shutdown => break,
        };
        if sock.write_all(&bytes).await.is_err() {
            break;
        }
    }
    // Last queue end dropped, write failed, or shutdown requested: shut the
    // socket down so a lingering peer observes EOF promptly.
    let _ = sock.shutdown().await;
}

/// Pump-facing writer: pushes encoded game frames onto the shared outbound
/// queue. Flushing is implicit — once queued, the writer task owns delivery.
/// Backpressure comes from the bounded queue via a reserved-capacity permit
/// taken in `poll_ready` and spent in `start_send`. `closing` guarantees at
/// most ONE close frame ever leaves this connection (the read path's echo
/// or failure-close wins; the sink's teardown close only fires otherwise).
struct WsWriter {
    tx: mpsc::Sender<WsOut>,
    permit: Option<mpsc::OwnedPermit<WsOut>>,
    closing: Arc<AtomicBool>,
}

impl Sink<FrameBody> for WsWriter {
    type Error = io::Error;

    fn poll_ready(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if this.permit.is_some() {
            return Poll::Ready(Ok(()));
        }
        // `reserve_owned` takes the sender by value, so hand it a cheap
        // clone (an Arc bump); the permit itself is what gets stored.
        let mut reserve = std::pin::pin!(this.tx.clone().reserve_owned());
        match ready!(reserve.as_mut().poll(cx)) {
            Ok(permit) => {
                this.permit = Some(permit);
                Poll::Ready(Ok(()))
            }
            Err(_) => Poll::Ready(Err(writer_gone())),
        }
    }

    fn start_send(self: Pin<&mut Self>, item: FrameBody) -> io::Result<()> {
        let this = self.get_mut();
        match this.permit.take() {
            Some(permit) => {
                // `send` hands the sender back (chaining API); drop it.
                let _ = permit.send(WsOut::Game(encode_game_envelope(&item)));
                Ok(())
            }
            // Unreachable after a successful poll_ready; a defensive error
            // beats a panic either way.
            None => {
                use tokio::sync::mpsc::error::TrySendError;
                match this.tx.try_send(WsOut::Game(encode_game_envelope(&item))) {
                    Ok(()) => Ok(()),
                    Err(TrySendError::Full(_)) => Err(io::Error::new(
                        io::ErrorKind::WouldBlock,
                        "websocket outbound queue full",
                    )),
                    Err(TrySendError::Closed(_)) => Err(writer_gone()),
                }
            }
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_close(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        // Best-effort WS close notice — but only if the read path has not
        // already sent one (echo / failure close): a second close frame is
        // noise. The actual socket shutdown happens when every queue end is
        // gone (writer task then shuts the half).
        let this = self.get_mut();
        if !this.closing.swap(true, Ordering::SeqCst) {
            let _ = this.tx.try_send(WsOut::Control(OP_CLOSE, Vec::new()));
        }
        Poll::Ready(Ok(()))
    }
}

fn writer_gone() -> io::Error {
    io::Error::new(io::ErrorKind::BrokenPipe, "websocket writer task is gone")
}

// ── transport wiring ─────────────────────────────────────────────────────

/// Maximum WS message (and therefore game-frame envelope) size. Mirrors
/// tcp.rs's guardrail: enforced BEFORE allocation, on both the frame level
/// and the reassembly level.
pub const DEFAULT_MAX_MESSAGE_BYTES: usize = 1024 * 1024;

/// WebSocket transport: accepts plain TCP connections, upgrades each with
/// the RFC 6455 opening handshake, then runs the standard framing + pumps
/// over the WS message layer.
#[derive(Clone)]
pub struct WsTransport {
    pub max_message_bytes: usize,
}

impl Default for WsTransport {
    fn default() -> Self {
        Self {
            max_message_bytes: DEFAULT_MAX_MESSAGE_BYTES,
        }
    }
}

struct WsListenerHandle {
    listener: TcpListener,
    max_message_bytes: usize,
}

impl Transport for WsTransport {
    fn bind(
        self: Arc<Self>,
        addr: SocketAddr,
    ) -> BoxFuture<'static, io::Result<Arc<dyn Listener>>> {
        Box::pin(async move {
            let listener = TcpListener::bind(addr).await?;
            debug!(%addr, "WebSocket listener bound");
            Ok(Arc::new(WsListenerHandle {
                listener,
                max_message_bytes: self.max_message_bytes,
            }) as Arc<dyn Listener>)
        })
    }
}

impl Listener for WsListenerHandle {
    fn accept(self: Arc<Self>) -> BoxFuture<'static, io::Result<Endpoint>> {
        Box::pin(async move {
            let (stream, peer) = self.listener.accept().await?;
            stream.set_nodelay(true)?;
            // One awaited source wrapped in a deadline (pump-timeout idiom):
            // the cap fires only while the handshake stays pending.
            match tokio::time::timeout(WS_HANDSHAKE_TIMEOUT, perform_upgrade(stream)).await {
                Ok(Ok(upgraded)) => {
                    debug!(%peer, "WebSocket upgrade completed");
                    let (read_half, write_half) = upgraded.into_split();
                    Ok(self.make_endpoint(read_half, write_half, peer))
                }
                Ok(Err(e)) => {
                    warn!(%peer, error = %e, "WebSocket handshake failed; closing");
                    Err(io::Error::other(format!("WebSocket handshake failed: {e}")))
                }
                Err(_) => {
                    warn!(%peer, timeout = ?WS_HANDSHAKE_TIMEOUT, "WebSocket handshake timed out; closing");
                    Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        format!("WebSocket handshake exceeded {:?}", WS_HANDSHAKE_TIMEOUT),
                    ))
                }
            }
        })
    }

    fn local_addr(&self) -> Option<SocketAddr> {
        self.listener.local_addr().ok()
    }
}

impl WsListenerHandle {
    /// Same shape as tcp/tls `make_endpoint`: build the pump-facing
    /// reader/writer pair plus the one extra socket-writer task the WS
    /// adapter needs (module docs). Idle-timeout support comes for free —
    /// `spawn_pumps` wraps our reader's pending reads like any other.
    fn make_endpoint(
        &self,
        read_half: OwnedReadHalf,
        write_half: OwnedWriteHalf,
        peer: SocketAddr,
    ) -> Endpoint {
        let max_message_bytes = self.max_message_bytes;
        Endpoint::new(
            move |conn: ConnectionId,
                  in_tx: Mailbox<ConnIn>,
                  out_rx: Inbox<FrameBatch>,
                  idle_timeout: Option<Duration>| {
                let (queue_tx, queue_rx) = mpsc::channel::<WsOut>(OUT_QUEUE_CAPACITY);
                // Detached on purpose: it is an implementation detail of the
                // adapter, owned by nobody above the pump layer; it exits by
                // itself when every queue end is dropped.
                let _ws_writer = tokio::spawn(ws_writer_task(write_half, queue_rx));
                let closing = Arc::new(AtomicBool::new(false));
                let reader = WsReader::new(
                    read_half,
                    max_message_bytes,
                    queue_tx.clone(),
                    closing.clone(),
                );
                let writer = WsWriter {
                    tx: queue_tx,
                    permit: None,
                    closing,
                };
                let (read, write) = spawn_pumps(conn, reader, writer, in_tx, out_rx, idle_timeout);
                (Some(read), write)
            },
        )
        .with_peer(peer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gsb_core::channel::channel;
    use tokio::io::AsyncReadExt;

    /// The canonical RFC 6455 §1.3 example pair.
    const RFC_KEY: &str = "dGhlIHNhbXBsZSBub25jZQ==";
    const RFC_ACCEPT: &str = "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=";

    #[test]
    fn base64_rfc4648_vectors() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn accept_key_matches_rfc6455_vector() {
        assert_eq!(accept_key(RFC_KEY.as_bytes()), RFC_ACCEPT);
    }

    #[test]
    fn server_frames_are_unmasked_with_correct_lengths() {
        let small = encode_server_frame(OP_BIN, b"ab");
        assert_eq!(small, vec![0x82, 0x02, b'a', b'b']);

        let mid_payload = vec![7u8; 300]; // forces the 16-bit length form
        let mid = encode_server_frame(OP_PING, &mid_payload);
        assert_eq!(&mid[..4], &[0x89, 126, 0x01, 0x2c]);

        let big_payload = vec![9u8; 70_000]; // forces the 64-bit length form
        let big = encode_server_frame(OP_BIN, &big_payload);
        assert_eq!(&big[..2], &[0x82, 127]);
        assert_eq!(&big[2..10], &(70_000u64).to_be_bytes());

        for frame in [&small, &mid, &big] {
            // Mask bit clear on every server frame.
            assert_eq!(frame[1] & 0x80, 0, "server frames must not be masked");
        }
    }

    #[test]
    fn game_envelope_roundtrips_like_tcp_framing() {
        let frame = FrameBody::new(0x1234, vec![1, 2, 3]);
        let env = encode_game_envelope(&frame);
        // Same prefix layout as framed.rs: LE length covering op+payload.
        assert_eq!(&env[..4], &((2 + 3usize) as u32).to_le_bytes());
        let parsed = FrameBody::decode(env.slice(4..)).unwrap();
        assert_eq!(parsed.op, 0x1234);
        assert_eq!(parsed.payload.as_ref(), &[1, 2, 3]);
    }

    // ── fake WS client (raw TcpStream, masked frames, no new deps) ──────

    /// A deterministic mask-key generator: RFC masking exists so proxies
    /// cannot guess payload bytes, not for secrecy — fixed keys are fine in
    /// tests and keep failures reproducible.
    struct MaskGen(u32);
    impl MaskGen {
        fn next(&mut self) -> [u8; 4] {
            self.0 = self.0.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            self.0.to_be_bytes()
        }
    }

    /// Encode one client-to-server WS frame (masked unless `mask` says no).
    fn encode_client_frame(
        fin: bool,
        opcode: u8,
        payload: &[u8],
        key: [u8; 4],
        mask: bool,
    ) -> Vec<u8> {
        let mut frame = Vec::with_capacity(14 + payload.len());
        frame.push(if fin { 0x80 } else { 0 } | opcode);
        let mask_bit = if mask { 0x80 } else { 0 };
        if payload.len() < 126 {
            frame.push(mask_bit | payload.len() as u8);
        } else if payload.len() <= u16::MAX as usize {
            frame.push(mask_bit | 126);
            frame.extend_from_slice(&(payload.len() as u16).to_be_bytes());
        } else {
            frame.push(mask_bit | 127);
            frame.extend_from_slice(&(payload.len() as u64).to_be_bytes());
        }
        if mask {
            frame.extend_from_slice(&key);
            let masked: Vec<u8> = payload
                .iter()
                .enumerate()
                .map(|(i, b)| b ^ key[i & 3])
                .collect();
            frame.extend_from_slice(&masked);
        } else {
            frame.extend_from_slice(payload);
        }
        frame
    }

    /// A raw-TCP fake client speaking enough of RFC 6455 to exercise the
    /// server: real handshake (accept key verified against the RFC vector),
    /// masked frames, fragmentation, close.
    struct FakeWsClient {
        stream: TcpStream,
        masks: MaskGen,
    }

    impl FakeWsClient {
        async fn connect(addr: SocketAddr) -> Self {
            let mut stream = TcpStream::connect(addr).await.expect("client connect");
            let request = format!(
                "GET /gsb HTTP/1.1\r\n\
                 Host: {addr}\r\n\
                 Upgrade: websocket\r\n\
                 Connection: Upgrade\r\n\
                 Sec-WebSocket-Key: {RFC_KEY}\r\n\
                 Sec-WebSocket-Version: 13\r\n\
                 \r\n"
            );
            stream
                .write_all(request.as_bytes())
                .await
                .expect("handshake write");
            let head = read_http_head(&mut stream).await;
            assert!(
                head.starts_with("HTTP/1.1 101 Switching Protocols\r\n"),
                "want 101, got: {head}"
            );
            let accept = header_value(&head, "sec-websocket-accept")
                .expect("101 response must carry Sec-WebSocket-Accept");
            assert_eq!(accept, RFC_ACCEPT, "accept key must match the RFC vector");
            Self {
                stream,
                masks: MaskGen(42),
            }
        }

        /// Raw variant used by the malformed-handshake tests.
        async fn raw(addr: SocketAddr) -> TcpStream {
            TcpStream::connect(addr).await.expect("raw connect")
        }

        async fn send_raw(&mut self, bytes: &[u8]) {
            self.stream.write_all(bytes).await.expect("raw write");
        }

        async fn send_frame(&mut self, fin: bool, opcode: u8, payload: &[u8], mask: bool) {
            let key = self.masks.next();
            let frame = encode_client_frame(fin, opcode, payload, key, mask);
            self.send_raw(&frame).await;
        }

        /// Send one game frame inside one masked binary WS message.
        async fn send_game(&mut self, frame: &FrameBody) {
            self.send_ws_binary(&encode_game_envelope(frame)).await;
        }

        async fn send_ws_binary(&mut self, body: &[u8]) {
            self.send_frame(true, OP_BIN, body, true).await;
        }

        /// Read one server frame: `(fin, opcode, payload)` — unmasked.
        async fn read_frame(&mut self) -> (bool, u8, Vec<u8>) {
            let mut head = [0u8; 2];
            self.stream.read_exact(&mut head).await.expect("frame head");
            let fin = head[0] & 0x80 != 0;
            assert_eq!(head[1] & 0x80, 0, "server must never mask");
            let l7 = (head[1] & 0x7f) as usize;
            let len = match l7 {
                0x7e => {
                    let mut ext = [0u8; 2];
                    self.stream.read_exact(&mut ext).await.expect("ext16");
                    u16::from_be_bytes(ext) as usize
                }
                0x7f => {
                    let mut ext = [0u8; 8];
                    self.stream.read_exact(&mut ext).await.expect("ext64");
                    u64::from_be_bytes(ext) as usize
                }
                n => n,
            };
            let mut payload = vec![0u8; len];
            if len > 0 {
                self.stream.read_exact(&mut payload).await.expect("payload");
            }
            (fin, head[0] & 0x0f, payload)
        }

        /// Read one echoed game frame.
        async fn read_game(&mut self) -> FrameBody {
            let (fin, opcode, payload) = self.read_frame().await;
            assert!(fin, "echoed messages are single-frame");
            assert_eq!(opcode, OP_BIN, "game frames ride binary messages");
            let declared =
                u32::from_le_bytes([payload[0], payload[1], payload[2], payload[3]]) as usize;
            assert_eq!(payload.len(), 4 + declared, "exactly one frame per message");
            FrameBody::decode(Bytes::copy_from_slice(&payload[4..])).expect("echo decode")
        }

        /// Read one frame; it must be the close. Returns its status code,
        /// then asserts the connection ends (EOF, or a reset if the server
        /// failed before draining everything we pipelined).
        async fn expect_close_then_eof(&mut self) -> u16 {
            let (_, opcode, payload) = self.read_frame().await;
            assert_eq!(opcode, OP_CLOSE, "next frame must be the close");
            let code = match payload.as_slice() {
                [] => 1005, // no status present
                [hi, lo] => u16::from_be_bytes([*hi, *lo]),
                other => panic!(
                    "close payload must be empty or 2 bytes, got {}",
                    other.len()
                ),
            };
            self.expect_eof().await;
            code
        }

        /// The socket must be over: clean EOF, or a reset when the server
        /// tore down with unread bytes still in flight (RST beats FIN).
        async fn expect_eof(&mut self) {
            let mut eof = [0u8; 1];
            match self.stream.read(&mut eof).await {
                Ok(0) => {}
                Ok(n) => panic!("expected end of stream, got {n} byte(s)"),
                Err(e) if e.kind() == io::ErrorKind::ConnectionReset => {}
                Err(e) => panic!("unexpected read error at teardown: {e}"),
            }
        }
    }

    async fn read_http_head(stream: &mut TcpStream) -> String {
        let mut buf = Vec::with_capacity(512);
        let mut byte = [0u8; 1];
        while buf.windows(4).position(|w| w == b"\r\n\r\n").is_none() {
            let n = stream.read(&mut byte).await.expect("http head read");
            assert!(n > 0, "EOF before the HTTP response head ended");
            buf.push(byte[0]);
            assert!(buf.len() < 16 * 1024, "response head runaway");
        }
        String::from_utf8_lossy(&buf).into_owned()
    }

    fn header_value<'a>(head: &'a str, name: &str) -> Option<&'a str> {
        head.lines().find_map(|line| {
            let (n, v) = line.split_once(':')?;
            n.trim().eq_ignore_ascii_case(name).then(|| v.trim())
        })
    }

    // ── full-transport scaffolding ──────────────────────────────────────

    /// Bind a `WsTransport` and run ONE accept whose endpoint echoes every
    /// game frame back through the standard pumps (like tcp.rs's tests).
    /// Returns the bound address.
    async fn serve_echo(idle_timeout: Option<Duration>) -> SocketAddr {
        serve_echo_max(DEFAULT_MAX_MESSAGE_BYTES, idle_timeout).await
    }

    /// Same, with an explicit message-size ceiling.
    async fn serve_echo_max(
        max_message_bytes: usize,
        idle_timeout: Option<Duration>,
    ) -> SocketAddr {
        let transport: Arc<dyn Transport> = Arc::new(WsTransport { max_message_bytes });
        let addr = SocketAddr::from(([127, 0, 0, 1], 0));
        let listener = transport.bind(addr).await.expect("bind");
        let addr = listener.local_addr().expect("local addr");
        tokio::spawn(async move {
            // A failed handshake (the 400 tests hit this listener too)
            // surfaces as an accept error: fine, this helper only serves
            // the happy path.
            let Ok(endpoint) = listener.accept().await else {
                return;
            };
            let (in_tx, mut in_rx) = channel::<ConnIn>(16);
            let (out_tx, out_rx) = channel::<FrameBatch>(16);
            let (read, write) = endpoint.start_pump(ConnectionId(77), in_tx, out_rx, idle_timeout);
            while let Some(msg) = in_rx.recv().await {
                match msg {
                    // Echo; if the writer side is gone the pump exit ends
                    // this loop anyway.
                    ConnIn::Frame(frame) => drop(out_tx.send(vec![frame]).await),
                    ConnIn::Closed { .. } | ConnIn::ServerClosed { .. } => break,
                    _ => {}
                }
            }
            drop(out_tx);
            if let Some(read) = read {
                let _ = read.await;
            }
            let _ = write.await;
        });
        addr
    }

    // ── handshake behavior ──────────────────────────────────────────────

    /// Malformed request heads get an HTTP 400 and the accept surfaces an
    /// error (same contract as the TLS listener).
    #[tokio::test]
    async fn malformed_handshake_gets_http_400() {
        let addr = serve_echo(None).await;
        let mut client = FakeWsClient::raw(addr).await;
        client.write_all(b"NONSENSE\r\n\r\n").await.unwrap();

        let head = read_http_head(&mut client).await;
        assert!(head.starts_with("HTTP/1.1 400 Bad Request"), "got: {head}");
        // Drain the short plain-text body (Content-Length delimited), then
        // the server must hang up (EOF, or a reset if it tore down first).
        let len: usize = header_value(&head, "content-length")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        let mut body = vec![0u8; len];
        if len > 0 {
            client.read_exact(&mut body).await.expect("400 body");
        }
        let mut eof = [0u8; 1];
        match client.read(&mut eof).await {
            Ok(0) => {}
            Ok(n) => panic!("expected end of stream, got {n} byte(s)"),
            Err(e) if e.kind() == io::ErrorKind::ConnectionReset => {}
            Err(e) => panic!("unexpected read error at teardown: {e}"),
        }
    }

    /// A well-formed HTTP request that simply is not a websocket upgrade is
    /// also a 400 (missing/mismatched headers).
    #[tokio::test]
    async fn non_websocket_get_request_gets_http_400() {
        let addr = serve_echo(None).await;
        let mut client = FakeWsClient::raw(addr).await;
        client
            .write_all(b"GET / HTTP/1.1\r\nHost: x\r\nAccept: */*\r\n\r\n")
            .await
            .unwrap();
        let head = read_http_head(&mut client).await;
        assert!(head.starts_with("HTTP/1.1 400 Bad Request"), "got: {head}");
    }

    /// Wrong Sec-WebSocket-Version → 400 (the header IS sent, just wrong).
    #[tokio::test]
    async fn wrong_version_gets_http_400() {
        let addr = serve_echo(None).await;
        let mut client = FakeWsClient::raw(addr).await;
        client
            .write_all(
                b"GET / HTTP/1.1\r\nHost: x\r\nUpgrade: websocket\r\n\
                  Connection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
                  Sec-WebSocket-Version: 8\r\n\r\n",
            )
            .await
            .unwrap();
        let head = read_http_head(&mut client).await;
        assert!(head.starts_with("HTTP/1.1 400 Bad Request"), "got: {head}");
    }

    // ── data plane ──────────────────────────────────────────────────────

    /// Handshake succeeds with the RFC-vector accept key, and masked binary
    /// game frames echo back one-for-one — including one big enough to force
    /// the 64-bit length form in BOTH directions.
    #[tokio::test]
    async fn handshake_and_masked_game_frames_echo() {
        let addr = serve_echo(None).await;
        let mut client = FakeWsClient::connect(addr).await;

        client
            .send_game(&FrameBody::new(42, b"hello".as_slice()))
            .await;
        let echo = client.read_game().await;
        assert_eq!(echo.op, 42);
        assert_eq!(echo.payload.as_ref(), b"hello");

        let big_payload: Vec<u8> = (0..70_000u32).map(|i| i as u8).collect();
        client
            .send_game(&FrameBody::new(7, big_payload.clone()))
            .await;
        let echo = client.read_game().await;
        assert_eq!(echo.op, 7);
        assert_eq!(echo.payload.as_ref(), big_payload.as_slice());
    }

    /// A binary message fragmented into three frames reassembles into ONE
    /// game frame (continuation opcodes, final FIN).
    #[tokio::test]
    async fn fragmented_binary_message_reassembles() {
        let addr = serve_echo(None).await;
        let mut client = FakeWsClient::connect(addr).await;

        let envelope =
            encode_game_envelope(&FrameBody::new(9, b"fragmented-game-frame".as_slice()));
        let mid = envelope.len() / 3;
        let (first, rest) = envelope.split_at(mid);
        let (second, third) = rest.split_at(rest.len() / 2);
        client.send_frame(false, OP_BIN, first, true).await;
        client.send_frame(false, OP_CONT, second, true).await;
        client.send_frame(true, OP_CONT, third, true).await;

        let echo = client.read_game().await;
        assert_eq!(echo.op, 9);
        assert_eq!(echo.payload.as_ref(), b"fragmented-game-frame");
    }

    /// Ping → Pong with the same application data, no game frame yielded.
    #[tokio::test]
    async fn ping_gets_pong() {
        let addr = serve_echo(None).await;
        let mut client = FakeWsClient::connect(addr).await;

        client.send_frame(true, OP_PING, b"hb", true).await;
        let (_, opcode, payload) = client.read_frame().await;
        assert_eq!(opcode, OP_PONG);
        assert_eq!(payload, b"hb");

        // And the data plane still works after the control exchange.
        client
            .send_game(&FrameBody::new(1, b"still-alive".as_slice()))
            .await;
        let echo = client.read_game().await;
        assert_eq!(echo.payload.as_ref(), b"still-alive");
    }

    /// An unmasked client frame is a protocol violation: the server fails
    /// the connection with close code 1002 and shuts the socket down.
    #[tokio::test]
    async fn unmasked_client_frame_is_rejected_with_1002() {
        let addr = serve_echo(None).await;
        let mut client = FakeWsClient::connect(addr).await;

        let envelope = encode_game_envelope(&FrameBody::new(1, b"sneaky".as_slice()));
        client.send_ws_binary_unmasked(&envelope).await;

        assert_eq!(client.expect_close_then_eof().await, 1002);
    }

    /// Text messages have no meaning in the gsb wire contract → 1003.
    #[tokio::test]
    async fn text_message_is_rejected_with_1003() {
        let addr = serve_echo(None).await;
        let mut client = FakeWsClient::connect(addr).await;

        client.send_frame(true, OP_TEXT, b"hi", true).await;
        assert_eq!(client.expect_close_then_eof().await, 1003);
    }

    /// An oversized declared length fails fast — before its payload even
    /// arrives — with close code 1009.
    #[tokio::test]
    async fn oversized_declared_length_is_rejected_with_1009() {
        let addr = serve_echo_max(64, None).await;
        let mut client = FakeWsClient::connect(addr).await;

        // Header claims 1000 payload bytes (over the 64 ceiling); we do not
        // even bother sending them all: rejection happens at header time.
        let mut frame = encode_client_frame(true, OP_BIN, &vec![0u8; 1000], [1, 2, 3, 4], true);
        frame.truncate(2 + 2 + 4 + 16); // head + ext16 + mask + partial payload
        client.send_raw(&frame).await;

        assert_eq!(client.expect_close_then_eof().await, 1009);
    }

    /// Client-initiated close: the server echoes the same status code, then
    /// ends the stream, and the pump reports a clean "peer closed".
    #[tokio::test]
    async fn close_handshake_echoes_code_and_reports_peer_closed() {
        let transport: Arc<dyn Transport> = Arc::new(WsTransport::default());
        let addr = SocketAddr::from(([127, 0, 0, 1], 0));
        let listener = transport.bind(addr).await.expect("bind");
        let addr = listener.local_addr().unwrap();

        // Connect in the background; the backlog holds the socket until
        // accept runs (same pattern as the idle-timeout test).
        let client = tokio::spawn(FakeWsClient::connect(addr));

        let endpoint = listener.accept().await.expect("ws accept");
        let (in_tx, mut in_rx) = channel::<ConnIn>(8);
        // `out_tx` stays alive until after the echo is read: dropping it
        // makes the writer pump emit the transport's own (empty) close,
        // which would race the client-initiated one.
        let (out_tx, out_rx) = channel::<FrameBatch>(8);
        let (read, write) = endpoint.start_pump(ConnectionId(5), in_tx, out_rx, None);
        tokio::spawn(async move {
            if let Some(read) = read {
                let _ = read.await;
            }
            let _ = write.await;
        });

        let mut client = client.await.expect("client handshake");
        client
            .send_frame(true, OP_CLOSE, &1000u16.to_be_bytes(), true)
            .await;

        let (fin, opcode, payload) = client.read_frame().await;
        assert!(fin && opcode == OP_CLOSE, "close echo expected");
        assert_eq!(payload, 1000u16.to_be_bytes(), "status code must be echoed");
        client.expect_eof().await;

        let msg = tokio::time::timeout(Duration::from_secs(5), in_rx.recv())
            .await
            .expect("inbox open")
            .expect("pump notified");
        match msg {
            ConnIn::Closed { reason } => assert_eq!(reason, "peer closed"),
            other => panic!("expected Closed, got {other:?}"),
        }
        drop(out_tx); // now the transport's own empty close may go out
    }

    /// The idle window still guards a WS connection: silence beyond the
    /// window produces `ServerClosed` (idle reason), exactly like tcp.rs —
    /// the WS reader simply pends like any other pump source.
    #[tokio::test]
    async fn idle_timeout_still_applies_to_websockets() {
        let transport: Arc<dyn Transport> = Arc::new(WsTransport::default());
        let addr = SocketAddr::from(([127, 0, 0, 1], 0));
        let listener = transport.bind(addr).await.expect("bind");
        let addr = listener.local_addr().unwrap();

        // The handshake only completes once accept runs, so connect in the
        // background first and let the TCP backlog hold the connection.
        let client = tokio::spawn(FakeWsClient::connect(addr));

        let endpoint = listener.accept().await.expect("ws accept");
        let (in_tx, mut in_rx) = channel::<ConnIn>(8);
        let (out_tx, out_rx) = channel::<FrameBatch>(8);
        let (read, write) = endpoint.start_pump(
            ConnectionId(6),
            in_tx,
            out_rx,
            Some(Duration::from_millis(200)),
        );
        // Hold the connection OPEN (binding matters: dropping it would send
        // a FIN and look like a clean close) and say nothing — after the
        // window the reader pump must fire ServerClosed on its own.
        let _client = client.await.expect("client handshake");

        let msg = tokio::time::timeout(Duration::from_secs(5), in_rx.recv())
            .await
            .expect("inbox open")
            .expect("pump notified");
        match msg {
            ConnIn::ServerClosed { reason } => {
                assert!(reason.contains("idle timeout"), "reason: {reason}");
            }
            other => panic!("expected ServerClosed, got {other:?}"),
        }
        read.expect("reader pump exits").await.unwrap();
        drop(out_tx);
        write.await.expect("writer pump exits");
    }

    impl FakeWsClient {
        /// Test-only helper: deliberately unmasked binary message.
        async fn send_ws_binary_unmasked(&mut self, body: &[u8]) {
            self.send_frame(true, OP_BIN, body, false).await;
        }
    }
}
