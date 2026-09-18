//! The accept path: one loop per listener, one connection id
//! sequence shared by every door.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::config::*;
use gsb_core::auth::TicketAuth;
use gsb_core::channel::Mailbox;
use gsb_core::conn::ConnectionActor;
use gsb_core::id::ConnectionId;
use gsb_core::metrics::MetricsEvent;
use gsb_core::registry::RegistryMsg;
use gsb_net::quic::{QuicTransport, QuicTransportConfig};
use gsb_net::tcp::TcpTransport;
use gsb_net::tls::{TlsTransport, TlsTransportConfig};
use gsb_net::transport::Transport;
use gsb_net::udp::{UdpTransport, UdpTransportConfig};
use gsb_net::ws::WsTransport;
use gsb_protocol::MessageTable;

/// The server-wide connection-id sequence: ONE monotonic counter shared by
/// every accept task.
///
/// WHY a central counter instead of per-listener ranges or registry-assigned
/// ids: the registry (and every room) keys connections by [`ConnectionId`],
/// so a collision across two doors would silently re-route one client's
/// frames into another's inbox — uniqueness is a correctness invariant, not
/// a naming nicety. A single atomic fetch_add gives it with zero contention
/// concerns (one relaxed RMW per connection birth; accepts are human-scale
/// events even at load) and keeps ids dense from 1 like the single-loop era
/// did (`c1`, `c2`, … in logs/metrics stay interpretable). Registry-minted
/// ids were rejected because they would put a control-plane round trip on
/// the accept hot path and couple transport intake to registry liveness;
/// per-listener ranges were rejected because they leak listener identity
/// into the id space and complicate the cap accounting for no benefit.
pub(super) struct ConnIdSeq {
    /// The LAST minted raw value (0 = nothing minted yet, so the first
    /// connection gets `ConnectionId(1)` exactly as the old per-loop
    /// counter did).
    pub(super) last: AtomicU64,
}

impl ConnIdSeq {
    /// A fresh sequence starting below the first connection id.
    pub(super) fn new() -> Self {
        Self {
            last: AtomicU64::new(0),
        }
    }

    /// Mint the next unique id. `Relaxed`: the counter synchronizes nothing
    /// but its own monotonicity — no other memory is published through it —
    /// and atomics never go backwards, so distinct mints are distinct ids.
    fn mint(&self) -> ConnectionId {
        ConnectionId(self.last.fetch_add(1, Ordering::Relaxed) + 1)
    }
}

/// Everything ONE accept loop needs to push an accepted endpoint through
/// the shared pipeline: the control plane, the metrics producer, the wire
/// table, the auth hook and the channel-capacity policy. Cloned per
/// listener (senders and `Arc`s — cheap); immutable after construction, so
/// sharing needs no synchronization beyond the clone itself.
#[derive(Clone)]
pub(super) struct AcceptPipeline {
    /// Where `ConnOpened` goes (cloned again per spawned actor).
    pub(super) registry: Mailbox<RegistryMsg>,
    /// Metrics producer handle for connection actors.
    pub(super) metrics: mpsc::Sender<MetricsEvent>,
    /// The decoded-frame dispatch table (base + demo game ops).
    pub(super) table: Arc<MessageTable>,
    /// Ticket-validation hook (`None` = local auth), cloned per connection.
    pub(super) ticket_auth: Option<TicketAuth>,
    /// Inbound mailbox capacity (endpoint fallback + actor construction).
    pub(super) conn_inbox: usize,
    /// Outbound channel capacity (same contract as `conn_inbox`).
    pub(super) conn_out: usize,
    /// The session-lifecycle deadlines handed to the pumps: the reader's
    /// idle window and the writer's write stall (each `None` disables).
    pub(super) timeouts: gsb_net::pump::PumpTimeouts,
    /// THE shared id sequence across every listener's loop.
    pub(super) conn_ids: Arc<ConnIdSeq>,
}

