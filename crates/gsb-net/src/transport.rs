//! Transport abstraction.
//!
//! A [`Transport`] binds an address and produces a [`Listener`]; the
//! listener accepts [`Endpoint`]s; each endpoint, once started, pumps
//! frames into the connection actor and writes its outbound batches —
//! entirely inside this crate. The actor layer never touches sockets.
//!
//! The traits are object-safe and `Arc`-based, so a custom transport (e.g.
//! an rUDP-based one) can be provided as `Box<dyn Transport>` and swapped
//! in without changing any actor code.

use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;

use tokio::task::JoinHandle;

use gsb_core::channel::{FrameBatch, Inbox, Mailbox};
use gsb_core::conn::ConnIn;
use gsb_core::id::ConnectionId;

use crate::pump::PumpTimeouts;

pub(crate) mod door;
pub use door::{Door, is_listener_closed, listener_closed};

/// A boxed, 'static, Send future.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// A server endpoint: binds an address, then accepts peers.
pub trait Transport: Send + 'static {
    /// Bind the transport to `addr`.
    fn bind(
        self: Arc<Self>,
        addr: SocketAddr,
    ) -> BoxFuture<'static, std::io::Result<Arc<dyn Listener>>>;
}

/// Accepts peer [`Endpoint`]s.
pub trait Listener: Send + Sync + 'static {
    /// Wait for the next peer. Once [`Self::close`] has run, the pending
    /// accept and every later one end with [`listener_closed`] — the
    /// error an accept loop ends on ([`is_listener_closed`]; every
    /// in-tree listener keeps this through its [`Door`]). A listener that
    /// does not is stopped from outside when its loop overruns the stop
    /// grace (the composition root's backstop).
    fn accept(self: Arc<Self>) -> BoxFuture<'static, std::io::Result<Endpoint>>;

    /// The local address this listener is bound to, if applicable.
    fn local_addr(&self) -> Option<SocketAddr> {
        None
    }

    /// Stop accepting new peers (and, for transports with shared state,
    /// stop that state too): the pending [`Self::accept`] ends with
    /// [`listener_closed`] (BACKLOG B16 — the accept loop ends on it). `&self` (not `self: Arc<Self>`): the caller
    /// may hold other handles and must not be forced to consume one.
    /// Default: nothing to do — the listener's own close (when its last
    /// handle is dropped) is enough for a plain TCP listener socket.
    ///
    /// This is the trait door `DESIGN.md` §9 deferred: a graceful "close
    /// the listener" could not exist before a transport actually needed
    /// it. The rUDP transport needs it — its single shared demux task
    /// reads the one shared socket for every session and outlives every
    /// individual connection, so dropping the last `Arc` is not how it is
    /// told to stop: the listener carries a duplicate socket handle whose
    /// `shutdown` makes the demux's next read fail.
    ///
    /// What `close` must NOT do is cut the LIVE sessions short: they end
    /// through the connection-actor cascade, which is what lets each of
    /// them carry its close notice to the client first (`ERROR` code 14
    /// on a server stop — `docs/DESIGN.md` §5.6). The QUIC door is the
    /// one this rule shaped: it refuses new connections here instead of
    /// closing its endpoint (see its `close`).
    fn close(&self) {}
}

/// The pump spawner closure: hands a connection's channel ends (and the
/// session-lifecycle deadline policy) to the transport and returns the
/// reader/writer task handles.
pub type PumpSpawner = Box<
    dyn FnOnce(
            ConnectionId,
            Mailbox<ConnIn>,
            Inbox<FrameBatch>,
            PumpTimeouts,
        ) -> (Option<JoinHandle<()>>, JoinHandle<()>)
        + Send,
>;

