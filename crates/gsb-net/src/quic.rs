//! The QUIC transport (docs/DISTRIBUTED.md §6b, ROADMAP "QUIC taşıması"):
//! quinn over a single UDP socket, feeding the SAME length-prefix framing
//! and the SAME reader/writer pumps as [`crate::tcp`] and [`crate::tls`].
//!
//! # Stream mapping (v1 design decision)
//!
//! **Single bi-stream + length-prefix frames = TCP semantics over QUIC;
//! datagram mode is out of scope for v1.** The client opens exactly ONE
//! bidirectional stream immediately after connecting; the server accepts
//! that stream and wraps its halves in the generic [`FrameReader`] /
//! [`FrameWriter`] adapters. Every gsb connection therefore looks byte-for-
//! byte like a TCP connection below the pump seam: ordered, reliable,
//! backpressured frames — the actor layer cannot tell QUIC from TCP,
//! which is the whole point of the [`Transport`] trait seam (same argument
//! as TLS). Per-frame QUIC streams (one stream per frame, native
//! head-of-line freedom) and unreliable datagrams would change the
//! framing contract and the pump shapes; they stay undone until the
//! rehome/0-RTT round actually needs them (docs/DISTRIBUTED.md §6b).
//!
//! # Crypto provider
//!
//! WHY rustls-on-ring again (docs/SECURITY.md §2): quinn's rustls backend
//! wraps the SAME `rustls` 0.23 crate the TLS transport already pins, and
//! each config is built with a fresh `ring` provider instance (never a
//! global install — several transports bind in one process, and a second
//! `install_default` would panic). PEM cert-chain/key loading follows
//! [`crate::tls`] exactly; a malformed or missing file fails the BIND
//! with the offending path named (no silent fallback).
//!
//! # Guardrails
//!
//! - **Handshake timeout**: a slow/hostile client can hold an accept slot
//!   only for [`HANDSHAKE_TIMEOUT`] — and the window covers the client's
//!   promised bi-stream open too: a client that handshakes and then opens
//!   nothing — or opens the stream but never writes (QUIC is lazy: an
//!   untouched stream is never put on the wire) — is just as able to pin
//!   slots as one that stalls mid-handshake. Clients speak first (AUTH)
//!   or hang up cleanly. One awaited sequence wrapped in a single
//!   deadline (the pump-timeout idiom — no multiplexing).
//! - **Protocol idle timeout**: QUIC has no FIN/half-open detection; the
//!   negotiated `max_idle_timeout` ([`IDLE_TIMEOUT`]) is the ONLY signal
//!   that a vanished peer is gone. Long-silent sessions must heartbeat —
//!   the same discipline as the TCP reader-pump idle window, which remains
//!   the session-lifecycle clock on top of this backstop.

use std::io;
use std::io::BufReader;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use tracing::debug;
use tracing::warn;

use gsb_core::channel::FrameBatch;
use gsb_core::channel::Inbox;
use gsb_core::channel::Mailbox;
use gsb_core::conn::ConnIn;
use gsb_core::id::ConnectionId;

use crate::framed::FrameReader;
use crate::framed::FrameWriter;
use crate::pump::spawn_pumps;
use crate::transport::BoxFuture;
use crate::transport::Endpoint;
use crate::transport::Listener;
use crate::transport::Transport;

/// How long a client may spend between the first packet and a fully
/// accepted bi-stream before the server drops it. Same rationale as the
/// TLS transport's constant of the same name: long enough for any
/// legitimate WAN round-trip, short enough that a flood of hand-shy
/// clients cannot pin accept slots forever.
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// The QUIC `max_idle_timeout` set on both endpoints. Why it exists at
/// all: QUIC connections do not observe FINs or cable pulls — without a
/// negotiated idle window, a vanished peer's state lives forever (the
/// same problem the rUDP demux solves with its deadline heap, solved
/// here protocol-side). Sessions that may legitimately sit silent longer
/// than this must send application heartbeats.
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(30);

