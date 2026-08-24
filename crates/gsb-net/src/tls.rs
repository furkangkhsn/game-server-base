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
//! - **Handshake timeout**: a slow/hostile client can hold an accept slot
//!   only for [`HANDSHAKE_TIMEOUT`]; then the socket is dropped. This is
//!   the same family as the reader-pump idle window — a bounded wait on
//!   one awaited source, no timer task.
//! - **No silent fallback**: malformed/missing cert or key files fail the
//!   BIND with a clear error naming the file. A server that meant to be
//!   secure must never come up plaintext by accident.

use std::io;
use std::io::BufReader;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;
use tracing::debug;
use tracing::warn;

use gsb_core::channel::{FrameBatch, Inbox, Mailbox};
use gsb_core::conn::ConnIn;
use gsb_core::id::ConnectionId;

use crate::framed::FrameReader;
use crate::framed::FrameWriter;
use crate::pump::spawn_pumps;
use crate::transport::{BoxFuture, Endpoint, Listener, Transport};

/// How long a client may spend in the TLS handshake before the server
/// drops the socket. A config-free constant (like the rUDP MTU): long
/// enough for any legitimate round-trip over a WAN, short enough that a
/// connection flood of hand-shy clients cannot pin accept slots forever.
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
    listener: TcpListener,
    acceptor: TlsAcceptor,
    max_frame_bytes: usize,
}

/// Open a PEM file with the path in the error message (a bare `NotFound`
/// without the filename tells the operator nothing).
fn open_pem(path: &str, what: &str) -> io::Result<BufReader<std::fs::File>> {
    let file = std::fs::File::open(path).map_err(|e| {
        io::Error::new(
            e.kind(),
            format!("cannot open {what} file `{path}`: {e}"),
        )
    })?;
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
                format!("malformed private-key PEM in `tls_key` file `{}`: {e}", cfg.key_pem),
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
            let listener = TcpListener::bind(addr).await?;
            debug!(%addr, "TLS listener bound (rustls over TCP)");
            Ok(Arc::new(TlsListenerHandle {
                listener,
                acceptor: TlsAcceptor::from(Arc::new(server_config)),
                max_frame_bytes: self.config.max_frame_bytes,
            }) as Arc<dyn Listener>)
        })
    }
}

impl Listener for TlsListenerHandle {
    fn accept(self: Arc<Self>) -> BoxFuture<'static, io::Result<Endpoint>> {
        Box::pin(async move {
            let (stream, peer) = self.listener.accept().await?;
            stream.set_nodelay(true)?;
            // ONE awaited source wrapped in a deadline: the deadline fires
            // only while the handshake stays pending (the pump-timeout
            // idiom — no multiplexing).
            match tokio::time::timeout(HANDSHAKE_TIMEOUT, self.acceptor.accept(stream)).await {
                Ok(Ok(tls)) => {
                    debug!(%peer, "TLS handshake completed");
                    Ok(self.make_endpoint(tls, peer))
                }
                Ok(Err(e)) => {
                    // The client's failure (bad TLS, no shared cipher, a
                    // plaintext probe hitting this port). Reported so the
                    // accept loop backs off briefly; the socket is dropped.
                    warn!(%peer, error = %e, "TLS handshake failed; closing");
                    Err(io::Error::other(format!("TLS handshake failed: {e}")))
                }
                Err(_) => {
                    warn!(%peer, timeout = ?HANDSHAKE_TIMEOUT, "TLS handshake timed out; closing");
                    Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        format!("TLS handshake exceeded {:?}", HANDSHAKE_TIMEOUT),
                    ))
                }
            }
        })
    }

    fn local_addr(&self) -> Option<SocketAddr> {
        self.listener.local_addr().ok()
    }
}

