//! The TLS transport (docs/SECURITY.md §2, Tur A): rustls over the same
//! TCP accept path, feeding the SAME length-prefix framing and the SAME
//! reader/writer pumps as [`crate::tcp`].
//!
//! WHY rustls (and not native-tls): pure Rust on the `ring` provider — no
//! platform OpenSSL dependency, matching the workspace's dependency
//! discipline. WHY a transport at all (and not a flag inside `tcp`): the
//! handshake is per-connection work done between `accept` and the pumps;
//! wrapping it behind the existing [`Transport`]/[`Listener`]/[`Endpoint`]
//! traits means zero actor-layer changes — the connection actor cannot tell
//! TLS from plaintext, which is exactly the point of the trait seam.
//!
//! Guardrails:
//! - **Handshake off the accept loop** (BACKLOG B31): each connection's
//!   handshake runs in its own task (`crate::transport::intake`), at most
//!   [`TlsTransportConfig::max_pending_handshakes`] at once; a connection
//!   over the bound is closed unhandshaken and counted. A silent client
//!   holds one slot, never the door.
//! - **Handshake timeout**: a slow/hostile client can hold a handshake
//!   slot only for [`HANDSHAKE_TIMEOUT`]; then the socket is dropped. This
//!   is the same family as the reader-pump idle window — a bounded wait on
//!   one awaited source, no timer task.
//! - **No silent fallback**: malformed/missing cert or key files fail the
//!   BIND with a clear error naming the file. A server that meant to be
//!   secure must never come up plaintext by accident.

use std::io;
use std::io::BufReader;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use tokio_rustls::TlsAcceptor;
use tracing::debug;

use gsb_core::channel::{FrameBatch, Inbox, Mailbox};
use gsb_core::conn::ConnIn;
use gsb_core::id::ConnectionId;

use crate::framed::FrameReader;
use crate::framed::FrameWriter;
use crate::pump::spawn_pumps;
use crate::transport::intake::{Intake, IntakeHandle, run_tcp_intake};
use crate::transport::{BoxFuture, Endpoint, HandshakeStats, Listener, Transport};

/// How long a client may spend in the TLS handshake before the server
/// drops the socket. A config-free constant (like the rUDP MTU): long
/// enough for any legitimate round-trip over a WAN, short enough that a
/// connection flood of hand-shy clients cannot pin handshake slots forever.
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// TLS transport configuration: paths to the server's certificate chain
/// and private key, both PEM. Loaded once at bind; a missing or malformed
/// file is a bind error (see the module docs: no silent plaintext).
#[derive(Debug, Clone)]
pub struct TlsTransportConfig {
    /// Path to the PEM-encoded certificate chain (leaf first).
    pub cert_chain_pem: String,
    /// Path to the PEM-encoded private key matching the leaf certificate.
    pub key_pem: String,
    /// Maximum frame body size (same guard as TCP's `max_frame_bytes`).
    pub max_frame_bytes: usize,
    /// The bound on handshakes in flight (BACKLOG B31; default
    /// [`crate::transport::DEFAULT_MAX_PENDING_HANDSHAKES`]).
    pub max_pending_handshakes: usize,
    /// Where the handshake intake sends its refusals, timeouts and
    /// failures (B58; `None` = its stop log only).
    pub metrics: crate::TransportMetrics,
    /// The listening socket's accept backlog (BACKLOG B84;
    /// [`crate::listen::DEFAULT_LISTEN_BACKLOG`] is the queue tokio's own
    /// bind gives).
    pub listen_backlog: u32,
}

/// TLS-over-TCP transport: accepts plain TCP sockets, upgrades each to
/// rustls, then runs the standard framing + pump pair over the encrypted
/// stream.
#[derive(Clone)]
pub struct TlsTransport {
    pub config: TlsTransportConfig,
}

/// One accepted TLS connection's concrete stream type.
type TlsStreamOf = tokio_rustls::server::TlsStream<tokio::net::TcpStream>;

struct TlsListenerHandle {
    local_addr: Option<SocketAddr>,
    /// The handshakes, off the accept loop: `accept` takes finished ones;
    /// [`Listener::close`] (or the last handle's drop) closes its door —
    /// the raw accept, every handshake in flight, the pending accept (B16).
    intake: IntakeHandle,
}

/// Open a PEM file with the path in the error message (a bare `NotFound`
/// without the filename tells the operator nothing).
fn open_pem(path: &str, what: &str) -> io::Result<BufReader<std::fs::File>> {
    let file = std::fs::File::open(path)
        .map_err(|e| io::Error::new(e.kind(), format!("cannot open {what} file `{path}`: {e}")))?;
    Ok(BufReader::new(file))
}