/// One listener's accept loop: take the next endpoint, mint a globally
/// unique connection id, then hand the endpoint to the ordinary pipeline
/// (pumps → `ConnOpened` → connection actor) — byte-for-byte the flow the
/// single-listener era ran, just entered from N doors. Runs until the task
/// is aborted by `ServerHandle::stop` (after `Listener::close` made
/// `accept` fail fast on transports with shared state).
pub(super) async fn run_accept(
    pipeline: AcceptPipeline,
    listener: Arc<dyn gsb_net::transport::Listener>,
    addr: SocketAddr,
) {
    info!(%addr, "accepting connections");
    loop {
        // `accept` consumes the Arc; clone it per iteration.
        let l = Arc::clone(&listener);
        let mut endpoint = match l.accept().await {
            Ok(endpoint) => endpoint,
            Err(e) => {
                warn!(%e, "accept error; backing off");
                // Back off: a persistent error (e.g. EMFILE) must not
                // turn this loop into a CPU-burning spin.
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                continue;
            }
        };

        let conn = pipeline.conn_ids.mint();

        // The peer address (for the connection actor's violation-close
        // signal — see `gsb_core::conn`); a transport that does not
        // expose one reports an unspecified address.
        let peer = endpoint
            .peer()
            .unwrap_or_else(|| std::net::SocketAddr::from(([0, 0, 0, 0], 0)));

        // The connection's mailboxes, from the endpoint: pre-created
        // by a transport that establishes the session itself (rUDP:
        // at handshake, before this loop runs), created here for a
        // transport that does not (TCP — the exact channels this loop
        // used to create directly).
        let (in_tx, in_rx) = endpoint.take_inbox(pipeline.conn_inbox);
        let (out_tx, out_rx) = endpoint.take_outbox(pipeline.conn_out);

        // Reader + writer pumps (they finish on their own when the
        // peer or the actor goes away; the idle window, if enabled,
        // is what detects a half-open peer that sends nothing, and the
        // write stall the peer that stops reading).
        let _pumps = endpoint.start_pump(conn, in_tx.clone(), out_rx, pipeline.timeouts);

        // Register before spawning the actor: the registry owns the
        // notification path, and it must know the inbox before any
        // frame can reach the actor.
        let _ = pipeline
            .registry
            .send(RegistryMsg::ConnOpened {
                conn,
                inbox: in_tx.clone(),
            })
            .await;

        // One cheap sender clone per connection (unbounded sender is
        // an Arc).
        tokio::spawn(
            ConnectionActor::new(
                conn,
                peer,
                Arc::clone(&pipeline.table),
                pipeline.registry.clone(),
                in_rx,
                out_tx,
                pipeline.metrics.clone(),
                pipeline.ticket_auth.clone(),
            )
            .run(),
        );
    }
}

/// Build and bind ONE listener from a validated spec. The transport
/// instance is per-listener ON PURPOSE even for two entries of the same
/// kind: each door owns its socket (and, for rUDP, its own demux state),
/// so closing one listener can never disturb another's sessions.
pub(super) async fn bind_listener(
    spec: &ListenerSpec,
    cfg: &Config,
    idle_timeout: Option<std::time::Duration>,
    cookie_key: Option<[u8; 16]>,
) -> Result<(Arc<dyn gsb_net::transport::Listener>, SocketAddr), ServerError> {
    let transport: Arc<dyn Transport> = match spec {
        ListenerSpec::Tcp { .. } => Arc::new(TcpTransport {
            max_frame_bytes: cfg.max_frame_bytes,
        }),
        ListenerSpec::Tls {
            cert_pem, key_pem, ..
        } => Arc::new(TlsTransport {
            config: TlsTransportConfig {
                cert_chain_pem: cert_pem.clone(),
                key_pem: key_pem.clone(),
                max_frame_bytes: cfg.max_frame_bytes,
            },
        }),
        ListenerSpec::Udp { .. } => Arc::new(UdpTransport {
            config: UdpTransportConfig {
                // The demux pre-creates the mailboxes at handshake: same
                // capacities as the TCP path (cfg.conn_inbox/conn_out are
                // the fallbacks `Endpoint::take_*` would use).
                inbox_capacity: cfg.conn_inbox,
                outbox_capacity: cfg.conn_out,
                max_datagram_bytes: cfg.udp_max_datagram_bytes,
                idle_timeout,
                cookie_key,
            },
        }),
        ListenerSpec::Quic {
            cert_pem, key_pem, ..
        } => Arc::new(QuicTransport {
            config: QuicTransportConfig {
                cert_chain_pem: cert_pem.clone(),
                key_pem: key_pem.clone(),
                max_frame_bytes: cfg.max_frame_bytes,
            },
        }),
        ListenerSpec::Ws { .. } => Arc::new(WsTransport {
            // The global frame cap bounds the FRAME BODY (op + payload);
            // one WS message carries exactly that body behind its own
            // 4-byte length prefix (the wire contract), so the WS-level
            // ceiling is the body cap plus exactly that prefix. Derived,
            // not a separate knob, on purpose: a frame legal on every
            // other door must be legal here too, and per-listener
            // overrides would fork the pipeline's semantics per door
            // (see `ListenerEntry`).
            max_message_bytes: cfg.max_frame_bytes.saturating_add(4),
        }),
    };
    let listener = transport.bind(spec.addr()).await?;
    let addr = listener.local_addr().ok_or_else(|| {
        ServerError::BadBind(
            spec.addr().to_string(),
            "listener reports no address".into(),
        )
    })?;
    Ok((listener, addr))
}
