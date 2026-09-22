//! The default transport: TCP + `[u32 LE length][u16 LE opcode][payload]`.
//!
//! This is the de-facto industry-standard framing for a tokio game server
//! stack. Frame bodies are handed to the actor layer via
//! [`gsb_protocol::FrameBody`]; the 4-byte length prefix lives in
//! [`crate::framed`], inside this crate, and never leaks out.

use std::sync::Arc;

use tokio::net::TcpListener;
use tokio::net::TcpStream;
use tokio::net::tcp::OwnedReadHalf;
use tokio::net::tcp::OwnedWriteHalf;

use gsb_core::channel::{FrameBatch, Inbox, Mailbox};
use gsb_core::conn::ConnIn;
use gsb_core::id::ConnectionId;
use tracing::debug;

use crate::framed::FrameReader;
use crate::framed::FrameWriter;
use crate::pump::spawn_pumps;
use crate::transport::{BoxFuture, Endpoint, Listener, Transport};

#[cfg(test)]
use futures::Sink;
#[cfg(test)]
use futures::Stream;
#[cfg(test)]
use futures::StreamExt;
#[cfg(test)]
use gsb_protocol::FrameBody;

/// Maximum frame body size (opcode + payload). Defaults to 1 MiB.
pub const DEFAULT_MAX_FRAME_BYTES: usize = 1024 * 1024;

/// Default TCP transport.
#[derive(Clone)]
pub struct TcpTransport {
    pub max_frame_bytes: usize,
}

impl Default for TcpTransport {
    fn default() -> Self {
        Self {
            max_frame_bytes: DEFAULT_MAX_FRAME_BYTES,
        }
    }
}

struct TcpListenerHandle {
    listener: TcpListener,
    max_frame_bytes: usize,
}

impl Transport for TcpTransport {
    fn bind(
        self: Arc<Self>,
        addr: std::net::SocketAddr,
    ) -> BoxFuture<'static, std::io::Result<Arc<dyn Listener>>> {
        Box::pin(async move {
            let listener = TcpListener::bind(addr).await?;
            debug!(%addr, "TCP listener bound");
            Ok(Arc::new(TcpListenerHandle {
                listener,
                max_frame_bytes: self.max_frame_bytes,
            }) as Arc<dyn Listener>)
        })
    }
}

/// The TCP reader: length-delimited frames over the socket's read half.
type TcpReader = FrameReader<OwnedReadHalf>;
/// The TCP writer: length-prefixed frames into the socket's write half.
type TcpWriter = FrameWriter<OwnedWriteHalf>;

impl Listener for TcpListenerHandle {
    fn accept(self: Arc<Self>) -> BoxFuture<'static, std::io::Result<Endpoint>> {
        Box::pin(async move {
            let (stream, peer) = self.listener.accept().await?;
            stream.set_nodelay(true)?;
            debug!(%peer, "connection accepted");
            Ok(self.make_endpoint(stream, peer))
        })
    }

    fn local_addr(&self) -> Option<std::net::SocketAddr> {
        self.listener.local_addr().ok()
    }
}

impl TcpListenerHandle {
    fn make_endpoint(&self, stream: TcpStream, peer: std::net::SocketAddr) -> Endpoint {
        let (read_half, write_half) = stream.into_split();
        let max_frame_bytes = self.max_frame_bytes;
        // The reader handle is `Some` here: TCP has a per-connection read
        // half, so the reader pump is genuinely this endpoint's task.
        Endpoint::new(
            move |conn: ConnectionId,
                  in_tx: Mailbox<ConnIn>,
                  out_rx: Inbox<FrameBatch>,
                  timeouts: crate::pump::PumpTimeouts| {
                let reader = TcpReader::new(read_half, max_frame_bytes);
                let writer = TcpWriter::new(write_half);
                let (read, write) = spawn_pumps(conn, reader, writer, in_tx, out_rx, timeouts);
                (Some(read), write)
            },
        )
        .with_peer(peer)
    }
}

#[cfg(test)]
impl FrameReader<OwnedReadHalf> {
    /// Test helper: build a reader/writer pair over a live TCP stream.
    pub fn for_stream(
        stream: TcpStream,
        max_frame_bytes: usize,
    ) -> (
        impl Stream<Item = std::io::Result<FrameBody>> + 'static,
        impl Sink<FrameBody, Error = std::io::Error> + crate::pump::WriteProgress + 'static,
    ) {
        let (r, w) = stream.into_split();
        (FrameReader::new(r, max_frame_bytes), FrameWriter::new(w))
    }
}

#[cfg(test)]
mod tests;