/// Load and validate the server identity: parse the PEM cert chain + key
/// and build the rustls server config. Any problem here is a STARTUP
/// failure with the offending file named — see the module docs.
fn load_server_config(cfg: &TlsTransportConfig) -> io::Result<rustls::ServerConfig> {
    let certs: Vec<rustls::pki_types::CertificateDer<'static>> =
        rustls_pemfile::certs(&mut open_pem(&cfg.cert_chain_pem, "`tls_cert`")?)
            .collect::<Result<_, _>>()
            .map_err(|e| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "malformed certificate PEM in `tls_cert` file `{}`: {e}",
                        cfg.cert_chain_pem
                    ),
                )
            })?;
    if certs.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "no certificates found in `tls_cert` file `{}`",
                cfg.cert_chain_pem
            ),
        ));
    }
    let key = rustls_pemfile::private_key(&mut open_pem(&cfg.key_pem, "`tls_key`")?)
        .map_err(|e| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "malformed private-key PEM in `tls_key` file `{}`: {e}",
                    cfg.key_pem
                ),
            )
        })?
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("no private key found in `tls_key` file `{}`", cfg.key_pem),
            )
        })?;

    // A fresh provider instance per bind (never a global install): several
    // transports may bind in one process (tests), and installing a process-
    // wide default would make the second one panic.
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("TLS protocol-version setup failed: {e}"),
            )
        })?
        .with_no_client_auth()
        // mTLS is NOT-DONE for this turn (docs/SECURITY.md §6).
        .with_single_cert(certs, key)
        .map_err(|e| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "invalid TLS certificate/key pair (`{}` vs `{}`): {e}",
                    cfg.cert_chain_pem, cfg.key_pem
                ),
            )
        })
}

impl Transport for TlsTransport {
    fn bind(
        self: Arc<Self>,
        addr: SocketAddr,
    ) -> BoxFuture<'static, io::Result<Arc<dyn Listener>>> {
        Box::pin(async move {
            // Load the identity BEFORE binding the socket: a server whose
            // keys are broken must not half-start (the port would be taken
            // while startup fails, confusing restart logic).
            let server_config = load_server_config(&self.config)?;
            let listener = crate::listen::bind_tcp(addr, self.config.listen_backlog)?;
            debug!(
                %addr,
                backlog = self.config.listen_backlog,
                "TLS listener bound (rustls over TCP)"
            );
            let local_addr = listener.local_addr().ok();
            let intake = Intake::new("TLS", self.config.max_pending_handshakes);
            let acceptor = TlsAcceptor::from(Arc::new(server_config));
            let max_frame_bytes = self.config.max_frame_bytes;
            tokio::spawn(run_tcp_intake(
                Arc::clone(&intake),
                listener,
                HANDSHAKE_TIMEOUT,
                {
                    let metrics = self.config.metrics.clone();
                    move |stream, peer| {
                        handshake(
                            acceptor.clone(),
                            stream,
                            peer,
                            max_frame_bytes,
                            metrics.clone(),
                        )
                    }
                },
                self.config.metrics.clone(),
            ));
            Ok(Arc::new(TlsListenerHandle {
                local_addr,
                intake: IntakeHandle(intake),
            }) as Arc<dyn Listener>)
        })
    }
}

impl Listener for TlsListenerHandle {
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

/// One connection's TLS handshake, in its own task under the door and
/// [`HANDSHAKE_TIMEOUT`]. A failure (bad TLS, no shared cipher, a
/// plaintext probe) is counted and logged there; the socket dropped.
async fn handshake(
    acceptor: TlsAcceptor,
    stream: tokio::net::TcpStream,
    peer: SocketAddr,
    max_frame_bytes: usize,
    metrics: crate::TransportMetrics,
) -> io::Result<Endpoint> {
    stream.set_nodelay(true)?;
    let tls = acceptor.accept(stream).await?;
    Ok(make_endpoint(tls, peer, max_frame_bytes, metrics))
}

/// Same wiring as TCP's `make_endpoint`: split the stream halves and
/// hand them to the shared generic framing + pumps. The rustls halves
/// implement `AsyncRead`/`AsyncWrite`, so NOTHING below this point
/// knows TLS is involved.
fn make_endpoint(
    stream: TlsStreamOf,
    peer: SocketAddr,
    max_frame_bytes: usize,
    metrics: crate::TransportMetrics,
) -> Endpoint {
    let (read_half, write_half) = tokio::io::split(stream);
    // The reader handle is `Some`: like TCP, TLS has a per-connection
    // read half owned by this endpoint's reader pump.
    Endpoint::new(
        move |conn: ConnectionId,
              in_tx: Mailbox<ConnIn>,
              out_rx: Inbox<FrameBatch>,
              timeouts: crate::pump::PumpTimeouts| {
            let reader = FrameReader::new(read_half, max_frame_bytes);
            let writer = FrameWriter::new(write_half);
            let (read, write) = spawn_pumps(conn, reader, writer, in_tx, out_rx, timeouts, metrics);
            (Some(read), write)
        },
    )
    .with_peer(peer)
}

#[cfg(test)]
mod tests;
