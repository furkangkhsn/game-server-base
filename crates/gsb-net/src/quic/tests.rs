//! Unit tests for [`super`] (moved out of the module file so the
//! implementation reads on its own; still a child module, so
//! `use super::*` reaches the parent's private items exactly as
//! before).

use super::*;

use futures::SinkExt;
use futures::StreamExt;

use crate::framed::{FrameReader, FrameWriter};
use crate::quic::config::idle_timeout;
use crate::transport::Transport;
use gsb_core::channel::FrameBatch;
use gsb_core::channel::channel;
use gsb_core::conn::ConnIn;
use gsb_core::id::ConnectionId;
use gsb_protocol::FrameBody;
use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use tracing::debug;

async fn connect(
    addr: SocketAddr,
    server_name: &str,
    ca_pem: &[u8],
) -> io::Result<(QuicReader, QuicWriter)> {
    let conn = handshake_only(addr, server_name, ca_pem).await?;
    // THE v1 contract: one bi-stream, opened immediately (see module docs).
    let (send, recv) = conn
        .open_bi()
        .await
        .map_err(|e| io::Error::other(format!("QUIC stream open failed: {e}")))?;
    debug!(%addr, %server_name, "QUIC client connected; bi-stream open");
    Ok((
        FrameReader::new(recv, crate::tcp::DEFAULT_MAX_FRAME_BYTES),
        FrameWriter::new(send::QuicSend::new(send)),
    ))
}

/// A verified QUIC connection and nothing more: no stream is opened
/// (the peer [`connect`] completes, and the one a door must not wait on).
async fn handshake_only(
    addr: SocketAddr,
    server_name: &str,
    ca_pem: &[u8],
) -> io::Result<quinn::Connection> {
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

    endpoint
        .connect(addr, server_name)
        .map_err(|e| io::Error::other(format!("QUIC connect setup failed: {e}")))?
        .await
        .map_err(|e| io::Error::other(format!("QUIC connect failed: {e}")))
}

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
    let dir = std::env::temp_dir().join(format!("gsb-net-quic-{tag}-{}-{n}", std::process::id()));
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
            max_pending_handshakes: crate::transport::DEFAULT_MAX_PENDING_HANDSHAKES,
            metrics: None,
            buffers: crate::listen::UdpBuffers::default(),
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
        let (read, write) = endpoint.start_pump(
            ConnectionId(1),
            in_tx.clone(),
            out_rx,
            crate::pump::PumpTimeouts::default(),
        );
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

/// The door (BACKLOG B16): `close` ends the parked accept.
#[tokio::test]
async fn close_ends_the_parked_accept() {
    let pki = mint_pki("door");
    let t = Arc::new(transport_for(&pki));
    let listener = t.bind("127.0.0.1:0".parse().unwrap()).await.expect("bind");
    crate::transport::door::tests::close_ends_a_parked_accept(listener).await;
}

mod buffers;
mod certs;
mod off_accept;
mod slow_reader;
