//! Unit tests for [`super`] (moved out of the module file so the
//! implementation reads on its own; still a child module, so
//! `use super::*` reaches the parent's private items exactly as
//! before).

use super::*;
use crate::transport::Transport;
use bytes::Bytes;
use gsb_core::channel::FrameBatch;
use gsb_core::channel::channel;
use gsb_core::conn::ConnIn;
use gsb_core::id::ConnectionId;
use gsb_protocol::FrameBody;
use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;

/// The canonical RFC 6455 §1.3 example pair.
pub(super) const RFC_KEY: &str = "dGhlIHNhbXBsZSBub25jZQ==";
pub(super) const RFC_ACCEPT: &str = "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=";

// ── fake WS client (raw TcpStream, masked frames, no new deps) ──────

mod client;
use client::*;
mod close_frames;
mod fragmentation;
mod framing;
mod protocol;
mod queue;
mod rig;
mod slow_reader;

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
async fn serve_echo_max(max_message_bytes: usize, idle_timeout: Option<Duration>) -> SocketAddr {
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
        let (read, write) = endpoint.start_pump(
            ConnectionId(77),
            in_tx,
            out_rx,
            crate::pump::PumpTimeouts {
                idle: idle_timeout,
                write_stall: None,
            },
        );
        while let Some(msg) = in_rx.recv().await {
            match msg {
                // Echo; if the writer side is gone the pump exit ends
                // this loop anyway.
                ConnIn::Frame(frame) => drop(out_tx.send(vec![frame]).await),
                // Every end the reader pump reports: the peer left, the
                // server's verdict, or the stream refused (a protocol
                // violation — what the failure-close tests provoke).
                ConnIn::Closed { .. }
                | ConnIn::ServerClosed { .. }
                | ConnIn::StreamRejected { .. } => break,
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

mod handshake;