/// The single ALPN protocol id both sides MUST agree on. QUIC mandates
/// ALPN negotiation (an empty mismatch is a failed handshake), so unlike
/// TLS-over-TCP this transport cannot skip protocol naming.
pub const ALPN_PROTOCOL: &[u8] = b"gsb-net/1";

/// QUIC transport configuration: paths to the server's certificate chain
/// and private key, both PEM. Loaded once at bind; a missing or malformed
/// file is a bind error (see the module docs: no silent fallback).
#[derive(Debug, Clone)]
pub struct QuicTransportConfig {
    /// Path to the PEM-encoded certificate chain (leaf first).
    pub cert_chain_pem: String,
    /// Path to the PEM-encoded private key matching the leaf certificate.
    pub key_pem: String,
    /// Maximum frame body size (same guard as TCP's `max_frame_bytes`,
    /// enforced by the shared framing codec).
    pub max_frame_bytes: usize,
}

/// QUIC transport: binds one UDP socket via quinn; each accepted QUIC
/// connection carries exactly one bidirectional stream framed like TCP.
#[derive(Clone)]
pub struct QuicTransport {
    pub config: QuicTransportConfig,
}

/// The accepted connection's reader half: length-delimited frames off the
/// server's side of the client-opened bi-stream.
type QuicReader = FrameReader<quinn::RecvStream>;
/// The accepted connection's writer half: length-prefixed frames into the
/// same bi-stream.
type QuicWriter = FrameWriter<quinn::SendStream>;

struct QuicListenerHandle {
    endpoint: quinn::Endpoint,
    max_frame_bytes: usize,
}

/// Open a PEM file with the path in the error message (same rule as the
/// TLS transport: a bare `NotFound` tells the operator nothing).
fn open_pem(path: &str, what: &str) -> io::Result<BufReader<std::fs::File>> {
    let file = std::fs::File::open(path).map_err(|e| {
        io::Error::new(
            e.kind(),
            format!("cannot open {what} file `{path}`: {e}"),
        )
    })?;
    Ok(BufReader::new(file))
}

/// Build the QUIC idle-timeout value from [`IDLE_TIMEOUT`].
fn idle_timeout() -> io::Result<quinn::IdleTimeout> {
    quinn::IdleTimeout::try_from(IDLE_TIMEOUT).map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("QUIC idle timeout out of range: {e}"),
        )
    })
}

/// Load and validate the server identity and wrap it in a quinn server
/// config. Any problem is a STARTUP failure with the offending file named
/// — see the module docs.
fn load_server_config(cfg: &QuicTransportConfig) -> io::Result<quinn::ServerConfig> {
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

    // A fresh provider instance per bind (never a global install): the
    // same reasoning as the TLS transport — several transports may bind
    // in one process, and a second process-wide install would panic.
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut tls = rustls::ServerConfig::builder_with_provider(provider)
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
                    "invalid certificate/key pair (`{}` vs `{}`): {e}",
                    cfg.cert_chain_pem, cfg.key_pem
                ),
            )
        })?;
    tls.alpn_protocols = vec![ALPN_PROTOCOL.to_vec()];

    // QUIC needs TLS 1.3 with a QUIC-capable suite; the conversion fails
    // loudly otherwise (it cannot fall back silently).
    let crypto = quinn::crypto::rustls::QuicServerConfig::try_from(tls).map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("QUIC TLS setup failed (TLS 1.3 required): {e}"),
        )
    })?;
    let mut server_config = quinn::ServerConfig::with_crypto(Arc::new(crypto));
    let mut transport = quinn::TransportConfig::default();
    transport.max_idle_timeout(Some(idle_timeout()?));
    server_config.transport_config(Arc::new(transport));
    Ok(server_config)
}

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
            Ok(Arc::new(QuicListenerHandle {
                endpoint,
                max_frame_bytes: self.config.max_frame_bytes,
            }) as Arc<dyn Listener>)
        })
    }
}

