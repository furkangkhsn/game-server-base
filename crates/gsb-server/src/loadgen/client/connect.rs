//! A client session's wire: the transport-specific half of its birth,
//! shared by the plain and the churn client, and the run's TLS material.

use std::net::SocketAddr;

use gsb_client::Conn;

use super::stall::tcp_connect;

/// The client-side TLS material: the connector trusting the `--tls-ca`
/// root, and the name to expect in the server certificate. `None` =
/// plaintext TCP. Built ONCE per run ([`TlsOpts::load`]) and cloned into
/// every client: the CA PEM is read and parsed once, and every
/// connection shares one rustls client config (the connector is an
/// `Arc` inside).
#[derive(Clone)]
pub(crate) struct TlsOpts {
    pub(crate) connector: tokio_rustls::TlsConnector,
    pub(crate) server_name: String,
}

impl TlsOpts {
    /// Read and parse the CA PEM at `ca_path` into the run's connector.
    pub(crate) fn load(ca_path: &str, server_name: String) -> Self {
        Self {
            connector: tls_connector(ca_path),
            server_name,
        }
    }
}

/// Build a rustls connector trusting ONLY the CA PEM at `ca_path` (the
/// `--tls-ca` root; a self-signed test CA works — docs/SECURITY.md §2).
fn tls_connector(ca_path: &str) -> tokio_rustls::TlsConnector {
    let pem = std::fs::read_to_string(ca_path)
        .unwrap_or_else(|e| panic!("cannot read --tls-ca `{ca_path}`: {e}"));
    let certs = gsb_client::tls::certs_from_pem(&pem)
        .unwrap_or_else(|e| panic!("malformed certificate PEM in `{ca_path}`: {e}"));
    gsb_client::tls::connector(certs)
        .unwrap_or_else(|e| panic!("--tls-ca PEM is not a certificate: {e}"))
}

/// Connect one wire of the given kind (the transport-specific half of a
/// client session's birth; shared by the plain and the churn client —
/// TCP gets `nodelay` and plaintext and TLS share one wire shape
/// (`gsb_client::Conn::Stream`), rUDP runs its cookie handshake, and
/// WebSocket runs the HTTP upgrade over the same TCP socket a stream
/// client opens — the slow reader's small receive buffer included). With
/// TLS material, `connect_ms` includes the rustls handshake; on
/// WebSocket, the upgrade's round trip (the command line refuses TLS
/// with WebSocket: the gsb door has no TLS form).
pub(crate) async fn connect_wire(
    kind: crate::Transport,
    addr: SocketAddr,
    tls: &Option<TlsOpts>,
    rcvbuf: Option<u32>,
) -> std::io::Result<Conn> {
    Ok(match kind {
        crate::Transport::Udp => gsb_client::connect::udp(addr).await?,
        crate::Transport::Ws => {
            let stream = tcp_connect(addr, rcvbuf).await?;
            gsb_client::connect::ws_stream(stream, &addr.to_string()).await?
        }
        crate::Transport::Tcp => {
            let stream = tcp_connect(addr, rcvbuf).await?;
            match tls {
                None => gsb_client::connect::tcp_stream(stream),
                Some(opts) => {
                    let dns: rustls::pki_types::ServerName<'static> =
                        opts.server_name.clone().try_into().map_err(|_| {
                            std::io::Error::new(
                                std::io::ErrorKind::InvalidInput,
                                format!(
                                    "--tls-server-name `{}` is not a DNS name",
                                    opts.server_name
                                ),
                            )
                        })?;
                    // The handshake happens HERE: connect_ms covers it (the
                    // same convention as the rUDP cookie handshake above).
                    gsb_client::tls::connect(stream, &opts.connector, dns).await?
                }
            }
        }
    })
}
