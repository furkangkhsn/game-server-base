//! Bind and accept: one quinn endpoint per listener, one bidirectional
//! stream per connection, then the same framed pumps every other door
//! hands the actor layer. Each connection's handshake and stream wait
//! run in its own task, off the accept loop (BACKLOG B31).

use std::io;
use std::net::SocketAddr;
use std::sync::Arc;

use tracing::debug;

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
use crate::transport::HandshakeStats;
use crate::transport::Listener;
use crate::transport::Transport;
use crate::transport::intake::{Intake, IntakeHandle};
use crate::transport::listener_closed;

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
            let intake = Intake::new("QUIC", self.config.max_pending_handshakes);
            tokio::spawn(run_intake(
                Arc::clone(&intake),
                endpoint.clone(),
                self.config.max_frame_bytes,
                self.config.metrics.clone(),
            ));
            Ok(Arc::new(QuicListenerHandle {
                endpoint,
                intake: IntakeHandle(intake),
            }) as Arc<dyn Listener>)
        })
    }
}

impl Listener for QuicListenerHandle {
    fn accept(self: Arc<Self>) -> BoxFuture<'static, io::Result<Endpoint>> {
        // `self` rides along: the last handle's drop closes the door.
        Box::pin(async move { Arc::clone(&self.intake.0).next().await })
    }

    fn local_addr(&self) -> Option<SocketAddr> {
        self.endpoint.local_addr().ok()
    }

    /// Graceful-shutdown door (see the trait doc): stop accepting — new
    /// connection attempts are refused from here on, and the pending
    /// accept and every handshake in flight end (the intake's door) —
    /// and leave every LIVE connection to
    /// the actor cascade, exactly like a TCP listener's close leaves its
    /// accepted sockets alone.
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
        self.intake.0.close();
    }

    fn handshake_stats(&self) -> Option<HandshakeStats> {
        Some(self.intake.0.stats())
    }
}

/// The door's intake task: take each incoming connection and give it a
/// slot and a handshake task — or refuse it — until the door closes or
/// the endpoint does.
async fn run_intake(
    intake: Arc<Intake>,
    endpoint: quinn::Endpoint,
    max_frame_bytes: usize,
    metrics: crate::TransportMetrics,
) {
    let mut flusher = crate::metrics::Flusher::new(metrics);
    loop {
        // `None`: the endpoint is closed and will never accept again.
        let next = async { endpoint.accept().await.ok_or_else(listener_closed) };
        let Ok(incoming) = intake.door().admit(next).await else {
            break;
        };
        let peer = incoming.remote_address();
        match intake.try_slot() {
            Some(slot) => {
                let handshake = handshake(incoming, peer, max_frame_bytes);
                intake.spawn(slot, peer, HANDSHAKE_TIMEOUT, handshake);
            }
            None => {
                debug!(%peer, "handshake bound reached; connection refused");
                incoming.refuse();
            }
        }
        intake.flush_metrics(&mut flusher, false);
    }
    intake.flush_metrics(&mut flusher, true);
    intake.log_summary();
}

/// One connection's handshake THEN the client's promised bi-stream (its
/// own task; the intake puts ONE deadline, [`HANDSHAKE_TIMEOUT`], and
/// the door around both — a client that completes the handshake and
/// then never opens a stream is equally able to pin a slot). A failure
/// is the client's (bad cert, unknown CA, no shared ALPN, stream
/// refused): counted and logged by the intake, the connection dropped.
async fn handshake(
    incoming: quinn::Incoming,
    peer: SocketAddr,
    max_frame_bytes: usize,
) -> io::Result<Endpoint> {
    let conn = incoming.await.map_err(io::Error::other)?;
    let (send, recv) = conn.accept_bi().await.map_err(io::Error::other)?;
    debug!(%peer, "QUIC connection accepted; bi-stream open");
    Ok(make_endpoint(send, recv, peer, max_frame_bytes))
}

/// Same wiring as TCP/TLS `make_endpoint`: hand the stream halves to
/// the shared generic framing + pumps. quinn's `RecvStream`/
/// `SendStream` implement `AsyncRead`/`AsyncWrite`, so NOTHING below
/// this point knows QUIC is involved. The `Connection` handle itself
/// is intentionally dropped here: the streams pin the connection's
/// shared state, so it lives as long as its pumps do.
fn make_endpoint(
    send: quinn::SendStream,
    recv: quinn::RecvStream,
    peer: SocketAddr,
    max_frame_bytes: usize,
) -> Endpoint {
    // The reader handle is `Some`: like TCP, QUIC-v1 has a
    // per-connection read half owned by this endpoint's reader pump.
    Endpoint::new(
        move |conn: ConnectionId,
              in_tx: Mailbox<ConnIn>,
              out_rx: Inbox<FrameBatch>,
              timeouts: crate::pump::PumpTimeouts| {
            let reader: QuicReader = FrameReader::new(recv, max_frame_bytes);
            let writer: QuicWriter = FrameWriter::new(send::QuicSend::new(send));
            let (read, write) = spawn_pumps(conn, reader, writer, in_tx, out_rx, timeouts);
            (Some(read), write)
        },
    )
    .with_peer(peer)
}