impl Listener for QuicListenerHandle {
    fn accept(self: Arc<Self>) -> BoxFuture<'static, io::Result<Endpoint>> {
        Box::pin(async move {
            let Some(incoming) = self.endpoint.accept().await else {
                return Err(io::Error::other("QUIC endpoint closed"));
            };
            let peer = incoming.remote_address();
            // ONE awaited sequence wrapped in a single deadline: handshake
            // THEN the client's promised bi-stream. The deadline covers
            // both — a client that completes the handshake and then never
            // opens a stream is equally able to pin an accept slot. The
            // deadline fires only while the sequence stays pending (the
            // pump-timeout idiom — no multiplexing).
            match tokio::time::timeout(HANDSHAKE_TIMEOUT, async {
                let conn = incoming.await?;
                let (send, recv) = conn.accept_bi().await?;
                Ok::<_, quinn::ConnectionError>((send, recv))
            })
            .await
            {
                Ok(Ok((send, recv))) => {
                    debug!(%peer, "QUIC connection accepted; bi-stream open");
                    Ok(self.make_endpoint(send, recv, peer))
                }
                Ok(Err(e)) => {
                    // The client's failure (bad cert, unknown CA, no
                    // shared ALPN, stream refused). Reported so the accept
                    // loop can back off briefly; the connection is dropped.
                    warn!(%peer, error = %e, "QUIC handshake failed; closing");
                    Err(io::Error::other(format!("QUIC handshake failed: {e}")))
                }
                Err(_) => {
                    warn!(%peer, timeout = ?HANDSHAKE_TIMEOUT, "QUIC handshake timed out; closing");
                    Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        format!("QUIC handshake exceeded {:?}", HANDSHAKE_TIMEOUT),
                    ))
                }
            }
        })
    }

    fn local_addr(&self) -> Option<SocketAddr> {
        self.endpoint.local_addr().ok()
    }

    /// Graceful-shutdown door (see the trait doc): one UDP socket serves
    /// every connection, so the endpoint IS shared state — closing it
    /// notifies every live connection and makes pending `accept`s yield.
    /// Dropping the last handle alone would leave connections running
    /// until their idle timeouts.
    fn close(&self) {
        self.endpoint.close(quinn::VarInt::from_u32(0), b"");
    }
}

impl QuicListenerHandle {
    /// Same wiring as TCP/TLS `make_endpoint`: hand the stream halves to
    /// the shared generic framing + pumps. quinn's `RecvStream`/
    /// `SendStream` implement `AsyncRead`/`AsyncWrite`, so NOTHING below
    /// this point knows QUIC is involved. The `Connection` handle itself
    /// is intentionally dropped here: the streams pin the connection's
    /// shared state, so it lives as long as its pumps do.
    fn make_endpoint(
        &self,
        send: quinn::SendStream,
        recv: quinn::RecvStream,
        peer: SocketAddr,
    ) -> Endpoint {
        let max_frame_bytes = self.max_frame_bytes;
        // The reader handle is `Some`: like TCP, QUIC-v1 has a
        // per-connection read half owned by this endpoint's reader pump.
        Endpoint::new(
            move |conn: ConnectionId,
                  in_tx: Mailbox<ConnIn>,
                  out_rx: Inbox<FrameBatch>,
                  idle_timeout: Option<Duration>| {
                let reader: QuicReader = FrameReader::new(recv, max_frame_bytes);
                let writer: QuicWriter = FrameWriter::new(send);
                let (read, write) =
                    spawn_pumps(conn, reader, writer, in_tx, out_rx, idle_timeout);
                (Some(read), write)
            },
        )
        .with_peer(peer)
    }
}

