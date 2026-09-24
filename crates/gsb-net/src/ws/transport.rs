//! Transport wiring: bind, accept, upgrade, then hand the actor layer
//! an [`Endpoint`] that speaks the same frames as every other door.

use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use tokio::net::TcpListener;
use tokio::net::tcp::OwnedReadHalf;
use tokio::net::tcp::OwnedWriteHalf;
use tracing::debug;
use tracing::warn;

use gsb_core::channel::FrameBatch;
use gsb_core::channel::Inbox;
use gsb_core::channel::Mailbox;
use gsb_core::conn::ConnIn;
use gsb_core::id::ConnectionId;

use crate::pump::spawn_pumps;
use crate::transport::BoxFuture;
use crate::transport::Endpoint;
use crate::transport::Listener;
use crate::transport::Transport;
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
}

impl Default for WsTransport {
    fn default() -> Self {
        Self {
            max_message_bytes: DEFAULT_MAX_MESSAGE_BYTES,
            mapping: WsMessageMapping::GameEnvelope,
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
    listener: TcpListener,
    max_message_bytes: usize,
    mapping: WsMessageMapping,
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
                mapping: self.mapping,
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
        let mapping = self.mapping;
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
                );
                let writer = WsWriter::new(queue_tx, mapping, closing, written);
                let (read, write) = spawn_pumps(conn, reader, writer, in_tx, out_rx, timeouts);
                (Some(read), write)
            },
        )
        .with_peer(peer)
    }
}
