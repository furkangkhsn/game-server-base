//! The TLS material is loaded once per run (BACKLOG B27): the CA PEM is
//! read and parsed when [`TlsOpts`] is built, never per connection.
//! Proof by removal: once the options exist, the PEM file is deleted,
//! and connections still complete their handshakes on the shared
//! connector (the per-connection read panicked on the missing file).

use std::sync::Arc;

use rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer};
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;

use super::*;

/// A test CA and a `localhost` leaf it signed: `(CA PEM, acceptor)`.
fn mint() -> (String, TlsAcceptor) {
    let ca_key = rcgen::KeyPair::generate().expect("ca key");
    let mut ca_params = rcgen::CertificateParams::new(vec!["loadgen test CA".into()]).expect("ca");
    ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    let ca = ca_params.self_signed(&ca_key).expect("ca cert");
    let key = rcgen::KeyPair::generate().expect("leaf key");
    let leaf = rcgen::CertificateParams::new(vec!["localhost".into()])
        .expect("leaf")
        .signed_by(&key, &ca, &ca_key)
        .expect("leaf cert");
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("versions")
        .with_no_client_auth()
        .with_single_cert(
            vec![leaf.der().clone()],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.serialize_der())),
        )
        .expect("server config");
    (ca.pem(), TlsAcceptor::from(Arc::new(config)))
}

#[tokio::test]
async fn the_ca_is_read_once_per_run() {
    let (ca_pem, acceptor) = mint();
    let path = std::env::temp_dir().join(format!("gsb-loadgen-ca-{}.pem", std::process::id()));
    std::fs::write(&path, ca_pem).expect("write CA PEM");
    let tls = Some(TlsOpts::load(
        path.to_str().expect("utf-8 path"),
        "localhost".into(),
    ));
    std::fs::remove_file(&path).expect("remove CA PEM");

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let server = tokio::spawn(async move {
        for _ in 0..2 {
            let (tcp, _) = listener.accept().await.expect("accept");
            let mut s = acceptor.accept(tcp).await.expect("server handshake");
            // Hold the session until the client is done with it.
            let _ = tokio::io::AsyncReadExt::read(&mut s, &mut [0u8; 1]).await;
        }
    });
    for _ in 0..2 {
        let conn = connect_wire(crate::Transport::Tcp, addr, &tls, None)
            .await
            .expect("the handshake on the shared connector");
        drop(conn);
    }
    server.await.expect("both sessions handshook");
}
