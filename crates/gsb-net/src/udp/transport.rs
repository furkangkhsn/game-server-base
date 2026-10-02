//! The bind/accept side: the transport's configuration, its
//! [`Transport`] entry point, and the listener handle that hands out
//! endpoints as the shared demux discovers new sessions.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use crossbeam_channel::Receiver;
use tokio::net::UdpSocket;
use tokio::task::JoinHandle;
use tracing::info;

use crate::transport::{BoxFuture, Door, Endpoint, Listener, Transport, listener_closed};
use crate::udp::*;

mod queued;
pub(super) use queued::Queued;

/// rUDP transport configuration (set by the composition root from the
/// server config — the channel capacities mirror `conn_inbox`/`conn_out`
/// because the demux pre-creates them at handshake).
#[derive(Debug, Clone)]
pub struct UdpTransportConfig {
    pub inbox_capacity: usize,
    pub outbox_capacity: usize,
    /// The datagram budget (feature 3; default [`DEFAULT_MAX_DATAGRAM_BYTES`]).
    pub max_datagram_bytes: usize,
    /// The session idle window (feature 4; `None` disables the sweep).
    pub idle_timeout: Option<Duration>,
    /// Operator-supplied cookie key (16 bytes; the composition root
    /// parses the config's 32-hex-char string into these). `None` = draw
    /// from the OS entropy source at bind time. See `CookieKey`.
    pub cookie_key: Option<[u8; 16]>,
    /// Where the demux and the writers send their loss counters (B58;
    /// `None` = counted in their stop logs only).
    pub metrics: crate::TransportMetrics,
    /// The shared socket's kernel buffers (BACKLOG B4; unset = the
    /// system default, untouched). See [`crate::listen::bind_udp`].
    pub buffers: crate::listen::UdpBuffers,
    /// The writers' congestion response (rUDP hardening round 3; the
    /// server's `udp_congestion`). `Off` (the default): the writer as it
    /// was. See `crate::udp::congestion`.
    pub congestion: UdpCongestion,
    /// Connection migration (BACKLOG B3; the server's `udp_migration`):
    /// `true` grants a connection id to every client that asks, so its
    /// session survives an address change after path validation. `false`
    /// (the default): no CID is granted and the door is byte for byte
    /// what it was. Pre-crypto a CID is a bearer token — see
    /// `crate::udp` module `path` and `docs/RUDP-SECURITY.md` §7.
    pub migration: bool,
    /// The per-source cap on pending sessions (BACKLOG B89; the server's
    /// `max_handshakes_per_source`, D11's key): one source — an IPv4
    /// address, an IPv6 /64 — holds at most this many sessions the demux
    /// has established (a verified proof) and the accept loop has not
    /// taken yet. A verified proof over it creates nothing, gets no
    /// accept and is counted (`udp_proofs_refused_per_source`); the
    /// client re-sends it. `None` (the default) or `0`: no cap.
    pub max_handshakes_per_source: Option<usize>,
    /// The record layer (B5a, `crate::udp` module `sealed`):
    /// [`UdpSecurity::Sealed`] runs Noise NK in the cookie handshake and
    /// seals every session datagram under the given static key;
    /// [`UdpSecurity::Plaintext`] (this struct's default — a library
    /// cannot invent a server's identity) is the door before B5a. The
    /// server's own default is sealed (its `udp_security`).
    pub security: UdpSecurity,
    /// The sealed door's global handshake budget (B119; the server's
    /// `udp_handshakes_per_sec`): Diffie-Hellmans per second the demux
    /// runs, a token bucket checked after the cookie and the per-source
    /// cap. A verified proof over it creates nothing and is counted
    /// (`udp_proofs_refused_budget`); the client re-sends it. `None` or
    /// `0`: no budget. Default [`DEFAULT_HANDSHAKES_PER_SEC`].
    pub handshakes_per_sec: Option<u32>,
}

impl Default for UdpTransportConfig {
    fn default() -> Self {
        Self {
            inbox_capacity: 1024,
            outbox_capacity: 256,
            max_datagram_bytes: DEFAULT_MAX_DATAGRAM_BYTES,
            idle_timeout: Some(Duration::from_secs(30)),
            cookie_key: None,
            metrics: None,
            buffers: crate::listen::UdpBuffers::default(),
            congestion: UdpCongestion::Off,
            migration: false,
            max_handshakes_per_source: None,
            security: UdpSecurity::Plaintext,
            handshakes_per_sec: Some(DEFAULT_HANDSHAKES_PER_SEC),
        }
    }
}

/// The rUDP transport: one socket, one shared demux, many sessions.
#[derive(Debug, Clone, Default)]
pub struct UdpTransport {
    pub config: UdpTransportConfig,
}

pub(super) struct UdpListenerHandle {
    /// The shared socket (for `local_addr`; the demux and the per-session
    /// writers hold their own clones — the socket's fd outlives the
    /// listener and is released as the connections drain).
    sock: Arc<UdpSocket>,
    /// The endpoint stream from the demux. A crossbeam receiver on
    /// purpose: `recv`/`try_recv` take `&self`, so the receiver can live
    /// inside the `Arc<Self>` behind the `Listener` trait (a tokio mpsc
    /// receiver needs `&mut` — unreachable through an `Arc` without a
    /// lock, and locks are banned in this workspace).
    end_rx: Receiver<Queued>,
    demux: JoinHandle<()>,
    /// The kernel-drop watcher of the socket (B85; Linux, with metrics).
    kernel: Option<JoinHandle<()>>,
    /// Closed by [`Listener::close`]: ends the pending accept (B16).
    door: Door,
}

