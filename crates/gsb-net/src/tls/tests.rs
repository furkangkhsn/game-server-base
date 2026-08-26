//! Unit tests for [`super`] (moved out of the module file so the
//! implementation reads on its own; still a child module, so
//! `use super::*` reaches the parent's private items exactly as
//! before).

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
