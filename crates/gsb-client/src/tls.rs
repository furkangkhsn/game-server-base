//! TCP-over-TLS: the same frames over a rustls client stream. The caller
//! supplies the roots it trusts (no system store is consulted: the
//! server's CA is the caller's knowledge, a self-signed test CA works).

use std::io;
use std::sync::Arc;

use rustls::pki_types::{CertificateDer, ServerName};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;

use crate::conn::Conn;

/// The certificates in a PEM text (every `CERTIFICATE` block — a bundle
/// works).
pub fn certs_from_pem(pem: &str) -> io::Result<Vec<CertificateDer<'static>>> {
    rustls_pemfile::certs(&mut pem.as_bytes()).collect()
}

/// The rustls client config trusting exactly `roots` (the `ring`
/// provider, the workspace's pinned one; a fresh provider per config —
/// never a process-global install).
pub fn client_config(
    roots: impl IntoIterator<Item = CertificateDer<'static>>,
) -> io::Result<rustls::ClientConfig> {
    let mut store = rustls::RootCertStore::empty();
    for cert in roots {
        store
            .add(cert)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    }
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    Ok(rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(io::Error::other)?
        .with_root_certificates(store)
        .with_no_client_auth())
}

/// A connector trusting exactly `roots`.
pub fn connector(
    roots: impl IntoIterator<Item = CertificateDer<'static>>,
) -> io::Result<TlsConnector> {
    Ok(TlsConnector::from(Arc::new(client_config(roots)?)))
}

/// Run the TLS handshake over `tcp` (`TCP_NODELAY` set first) and wrap
/// the session as a [`Conn`]. The handshake happens here: a wrong CA
/// fails this call, before any frame moves.
pub async fn connect(
    tcp: TcpStream,
    connector: &TlsConnector,
    server_name: ServerName<'static>,
) -> io::Result<Conn> {
    tcp.set_nodelay(true).ok();
    let tls = connector.connect(server_name, tcp).await?;
    Ok(Conn::stream(tls))
}
