//! Transport wiring: bind, accept, upgrade, then hand the actor layer
//! an [`Endpoint`] that speaks the same frames as every other door. The
//! upgrade runs in its own task, off the accept loop (BACKLOG B31 —
//! `crate::transport::intake`).

use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use tokio::net::TcpListener;
use tokio::net::tcp::OwnedReadHalf;
use tokio::net::tcp::OwnedWriteHalf;
use tracing::debug;

use gsb_core::channel::FrameBatch;
use gsb_core::channel::Inbox;
use gsb_core::channel::Mailbox;
use gsb_core::conn::ConnIn;
use gsb_core::id::ConnectionId;

use crate::pump::spawn_pumps;
use crate::transport::BoxFuture;
use crate::transport::DEFAULT_MAX_PENDING_HANDSHAKES;
use crate::transport::Endpoint;
use crate::transport::HandshakeStats;
use crate::transport::Listener;
use crate::transport::Transport;
use crate::transport::intake::{Intake, IntakeHandle, run_tcp_intake};
use crate::ws::*;

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
    /// How a binary message maps to a frame. Every server door uses
    /// [`WsMessageMapping::GameEnvelope`] — the wire contract.
    pub mapping: WsMessageMapping,
    /// The bound on upgrades in flight (BACKLOG B31): a connection over
    /// it is closed unupgraded and counted.
    pub max_pending_handshakes: usize,
    /// Where the handshake intake and every connection's reader send
    /// their losses (B58; `None` = counted nowhere but the logs).
    pub metrics: crate::TransportMetrics,
}

impl Default for WsTransport {
    fn default() -> Self {
        Self {
            max_message_bytes: DEFAULT_MAX_MESSAGE_BYTES,
            mapping: WsMessageMapping::GameEnvelope,
            max_pending_handshakes: DEFAULT_MAX_PENDING_HANDSHAKES,
            metrics: None,
        }
    }
}

/// How one WS binary message maps to a [`gsb_protocol::FrameBody`]. Only
/// the message layer differs: framing, fragmentation, control frames, the
/// close handshake and every rejection are the same code either way.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum WsMessageMapping {
    /// The gsb wire contract (module docs): each message is exactly one
    /// `[u32 LE len][u16 LE op][payload]` envelope, validated (1007).
    #[default]
    GameEnvelope,
    /// The whole message is an opaque payload: inbound it becomes a frame
    /// with op 0, outbound a frame's payload is sent as the message and
    /// its op is dropped. It exists for the RFC 6455 conformance harness
    /// (`examples/ws_autobahn.rs`): the Autobahn fuzzing client checks
    /// echoes of arbitrary binary payloads, which the envelope would
    /// refuse. Server configuration cannot select it.
    Opaque,
}

pub(super) struct WsListenerHandle {
    local_addr: Option<SocketAddr>,
    /// The upgrades, off the accept loop: `accept` takes finished ones;
    /// [`Listener::close`] (or the last handle's drop) closes its door —
    /// the raw accept, every upgrade in flight, the pending accept (B16).
    intake: IntakeHandle,
}

impl Transport for WsTransport {
    fn bind(
        self: Arc<Self>,
        addr: SocketAddr,
    ) -> BoxFuture<'static, io::Result<Arc<dyn Listener>>> {
        Box::pin(async move {
            let listener = TcpListener::bind(addr).await?;
            debug!(%addr, "WebSocket listener bound");
            let local_addr = listener.local_addr().ok();
            let intake = Intake::new("WebSocket", self.max_pending_handshakes);
            let (max_message_bytes, mapping) = (self.max_message_bytes, self.mapping);
            let metrics = self.metrics.clone();
            tokio::spawn(run_tcp_intake(
                Arc::clone(&intake),
                listener,
                WS_HANDSHAKE_TIMEOUT,
                move |stream, peer| {
                    upgrade(stream, peer, max_message_bytes, mapping, metrics.clone())
                },
                self.metrics.clone(),
            ));
            Ok(Arc::new(WsListenerHandle {
                local_addr,
                intake: IntakeHandle(intake),
            }) as Arc<dyn Listener>)
        })
    }
}

impl Listener for WsListenerHandle {
    fn accept(self: Arc<Self>) -> BoxFuture<'static, io::Result<Endpoint>> {
        // `self` rides along: the last handle's drop closes the door.
        Box::pin(async move { Arc::clone(&self.intake.0).next().await })
    }

    fn local_addr(&self) -> Option<SocketAddr> {
        self.local_addr
    }

    fn close(&self) {
        self.intake.0.close();
    }

    fn handshake_stats(&self) -> Option<HandshakeStats> {
        Some(self.intake.0.stats())
    }
}

/// One connection's upgrade (its own task; the intake puts the
/// [`WS_HANDSHAKE_TIMEOUT`] deadline and the door around it).
async fn upgrade(
    stream: tokio::net::TcpStream,
    peer: SocketAddr,
    max_message_bytes: usize,
    mapping: WsMessageMapping,
    metrics: crate::TransportMetrics,
) -> io::Result<Endpoint> {
    stream.set_nodelay(true)?;
    let upgraded = perform_upgrade(stream).await?;
    let (read_half, write_half) = upgraded.into_split();
    Ok(make_endpoint(
        read_half,
        write_half,
        peer,
        max_message_bytes,
        mapping,
        metrics,
    ))
}

/// Same shape as tcp/tls `make_endpoint`: build the pump-facing
/// reader/writer pair plus the one extra socket-writer task the WS
/// adapter needs (module docs). Idle-timeout support comes for free —
/// `spawn_pumps` wraps our reader's pending reads like any other.
fn make_endpoint(
    read_half: OwnedReadHalf,
    write_half: OwnedWriteHalf,
    peer: SocketAddr,
    max_message_bytes: usize,
    mapping: WsMessageMapping,
    metrics: crate::TransportMetrics,
) -> Endpoint {
    Endpoint::new(
        move |conn: ConnectionId,
              in_tx: Mailbox<ConnIn>,
              out_rx: Inbox<FrameBatch>,
              timeouts: crate::pump::PumpTimeouts| {
            // The socket-writer task is detached on purpose: it is an
            // implementation detail of the adapter, owned by nobody
            // above the pump layer; it exits by itself when every
            // queue end is dropped.
            let (queue_tx, written) = spawn_socket_writer(write_half);
            let closing = Arc::new(AtomicBool::new(false));
            let reader = WsReader::new(
                read_half,
                max_message_bytes,
                mapping,
                queue_tx.clone(),
                closing.clone(),
                metrics,
            );
            let writer = WsWriter::new(queue_tx, mapping, closing, written);
            let (read, write) = spawn_pumps(conn, reader, writer, in_tx, out_rx, timeouts);
            (Some(read), write)
        },
    )
    .with_peer(peer)
}