/// A single accepted peer. Owns the I/O for that connection; the only thing
/// it exposes is [`Endpoint::start_pump`], which spawns the reader and
/// writer pump tasks wiring the socket to the connection actor.
///
/// Implementations decide how I/O is driven (split TCP halves, a UDP
/// datagram loop, …) — the actor layer cannot tell the difference.
///
/// `peer` is the peer's address, known to the transport at accept (TCP) or
/// handshake (rUDP) time. The connection actor carries it so its violation-
/// budget close signal can be acted on by a layer outside the server
/// (firewall, fail2ban, future auth) without any shared conn→addr table.
/// `None` for a transport that does not expose it (the composition root
/// then reports an unspecified address).
///
/// `in_box`/`out_box`: the connection's mailboxes. A transport that
/// establishes a session **itself** (the rUDP demux, at handshake — where
/// the inbound stream must already be routable before the accept loop has
/// run) pre-creates them and carries them here; `take_inbox`/`take_outbox`
/// hand them to the composition root. A transport that does not (TCP)
/// leaves them empty and they are created on take — the same channels the
/// accept loop used to create directly.
pub struct Endpoint {
    pump: PumpSpawner,
    peer: Option<SocketAddr>,
    in_box: Option<(Mailbox<ConnIn>, Inbox<ConnIn>)>,
    out_box: Option<(Mailbox<FrameBatch>, Inbox<FrameBatch>)>,
}

impl Endpoint {
    /// Wrap a pump spawner.
    pub fn new(
        pump: impl FnOnce(
            ConnectionId,
            Mailbox<ConnIn>,
            Inbox<FrameBatch>,
            PumpTimeouts,
        ) -> (Option<JoinHandle<()>>, JoinHandle<()>)
        + Send
        + 'static,
    ) -> Self {
        Self {
            pump: Box::new(pump) as PumpSpawner,
            peer: None,
            in_box: None,
            out_box: None,
        }
    }

    /// Record the peer address (set by the transport at accept/handshake).
    pub fn with_peer(mut self, peer: SocketAddr) -> Self {
        self.peer = Some(peer);
        self
    }

    /// The peer's address, if the transport exposes it.
    pub fn peer(&self) -> Option<SocketAddr> {
        self.peer
    }

    /// Carry a pre-created inbound mailbox (rUDP: created by the demux at
    /// handshake, before this endpoint reaches the accept loop).
    pub fn with_inbox(mut self, in_tx: Mailbox<ConnIn>, in_rx: Inbox<ConnIn>) -> Self {
        self.in_box = Some((in_tx, in_rx));
        self
    }

    /// Carry a pre-created outbound mailbox (rUDP: created by the demux at
    /// handshake; the demux keeps the sender for ACK piggyback).
    pub fn with_outbox(mut self, out_tx: Mailbox<FrameBatch>, out_rx: Inbox<FrameBatch>) -> Self {
        self.out_box = Some((out_tx, out_rx));
        self
    }

    /// Take the connection's inbound mailbox: the transport's
    /// pre-created pair if it has one, otherwise a fresh channel of the
    /// given capacity.
    pub fn take_inbox(&mut self, capacity: usize) -> (Mailbox<ConnIn>, Inbox<ConnIn>) {
        self.in_box
            .take()
            .unwrap_or_else(|| gsb_core::channel::channel(capacity))
    }

    /// Take the connection's outbound mailbox (same contract as
    /// [`Self::take_inbox`]).
    pub fn take_outbox(&mut self, capacity: usize) -> (Mailbox<FrameBatch>, Inbox<FrameBatch>) {
        self.out_box
            .take()
            .unwrap_or_else(|| gsb_core::channel::channel(capacity))
    }

    /// Spawn the reader + writer pumps for this endpoint.
    ///
    /// - `conn`: the connection id (for logging).
    /// - `in_tx`: where decoded frames go (the connection actor's inbox).
    /// - `out_rx`: where outbound batches come from (the room fan-out +
    ///   the connection actor's own control frames).
    /// - `timeouts`: the session-lifecycle deadlines, one per socket
    ///   direction — the reader's idle window and the writer's write
    ///   stall (`None` each disables; see [`PumpTimeouts`]).
    ///
    /// Returns the reader and writer task handles. The reader handle is
    /// `None` for transports whose read path is **shared across
    /// connections** (the rUDP demux: one task reads the single socket for
    /// every session and demuxes; it is owned by the listener, not by any
    /// endpoint, so no per-endpoint handle exists). When the peer goes
    /// away (or either window elapses) the tasks that do exist finish on
    /// their own.
    pub fn start_pump(
        self,
        conn: ConnectionId,
        in_tx: Mailbox<ConnIn>,
        out_rx: Inbox<FrameBatch>,
        timeouts: PumpTimeouts,
    ) -> (Option<JoinHandle<()>>, JoinHandle<()>) {
        (self.pump)(conn, in_tx, out_rx, timeouts)
    }
}