impl TlsListenerHandle {
    /// Same wiring as TCP's `make_endpoint`: split the stream halves and
    /// hand them to the shared generic framing + pumps. The rustls halves
    /// implement `AsyncRead`/`AsyncWrite`, so NOTHING below this point
    /// knows TLS is involved.
    fn make_endpoint(&self, stream: TlsStreamOf, peer: SocketAddr) -> Endpoint {
        let (read_half, write_half) = tokio::io::split(stream);
        let max_frame_bytes = self.max_frame_bytes;
        // The reader handle is `Some`: like TCP, TLS has a per-connection
        // read half owned by this endpoint's reader pump.
        Endpoint::new(
            move |conn: ConnectionId,
                  in_tx: Mailbox<ConnIn>,
                  out_rx: Inbox<FrameBatch>,
                  idle_timeout: Option<Duration>| {
                let reader = FrameReader::new(read_half, max_frame_bytes);
                let writer = FrameWriter::new(write_half);
                let (read, write) =
                    spawn_pumps(conn, reader, writer, in_tx, out_rx, idle_timeout);
                (Some(read), write)
            },
        )
        .with_peer(peer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use futures::SinkExt;
    use futures::StreamExt;

    use crate::tcp::DEFAULT_MAX_FRAME_BYTES;
    use gsb_core::channel::channel;
    use gsb_protocol::FrameBody;

    /// A minted mini-PKI: CA + localhost-SANed server cert, PEM files in a
    /// fresh temp dir. Every test mints its own (docs/SECURITY.md §2
    /// decision 5: no certificate is ever committed to the repository).
    struct TestPki {
        /// Kept alive so the temp dir outlives the test's file reads.
        dir: std::path::PathBuf,
        cert_pem_path: String,
        key_pem_path: String,
        /// The CA's DER cert: the client's trust anchor.
        ca_der: rustls::pki_types::CertificateDer<'static>,
    }

    fn mint_pki(tag: &str) -> TestPki {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "gsb-net-tls-{tag}-{}-{n}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");

        // The mini-CA.
        let ca_key = rcgen::KeyPair::generate().expect("ca key");
        let mut ca_params = rcgen::CertificateParams::new(vec!["gsb test CA".into()])
            .expect("ca params");
        ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        let ca_cert = ca_params.self_signed(&ca_key).expect("ca cert");

        // The server leaf: localhost DNS SAN (what clients verify by).
        let server_key = rcgen::KeyPair::generate().expect("server key");
        let server_params =
            rcgen::CertificateParams::new(vec!["localhost".into()]).expect("server params");
        let server_cert = server_params
            .signed_by(&server_key, &ca_cert, &ca_key)
            .expect("server cert");

        let cert_pem_path = dir.join("cert.pem");
        let key_pem_path = dir.join("key.pem");
        std::fs::write(&cert_pem_path, server_cert.pem()).expect("write cert pem");
        std::fs::write(&key_pem_path, server_key.serialize_pem()).expect("write key pem");

        TestPki {
            dir,
            cert_pem_path: cert_pem_path.display().to_string(),
            key_pem_path: key_pem_path.display().to_string(),
            ca_der: ca_cert.der().clone(),
        }
    }

    /// A client connector trusting ONLY the minted CA.
    fn client_connector(ca: &TestPki) -> tokio_rustls::TlsConnector {
        let mut roots = rustls::RootCertStore::empty();
        roots.add(ca.ca_der.clone()).expect("CA parses");
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let config = rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .expect("protocol versions")
            .with_root_certificates(roots)
            .with_no_client_auth();
        tokio_rustls::TlsConnector::from(Arc::new(config))
    }

    fn transport_for(pki: &TestPki) -> TlsTransport {
        TlsTransport {
            config: TlsTransportConfig {
                cert_chain_pem: pki.cert_pem_path.clone(),
                key_pem: pki.key_pem_path.clone(),
                max_frame_bytes: DEFAULT_MAX_FRAME_BYTES,
            },
        }
    }

    /// Full endpoint wiring over TLS: a real handshake, a client frame
    /// decoded into the inbox by the reader pump, and a server frame
    /// written back by the writer pump — the exact path an application
    /// frame travels in production.
    #[tokio::test]
    async fn handshake_then_frames_flow_both_ways() {
        let pki = mint_pki("flow");
        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let listener = Arc::new(transport_for(&pki))
            .bind(addr)
            .await
            .expect("bind succeeds with a valid pair");
        let bound = listener.local_addr().unwrap();

        // Server side: accept → endpoint → pumps wired to channels.
        let server = tokio::spawn(async move {
            let endpoint = listener.accept().await.expect("accept + handshake");
            let (in_tx, mut in_rx) = channel::<ConnIn>(8);
            let (out_tx, out_rx) = channel::<FrameBatch>(8);
            let (read, write) = endpoint.start_pump(ConnectionId(1), in_tx.clone(), out_rx, None);
            // Echo the first received frame back.
            match in_rx.recv().await.expect("inbox open") {
                ConnIn::Frame(f) => {
                    out_tx.send(vec![f]).await.ok();
                }
                other => panic!("expected a frame, got {other:?}"),
            }
            drop(in_tx); // close the inbox: the reader pump exits
            drop(out_tx); // close outbound: the writer pump exits
            if let Some(h) = read {
                let _ = h.await;
            }
            let _ = write.await;
        });

        // Client side: connect + verify against the minted CA. The framing
        // view mirrors the server's exactly (same generic adapters over the
        // split rustls halves).
        let connector = client_connector(&pki);
        let name: rustls::pki_types::ServerName<'static> =
            "localhost".try_into().expect("dns name");
        let tcp = tokio::net::TcpStream::connect(bound).await.expect("tcp");
        let tls = connector.connect(name, tcp).await.expect("tls handshake");
        let (tls_r, tls_w) = tokio::io::split(tls);
        let mut writer = crate::framed::FrameWriter::new(tls_w);
        let mut reader = crate::framed::FrameReader::new(tls_r, DEFAULT_MAX_FRAME_BYTES);
        writer
            .send(FrameBody::new(7, b"hello".as_slice()))
            .await
            .expect("client send");
        let echoed = reader.next().await.expect("reply").expect("io");
        assert_eq!(echoed.op, 7);
        assert_eq!(echoed.payload, b"hello".as_slice());
        // Drop the client halves FIRST: the server task below waits for its
        // pumps, and the reader pump ends on the client's EOF. (Awaiting the
        // server while still holding the connection would deadlock.)
        drop(writer);
        drop(reader);
        server.await.expect("server task");
    }

    /// A bind with a nonexistent cert file fails with the path named.
    #[tokio::test]
    async fn missing_cert_file_fails_the_bind() {
        let transport = TlsTransport {
            config: TlsTransportConfig {
                cert_chain_pem: "/nonexistent/gsb-tls/cert.pem".into(),
                key_pem: "/nonexistent/gsb-tls/key.pem".into(),
                max_frame_bytes: DEFAULT_MAX_FRAME_BYTES,
            },
        };
        let result = Arc::new(transport)
            .bind("127.0.0.1:0".parse().unwrap())
            .await;
        let Err(err) = result else {
            panic!("bind with a missing cert file must fail");
        };
        let msg = err.to_string();
        assert!(
            msg.contains("/nonexistent/gsb-tls/cert.pem"),
            "the error must name the file: {msg}"
        );
    }
    /// A garbage cert file (valid file, invalid PEM) fails the bind too —
    /// and never comes up plaintext.
    #[tokio::test]
    async fn malformed_cert_file_fails_the_bind() {
        let pki = mint_pki("garbage");
        let bad = pki.dir.join("garbage.pem");
        std::fs::write(&bad, "this is not a certificate\n").unwrap();
        let transport = TlsTransport {
            config: TlsTransportConfig {
                cert_chain_pem: bad.display().to_string(),
                key_pem: pki.key_pem_path.clone(),
                max_frame_bytes: DEFAULT_MAX_FRAME_BYTES,
            },
        };
        let result = Arc::new(transport)
            .bind("127.0.0.1:0".parse().unwrap())
            .await;
        let Err(err) = result else {
            panic!("bind with a malformed cert file must fail");
        };
        assert_eq!(err.kind(), io::ErrorKind::InvalidData, "err: {err}");
    }

    /// A client trusting a DIFFERENT CA fails the handshake cleanly (the
    /// server reports a failed handshake; the client gets an alert).
    #[tokio::test]
    async fn wrong_ca_fails_the_handshake() {
        let pki = mint_pki("wrong-ca");
        let other = mint_pki("other-ca");
        let listener = Arc::new(transport_for(&pki))
            .bind("127.0.0.1:0".parse().unwrap())
            .await
            .expect("bind");

        let bound = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            // The server observes the failed handshake as an accept error.
            assert!(
                listener.accept().await.is_err(),
                "handshake must fail server-side"
            );
        });

        let connector = client_connector(&other);
        let name: rustls::pki_types::ServerName<'static> =
            "localhost".try_into().expect("dns name");
        let tcp = tokio::net::TcpStream::connect(bound).await.unwrap();
        let result = connector.connect(name, tcp).await;
        assert!(result.is_err(), "client must reject the unknown CA");
        server.await.expect("server sees the failure");
    }
}
