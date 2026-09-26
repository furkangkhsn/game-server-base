//! Opening a [`Conn`]: one function per door. TLS and QUIC take the
//! trust roots from the caller (see [`tls`](crate::tls) and
//! [`quic`](crate::quic)); nothing here reads a file or picks a policy.

use std::io;
use std::net::SocketAddr;

use gsb_net::udp::UdpClient;
use tokio::net::TcpStream;

use crate::conn::Conn;

/// Plain TCP: connect, `TCP_NODELAY` on (a game client's small frames
/// must not wait for Nagle), split.
pub async fn tcp(addr: SocketAddr) -> io::Result<Conn> {
    let stream = TcpStream::connect(addr).await?;
    Ok(tcp_stream(stream))
}

/// Plain TCP over a stream the caller connected itself (its own socket
/// options — a load generator's small receive buffer, say). Sets
/// `TCP_NODELAY` like [`tcp`].
pub fn tcp_stream(stream: TcpStream) -> Conn {
    stream.set_nodelay(true).ok();
    let (r, w) = stream.into_split();
    Conn::halves(Box::new(r), Box::new(w))
}

/// rUDP: bind an ephemeral port and run the cookie handshake
/// (`UdpClient::connect` — returns once the server holds the session).
pub async fn udp(addr: SocketAddr) -> io::Result<Conn> {
    UdpClient::connect(addr).await.map(Conn::udp)
}
