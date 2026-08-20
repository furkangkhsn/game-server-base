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
use std::time::Duration;

use tokio::task::JoinHandle;

use gsb_core::channel::{FrameBatch, Inbox, Mailbox};
use gsb_core::conn::ConnIn;
use gsb_core::id::ConnectionId;

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
    /// Wait for the next peer.
    fn accept(self: Arc<Self>) -> BoxFuture<'static, std::io::Result<Endpoint>>;

    /// The local address this listener is bound to, if applicable.
    fn local_addr(&self) -> Option<SocketAddr> {
        None
    }
}

/// The pump spawner closure: hands a connection's channel ends (and the
/// idle-timeout policy) to the transport and returns the reader/writer
/// task handles.
type PumpSpawner = Box<
    dyn FnOnce(
            ConnectionId,
            Mailbox<ConnIn>,
            Inbox<FrameBatch>,
            Option<Duration>,
        ) -> (JoinHandle<()>, JoinHandle<()>)
        + Send,
>;

/// A single accepted peer. Owns the I/O for that connection; the only thing
/// it exposes is [`Endpoint::start_pump`], which spawns the reader and
/// writer pump tasks wiring the socket to the connection actor.
///
/// Implementations decide how I/O is driven (split TCP halves, a UDP
/// datagram loop, …) — the actor layer cannot tell the difference.
pub struct Endpoint {
    pump: PumpSpawner,
}

impl Endpoint {
    /// Wrap a pump spawner.
    pub fn new(
        pump: impl FnOnce(
            ConnectionId,
            Mailbox<ConnIn>,
            Inbox<FrameBatch>,
            Option<Duration>,
        ) -> (JoinHandle<()>, JoinHandle<()>)
        + Send
        + 'static,
    ) -> Self {
        Self {
            pump: Box::new(pump) as PumpSpawner,
        }
    }

    /// Spawn the reader + writer pumps for this endpoint.
    ///
    /// - `conn`: the connection id (for logging).
    /// - `in_tx`: where decoded frames go (the connection actor's inbox).
    /// - `out_rx`: where outbound batches come from (the room fan-out +
    ///   the connection actor's own control frames).
    /// - `idle_timeout`: the session-lifecycle idle window for the reader
    ///   (`None` disables; see `pump::spawn_pumps`).
    ///
    /// Returns the reader and writer task handles. When the peer goes away
    /// (or the idle window elapses) both tasks finish on their own.
    pub fn start_pump(
        self,
        conn: ConnectionId,
        in_tx: Mailbox<ConnIn>,
        out_rx: Inbox<FrameBatch>,
        idle_timeout: Option<Duration>,
    ) -> (JoinHandle<()>, JoinHandle<()>) {
        (self.pump)(conn, in_tx, out_rx, idle_timeout)
    }
}