impl Transport for UdpTransport {
    fn bind(
        self: Arc<Self>,
        addr: SocketAddr,
    ) -> BoxFuture<'static, std::io::Result<Arc<dyn Listener>>> {
        Box::pin(async move {
            // The kernel receive queue is the first line of buffering for
            // every session — there is no per-connection socket — so its
            // size is the operator's (B4; unset = the system default).
            let sock = Arc::new(bind_socket(addr, self.config.buffers)?);
            let (end_tx, end_rx) = crossbeam_channel::bounded(ENDPOINT_CHANNEL);
            // The cookie key: the operator's config, or the OS entropy
            // source. A failure here is deliberate (see `CookieKey`): a
            // predictable key inverts the anti-amplification property,
            // so the bind errors out and the server refuses to start
            // rather than run weak.
            let key_source = if self.config.cookie_key.is_some() {
                "config"
            } else {
                "os-entropy"
            };
            let key = match self.config.cookie_key {
                Some(bytes) => CookieKey::from_bytes(bytes),
                None => CookieKey::generate().map_err(|e| {
                    std::io::Error::other(format!(
                        "{e} (supply one explicitly via the config's \
                         udp_cookie_key: 32 hex characters)"
                    ))
                })?,
            };
            // A sealed door's state (B5a): its per-door reset-token key is
            // drawn here, so an entropy failure refuses the bind like the
            // cookie key's.
            let seal = match &self.config.security {
                UdpSecurity::Sealed(k) => Some(crate::udp::sealed::DoorSeal::new(
                    k.clone(),
                    self.config.handshakes_per_sec,
                )?),
                UdpSecurity::Plaintext => None,
            };
            // Only the PUBLIC key is ever logged (the clients pin it).
            let public = self.config.security.public_key().map(hex);
            let kernel = kernel::spawn(&sock, &self.config.metrics);
            let demux = tokio::spawn(demux(sock.clone(), end_tx, key, self.config.clone(), seal));
            info!(
                %addr,
                %key_source,
                sealed = public.is_some(),
                public_key = public.as_deref().unwrap_or("-"),
                "rUDP transport bound (shared demux started)"
            );
            Ok(Arc::new(UdpListenerHandle {
                sock,
                end_rx,
                demux,
                kernel,
                door: Door::new(),
            }) as Arc<dyn Listener>)
        })
    }
}

/// Lowercase hex of a public key (the bind log's).
fn hex(b: [u8; crate::seal::KEY_LEN]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// The door's one socket: bound with the configured kernel buffers, and
/// the sizes the kernel granted logged (with a warning when it capped
/// one below the request — B4).
pub(super) fn bind_socket(
    addr: SocketAddr,
    buffers: crate::listen::UdpBuffers,
) -> std::io::Result<UdpSocket> {
    let sock = UdpSocket::from_std(crate::listen::bind_udp(addr, buffers)?)?;
    let got = crate::listen::buffer_sizes(&sock)?;
    crate::listen::log_buffers("rUDP", sock.local_addr()?, buffers, got);
    Ok(sock)
}

impl Listener for UdpListenerHandle {
    fn accept(self: Arc<Self>) -> BoxFuture<'static, std::io::Result<Endpoint>> {
        Box::pin(async move {
            // A crossbeam `recv` parks its thread, so it runs on the
            // blocking pool (one parked blocking thread per PENDING
            // accept; endpoints are rare — handshakes — so this never
            // scales with traffic). The demux side is non-blocking
            // (`try_send`), so a slow accept loop can never stall the
            // demux (and therefore every other session).
            // The door ends a pending accept at once; the parked blocking
            // thread follows when the demux, stopped by the same `close`,
            // drops the endpoint sender (an endpoint it takes on the way
            // is dropped — its session was never adopted).
            // The endpoint is adopted only once the door has let the
            // accept through: one a closed door's parked thread takes
            // drops un-adopted, and counts itself (B74, `Queued`).
            let rx = self.end_rx.clone();
            self.door
                .admit(async move {
                    match tokio::task::spawn_blocking(move || rx.recv()).await {
                        Ok(Ok(queued)) => Ok(queued),
                        // The demux is gone: this door will not open again.
                        Ok(Err(_)) => Err(listener_closed()),
                        Err(e) => Err(std::io::Error::other(format!("rUDP accept: {e}"))),
                    }
                })
                .await
                .map(Queued::into_endpoint)
        })
    }

    fn local_addr(&self) -> Option<SocketAddr> {
        self.sock.local_addr().ok()
    }

    fn close(&self) {
        // Stop the shared demux: aborting it drops its socket clone and
        // its endpoint sender (the accept loop's `recv` then fails and
        // ends). The per-session writers are not touched here: they exit
        // with their connections (the actor cascade) and release their
        // socket clones on the way.
        self.door.close();
        self.demux.abort();
        if let Some(kernel) = &self.kernel {
            // Its `Drop` reads the socket's line one last time.
            kernel.abort();
        }
        // What is queued now will never be accepted: dropped here, each
        // counting itself (B74). One the demux still queues before the
        // abort lands counts itself when the handle goes.
        while self.end_rx.try_recv().is_ok() {}
    }
}
