//! Shared test support: a runtime-minted mini-PKI for the TLS transports
//! (docs/SECURITY.md §2 decision 5). Every test mints its own CA + a
//! localhost-SANed server cert at startup; NO certificate is ever
//! committed to the repository. Not a test target itself — included by
//! the e2e suites as a plain module.

#![allow(dead_code)]

use std::path::PathBuf;

use tokio_rustls::TlsConnector;

/// The default DNS name clients verify against (what the leaf cert's SAN
/// says; SECURITY §2 decision 6).
pub const TLS_SERVER_NAME: &str = "localhost";

/// A minted identity: PEM files on disk (the server config keys are PATHS
/// — see `Config::tls_cert`) plus the CA DER for building client trust.
pub struct TlsPki {
    /// Kept so the temp dir outlives the server's bind-time reads.
    pub dir: PathBuf,
    /// Value for `Config::tls_cert`.
    pub cert_pem_path: String,
    /// Value for `Config::tls_key`.
    pub key_pem_path: String,
    /// The CA certificate (DER): the client-side trust anchor.
    pub ca_der: rustls::pki_types::CertificateDer<'static>,
}

/// Mint CA + localhost-SANed server leaf into a fresh temp dir.
pub fn mint_tls_pki(tag: &str) -> TlsPki {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("gsb-server-tls-{tag}-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");

    // The mini-CA (self-signed; unconstrained for a test root).
    let ca_key = rcgen::KeyPair::generate().expect("ca key");
    let mut ca_params =
        rcgen::CertificateParams::new(vec!["gsb test CA".into()]).expect("ca params");
    ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    let ca_cert = ca_params.self_signed(&ca_key).expect("ca cert");

    // The server leaf: carries the localhost DNS SAN clients verify.
    let server_key = rcgen::KeyPair::generate().expect("server key");
    let server_params =
        rcgen::CertificateParams::new(vec![TLS_SERVER_NAME.into()]).expect("server params");
    let server_cert = server_params
        .signed_by(&server_key, &ca_cert, &ca_key)
        .expect("server cert");

    let cert_path = dir.join("cert.pem");
    let key_path = dir.join("key.pem");
    std::fs::write(&cert_path, server_cert.pem()).expect("write cert pem");
    std::fs::write(&key_path, server_key.serialize_pem()).expect("write key pem");

    TlsPki {
        dir,
        cert_pem_path: cert_path.display().to_string(),
        key_pem_path: key_path.display().to_string(),
        ca_der: ca_cert.der().clone(),
    }
}

/// A rustls client connector trusting ONLY the minted CA (no system roots:
/// the test proves the handshake against exactly this PKI).
pub fn tls_client_connector(pki: &TlsPki) -> TlsConnector {
    let mut roots = rustls::RootCertStore::empty();
    roots.add(pki.ca_der.clone()).expect("CA parses");
    // A fresh provider instance per connector (never a global install):
    // several servers/clients are built across one test binary's run.
    let provider = std::sync::Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("protocol versions")
        .with_root_certificates(roots)
        .with_no_client_auth();
    TlsConnector::from(std::sync::Arc::new(config))
}
