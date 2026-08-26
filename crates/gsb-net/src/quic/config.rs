//! Server identity: the PEM chain/key the listener binds with, and the
//! quinn transport parameters derived from this crate's timeouts.

use std::io;
use std::io::BufReader;
use std::sync::Arc;



use crate::quic::*;

/// Open a PEM file with the path in the error message (same rule as the
/// TLS transport: a bare `NotFound` tells the operator nothing).
pub(super) fn open_pem(path: &str, what: &str) -> io::Result<BufReader<std::fs::File>> {
    let file = std::fs::File::open(path).map_err(|e| {
        io::Error::new(
            e.kind(),
            format!("cannot open {what} file `{path}`: {e}"),
        )
    })?;
    Ok(BufReader::new(file))
}

/// Build the QUIC idle-timeout value from [`IDLE_TIMEOUT`].
pub(super) fn idle_timeout() -> io::Result<quinn::IdleTimeout> {
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
pub(super) fn load_server_config(cfg: &QuicTransportConfig) -> io::Result<quinn::ServerConfig> {
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
