//! The ops HTTP surface's socket (`docs/OPS.md`): bound like every other
//! TCP door, with the server's `listen_backlog` (BACKLOG B84).

use std::net::SocketAddr;

use tokio::net::TcpListener;

use crate::config::*;

/// Parse `cfg.http_listen` and bind it with `cfg.listen_backlog`: the
/// listener and the address it got (port 0 included). Any failure —
/// a malformed address, a taken port, a backlog the socket builder
/// refuses — is [`ServerError::BadHttpListen`] naming the address.
/// Called only when the surface is enabled (a non-empty `http_listen`).
pub(in crate::boot) fn bind_ops(cfg: &Config) -> Result<(TcpListener, SocketAddr), ServerError> {
    let refused = |e: String| ServerError::BadHttpListen(cfg.http_listen.clone(), e);
    let listen: SocketAddr = cfg
        .http_listen
        .parse()
        .map_err(|e: std::net::AddrParseError| refused(e.to_string()))?;
    let listener = gsb_net::listen::bind_tcp(listen, cfg.listen_backlog)
        .map_err(|e| refused(e.to_string()))?;
    let bound = listener.local_addr().map_err(|e| refused(e.to_string()))?;
    Ok((listener, bound))
}
