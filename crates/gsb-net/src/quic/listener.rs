//! Bind and accept: one quinn endpoint per listener, one bidirectional
//! stream per connection, then the same framed pumps every other door
//! hands the actor layer.

use std::io;
use std::net::SocketAddr;
use std::sync::Arc;

use tracing::debug;
use tracing::warn;

use gsb_core::channel::FrameBatch;
use gsb_core::channel::Inbox;
use gsb_core::channel::Mailbox;
use gsb_core::conn::ConnIn;
use gsb_core::id::ConnectionId;

use crate::framed::FrameReader;
use crate::framed::FrameWriter;
use crate::pump::spawn_pumps;
use crate::quic::*;
use crate::transport::BoxFuture;
use crate::transport::Endpoint;
use crate::transport::Listener;
use crate::transport::Transport;

impl Transport for QuicTransport {
    fn bind(
        self: Arc<Self>,
        addr: SocketAddr,
    ) -> BoxFuture<'static, io::Result<Arc<dyn Listener>>> {
        Box::pin(async move {
            // Load the identity BEFORE binding the socket: a server whose
            // keys are broken must not half-start (same rule as TLS).
            let server_config = load_server_config(&self.config)?;
            let endpoint = quinn::Endpoint::server(server_config, addr)?;
            debug!(%addr, "QUIC listener bound (quinn over UDP)");
            Ok(Arc::new(QuicListenerHandle {
                endpoint,
                max_frame_bytes: self.config.max_frame_bytes,
            }) as Arc<dyn Listener>)
        })
    }
}

impl Listener for QuicListenerHandle {
    fn accept(self: Arc<Self>) -> BoxFuture<'static, io::Result<Endpoint>> {
        Box::pin(async move {
            let Some(incoming) = self.endpoint.accept().await else {
                return Err(io::Error::other("QUIC endpoint closed"));
            };
            let peer = incoming.remote_address();
            // ONE awaited sequence wrapped in a single deadline: handshake
            // THEN the client's promised bi-stream. The deadline covers
            // both — a client that completes the handshake and then never
            // opens a stream is equally able to pin an accept slot. The
            // deadline fires only while the sequence stays pending (the
            // pump-timeout idiom — no multiplexing).
            match tokio::time::timeout(HANDSHAKE_TIMEOUT, async {
                let conn = incoming.await?;
                let (send, recv) = conn.accept_bi().await?;
                Ok::<_, quinn::ConnectionError>((send, recv))
            })
            .await
            {
                Ok(Ok((send, recv))) => {
                    debug!(%peer, "QUIC connection accepted; bi-stream open");
                    Ok(self.make_endpoint(send, recv, peer))
                }
                Ok(Err(e)) => {
                    // The client's failure (bad cert, unknown CA, no
                    // shared ALPN, stream refused). Reported so the accept
                    // loop can back off briefly; the connection is dropped.
                    warn!(%peer, error = %e, "QUIC handshake failed; closing");
                    Err(io::Error::other(format!("QUIC handshake failed: {e}")))
                }
                Err(_) => {
                    warn!(%peer, timeout = ?HANDSHAKE_TIMEOUT, "QUIC handshake timed out; closing");
                    Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        format!("QUIC handshake exceeded {:?}", HANDSHAKE_TIMEOUT),
                    ))
                }
            }
        })
    }

    fn local_addr(&self) -> Option<SocketAddr> {
        self.endpoint.local_addr().ok()
    }

    /// Graceful-shutdown door (see the trait doc): stop accepting — new
    /// connection attempts are refused from here on — and leave every
    /// LIVE connection to the actor cascade, exactly like a TCP
    /// listener's close leaves its accepted sockets alone.
    ///
    /// NOT `Endpoint::close`, which this used to call: it closes every
    /// connection IMMEDIATELY, and an immediate QUIC close abandons the
    /// stream data still in flight. On a server stop that data is the
    /// `ERROR` code 14 notice each connection actor has just queued
    /// (docs/DESIGN.md §5.6), and `stop()` closes the listeners right
    /// after asking the registry to stop — so the endpoint close raced
    /// ahead of every notice and the QUIC door alone stayed silent.
    /// Now each connection ends the way it does on every other door:
    /// the actor ends, its writer pump writes what is queued and
    /// finishes the stream, and the connection goes when its streams
    /// do (the quinn endpoint driver lives until the last one has).
    fn close(&self) {
        self.endpoint.set_server_config(None);
    }
}

impl QuicListenerHandle {
    /// Same wiring as TCP/TLS `make_endpoint`: hand the stream halves to
    /// the shared generic framing + pumps. quinn's `RecvStream`/
    /// `SendStream` implement `AsyncRead`/`AsyncWrite`, so NOTHING below
    /// this point knows QUIC is involved. The `Connection` handle itself
    /// is intentionally dropped here: the streams pin the connection's
    /// shared state, so it lives as long as its pumps do.
    fn make_endpoint(
        &self,
        send: quinn::SendStream,
        recv: quinn::RecvStream,
        peer: SocketAddr,
    ) -> Endpoint {
        let max_frame_bytes = self.max_frame_bytes;
        // The reader handle is `Some`: like TCP, QUIC-v1 has a
        // per-connection read half owned by this endpoint's reader pump.
        Endpoint::new(
            move |conn: ConnectionId,
                  in_tx: Mailbox<ConnIn>,
                  out_rx: Inbox<FrameBatch>,
                  timeouts: crate::pump::PumpTimeouts| {
                let reader: QuicReader = FrameReader::new(recv, max_frame_bytes);
                let writer: QuicWriter = FrameWriter::new(send);
                let (read, write) = spawn_pumps(conn, reader, writer, in_tx, out_rx, timeouts);
                (Some(read), write)
            },
        )
        .with_peer(peer)
    }
}