/// Test-only client helper: connect to a QUIC server verified against
/// `ca_pem`, open the single bi-stream, and return the SAME framed view
/// the server-side endpoint produces (mirrors the TLS tests' connector).
///
/// The internally-created quinn [`quinn::Endpoint`] is dropped on return:
/// quinn's driver task keeps serving a connection until its last stream
/// handle is gone, so the framed pair stays fully usable afterwards.
#[cfg(test)]
async fn connect(
    addr: SocketAddr,
    server_name: &str,
    ca_pem: &[u8],
) -> io::Result<(QuicReader, QuicWriter)> {
    let mut roots = rustls::RootCertStore::empty();
    for der in rustls_pemfile::certs(&mut &ca_pem[..]) {
        let der = der.map_err(|e| {
            io::Error::new(io::ErrorKind::InvalidData, format!("malformed CA PEM: {e}"))
        })?;
        roots.add(der).map_err(|e| {
            io::Error::new(io::ErrorKind::InvalidData, format!("unusable CA cert: {e}"))
        })?;
    }

    // Fresh ring provider per connect (never a global install): see the
    // module docs.
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut tls = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| io::Error::other(format!("TLS protocol-version setup failed: {e}")))?
        .with_root_certificates(roots)
        .with_no_client_auth();
    tls.alpn_protocols = vec![ALPN_PROTOCOL.to_vec()];
    let crypto = quinn::crypto::rustls::QuicClientConfig::try_from(tls)
        .map_err(|e| io::Error::other(format!("QUIC TLS setup failed: {e}")))?;
    let mut client_config = quinn::ClientConfig::new(Arc::new(crypto));
    let mut transport = quinn::TransportConfig::default();
    transport.max_idle_timeout(Some(idle_timeout()?));
    client_config.transport_config(Arc::new(transport));

    // Bind the wildcard address matching the server's family (a v6-only
    // socket cannot reach a v4 loopback target).
    let local = SocketAddr::new(
        if addr.is_ipv4() {
            std::net::Ipv4Addr::UNSPECIFIED.into()
        } else {
            std::net::Ipv6Addr::UNSPECIFIED.into()
        },
        0,
    );
    let mut endpoint = quinn::Endpoint::client(local)?;
    endpoint.set_default_client_config(client_config);

    let conn = endpoint
        .connect(addr, server_name)
        .map_err(|e| io::Error::other(format!("QUIC connect setup failed: {e}")))?
        .await
        .map_err(|e| io::Error::other(format!("QUIC connect failed: {e}")))?;
    // THE v1 contract: one bi-stream, opened immediately (see module docs).
    let (send, recv) = conn
        .open_bi()
        .await
        .map_err(|e| io::Error::other(format!("QUIC stream open failed: {e}")))?;
    debug!(%addr, %server_name, "QUIC client connected; bi-stream open");
    Ok((
        FrameReader::new(recv, crate::tcp::DEFAULT_MAX_FRAME_BYTES),
        FrameWriter::new(send),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    use futures::SinkExt;
    use futures::StreamExt;

    use gsb_core::channel::channel;
    use gsb_protocol::FrameBody;

    /// A minted mini-PKI: CA + localhost-SANed server cert, PEM in a fresh
    /// temp dir. Every test mints its own (docs/SECURITY.md §2 decision 5:
    /// no certificate is ever committed to the repository).
    struct TestPki {
        /// Kept alive (never read) so the temp dir outlives the test's
        /// file reads.
        _dir: std::path::PathBuf,
        cert_pem_path: String,
        key_pem_path: String,
        /// The CA's PEM: the client's trust anchor.
        ca_pem: String,
    }

    fn mint_pki(tag: &str) -> TestPki {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "gsb-net-quic-{tag}-{}-{n}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");

        // The mini-CA.
        let ca_key = rcgen::KeyPair::generate().expect("ca key");
        let mut ca_params =
            rcgen::CertificateParams::new(vec!["gsb test CA".into()]).expect("ca params");
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
        let ca_pem_path = dir.join("ca.pem");
        std::fs::write(&cert_pem_path, server_cert.pem()).expect("write cert pem");
        std::fs::write(&key_pem_path, server_key.serialize_pem()).expect("write key pem");
        std::fs::write(&ca_pem_path, ca_cert.pem()).expect("write ca pem");

        TestPki {
            _dir: dir,
            cert_pem_path: cert_pem_path.display().to_string(),
            key_pem_path: key_pem_path.display().to_string(),
            ca_pem: ca_cert.pem(),
        }
    }

    fn transport_for(pki: &TestPki) -> QuicTransport {
        QuicTransport {
            config: QuicTransportConfig {
                cert_chain_pem: pki.cert_pem_path.clone(),
                key_pem: pki.key_pem_path.clone(),
                max_frame_bytes: crate::tcp::DEFAULT_MAX_FRAME_BYTES,
            },
        }
    }

    /// Full endpoint wiring over QUIC: a real handshake, the client's
    /// bi-stream opened by the helper, a client frame decoded into the
    /// inbox by the reader pump, and a server frame written back by the
    /// writer pump — the exact path an application frame travels in
    /// production.
    #[tokio::test]
    async fn handshake_then_frames_flow_both_ways_over_quic() {
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
            let (read, write) =
                endpoint.start_pump(ConnectionId(1), in_tx.clone(), out_rx, None);
            // Echo the first received frame back.
            match in_rx.recv().await.expect("inbox open") {
                ConnIn::Frame(f) => {
                    out_tx.send(vec![f]).await.ok();
                }
                other => panic!("expected a frame, got {other:?}"),
            }
            drop(in_tx); // close the inbox: the reader pump exits
            drop(out_tx); // close outbound: the writer pump exits (FIN)
            if let Some(h) = read {
                let _ = h.await;
            }
            let _ = write.await;
        });

        // Client side: connect + verify against the minted CA, then speak
        // frames over the single bi-stream.
        let (mut reader, mut writer) = connect(bound, "localhost", pki.ca_pem.as_bytes())
            .await
            .expect("connect");
        writer
            .send(FrameBody::new(7, b"hello".as_slice()))
            .await
            .expect("client send");
        let echoed = reader.next().await.expect("reply").expect("io");
        assert_eq!(echoed.op, 7);
        assert_eq!(echoed.payload, b"hello".as_slice());
        // Drop the client halves FIRST: the server task below waits for
        // its pumps, and the reader pump ends on the client's stream FIN.
        // (Awaiting the server while still holding the connection would
        // deadlock.)
        drop(writer);
        drop(reader);
        server.await.expect("server task");
    }

    /// Self-signed certs minted at runtime (rcgen) bind cleanly, and a
    /// client trusting the minted CA connects and sees the peer address.
    #[tokio::test]
    async fn self_signed_certs_connect_successfully() {
        let pki = mint_pki("self-signed");
        let listener = Arc::new(transport_for(&pki))
            .bind("127.0.0.1:0".parse().unwrap())
            .await
            .expect("bind");
        let bound = listener.local_addr().unwrap();

        let server = tokio::spawn(async move {
            let endpoint = listener.accept().await.expect("accept");
            endpoint.peer()
        });
        let (reader, mut writer) = connect(bound, "localhost", pki.ca_pem.as_bytes())
            .await
            .expect("connect");
        // A bare `open_bi` puts nothing on the wire: QUIC is lazy, the
        // stream-open frame rides the next outgoing packet, so a client
        // that opens the stream and then stays SILENT would — by design —
        // hold the server inside [`HANDSHAKE_TIMEOUT`] (see the module
        // docs). Close the writer cleanly: the FIN carries the stream-open
        // across, the server's accept completes, and the rcgen-minted
        // cert path is proven end to end.
        writer.close().await.expect("clean client close");
        let peer = server.await.expect("server task").expect("peer recorded");
        assert_eq!(peer.ip().to_string(), "127.0.0.1");
        drop(reader);
    }

    /// A client trusting a DIFFERENT CA fails the handshake cleanly (the
    /// server reports a failed handshake; the client gets a connect
    /// error) — QUIC's TLS layer rejects before any stream can open.
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

        let result = connect(bound, "localhost", other.ca_pem.as_bytes()).await;
        assert!(result.is_err(), "client must reject the unknown CA");
        server.await.expect("server sees the failure");
    }

    /// A bind with a nonexistent cert file fails with the path named.
    #[tokio::test]
    async fn missing_cert_file_fails_the_bind() {
        let transport = QuicTransport {
            config: QuicTransportConfig {
                cert_chain_pem: "/nonexistent/gsb-quic/cert.pem".into(),
                key_pem: "/nonexistent/gsb-quic/key.pem".into(),
                max_frame_bytes: crate::tcp::DEFAULT_MAX_FRAME_BYTES,
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
            msg.contains("/nonexistent/gsb-quic/cert.pem"),
            "the error must name the file: {msg}"
        );
    }
}
