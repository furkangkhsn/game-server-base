//! QUIC: the v1 door contract of `gsb_net::quic` — TLS 1.3 with the
//! `gsb-net/1` ALPN, then exactly ONE bidirectional stream carrying the
//! same length-prefixed frames as TCP.

use std::io;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;

use rustls::pki_types::CertificateDer;

use crate::conn::Conn;

/// The quinn client config trusting exactly `roots`, with the gsb ALPN
/// set. Returned (not applied) so a caller can adjust it first — its
/// transport config, e.g. a test's deliberately tiny receive window.
pub fn client_config(
    roots: impl IntoIterator<Item = CertificateDer<'static>>,
) -> io::Result<quinn::ClientConfig> {
    let mut tls = crate::tls::client_config(roots)?;
    tls.alpn_protocols = vec![gsb_net::quic::ALPN_PROTOCOL.to_vec()];
    let crypto = quinn::crypto::rustls::QuicClientConfig::try_from(tls)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    Ok(quinn::ClientConfig::new(Arc::new(crypto)))
}

/// Connect to a QUIC door and open its one bi-stream.
///
/// The endpoint and connection handles are dropped on return on
/// purpose: quinn's driver keeps serving the connection until its last
/// stream handle is gone, so the stream pair inside the [`Conn`] is all
/// that needs holding — dropping the `Conn` ends the session.
pub async fn connect(
    addr: SocketAddr,
    server_name: &str,
    config: quinn::ClientConfig,
) -> io::Result<Conn> {
    let local: SocketAddr = if addr.is_ipv4() {
        (Ipv4Addr::UNSPECIFIED, 0).into()
    } else {
        (Ipv6Addr::UNSPECIFIED, 0).into()
    };
    let mut endpoint = quinn::Endpoint::client(local)?;
    endpoint.set_default_client_config(config);
    let conn = endpoint
        .connect(addr, server_name)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?
        .await
        .map_err(io::Error::other)?;
    let (send, recv) = conn.open_bi().await.map_err(io::Error::other)?;
    Ok(Conn::halves(Box::new(recv), Box::new(send)))
}
