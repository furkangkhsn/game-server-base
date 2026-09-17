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

use crate::transport::{BoxFuture, Endpoint, Listener, Transport};
use crate::udp::*;

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
    /// from the OS entropy source at bind time. See [`CookieKey`].
    pub cookie_key: Option<[u8; 16]>,
}

impl Default for UdpTransportConfig {
    fn default() -> Self {
        Self {
            inbox_capacity: 1024,
            outbox_capacity: 256,
            max_datagram_bytes: DEFAULT_MAX_DATAGRAM_BYTES,
            idle_timeout: Some(Duration::from_secs(30)),
            cookie_key: None,
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
    end_rx: Receiver<Endpoint>,
    demux: JoinHandle<()>,
}

impl Transport for UdpTransport {
    fn bind(
        self: Arc<Self>,
        addr: SocketAddr,
    ) -> BoxFuture<'static, std::io::Result<Arc<dyn Listener>>> {
        Box::pin(async move {
            let sock = Arc::new(UdpSocket::bind(addr).await?);
            // (The kernel receive queue is the first line of buffering
            // for every session — there is no per-connection socket.
            // Tuning it (SO_RCVBUF) would need the raw fd; left at the
            // system default in v1 — see ROADMAP "v1 constraints".)
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
            let cfg = self.config.clone();
            let demux = tokio::spawn(demux(
                sock.clone(),
                end_tx,
                key,
                cfg.inbox_capacity,
                cfg.outbox_capacity,
                cfg.max_datagram_bytes,
                cfg.idle_timeout,
            ));
            info!(%addr, %key_source, "rUDP transport bound (shared demux started)");
            Ok(Arc::new(UdpListenerHandle {
                sock,
                end_rx,
                demux,
            }) as Arc<dyn Listener>)
        })
    }
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
            let rx = self.end_rx.clone();
            match tokio::task::spawn_blocking(move || rx.recv()).await {
                Ok(Ok(endpoint)) => Ok(endpoint),
                Ok(Err(_)) | Err(_) => Err(std::io::Error::other("rUDP demux gone")),
            }
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
        self.demux.abort();
    }
}
