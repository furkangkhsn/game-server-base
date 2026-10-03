//! Connecting from a lobby's join grant (feature `grant`, BACKLOG B21):
//! the grant names the doors, the rUDP server key to pin, the ticket and
//! the room ([`gsb_ticket::JoinGrant`]); these open the door and run AUTH
//! + JOIN with it.
//!
//! The pin comes from the grant, never from the game server
//! (`docs/RUDP-SECURITY.md` decision 3): the client trusts the lobby it
//! reached over HTTPS, and the lobby vouches for the server's key. A
//! `udp` door without a key in the grant is refused here — a grant never
//! sends a client to a plaintext door. TLS and QUIC doors take the
//! caller's trust roots (no system store, as [`crate::tls`]) and verify
//! the door's host name.

use std::io;
use std::time::Duration;

use gsb_protocol::FrameBody;
use rustls::pki_types::{CertificateDer, ServerName};
use tokio::net::TcpStream;

use crate::conn::Conn;
use crate::error::ClientError;
use crate::session::{self, Credentials, Joined};

pub use gsb_ticket::{Door, JoinGrant, Transport};

/// The credentials the grant carries: its ticket (the identity is the
/// ticket's; `name` is only what the client calls itself).
pub fn credentials(grant: &JoinGrant, name: &str) -> Credentials {
    Credentials::named(name).with_ticket(grant.ticket.as_bytes().to_vec())
}

/// Open the grant's first door of `transport`. `roots`: the trust roots
/// of a `tls` or `quic` door (unused by the others).
pub async fn connect(
    grant: &JoinGrant,
    transport: Transport,
    roots: &[CertificateDer<'static>],
) -> io::Result<Conn> {
    let door = grant
        .door(transport)
        .ok_or_else(|| invalid(format!("the grant offers no {transport:?} door")))?;
    let addr = tokio::net::lookup_host(&door.addr)
        .await?
        .next()
        .ok_or_else(|| invalid(format!("door `{}` resolves to nothing", door.addr)))?;
    match transport {
        Transport::Tcp => crate::connect::tcp(addr).await,
        Transport::Ws => {
            let stream = TcpStream::connect(addr).await?;
            crate::connect::ws_stream(stream, &door.addr).await
        }
        Transport::Udp => {
            let key = grant.udp_key().map_err(|e| invalid(e.to_string()))?;
            crate::connect::udp(addr, Some(key)).await
        }
        Transport::Tls => {
            let name = server_name(&door.addr)?;
            let connector = crate::tls::connector(roots.iter().cloned())?;
            let tcp = TcpStream::connect(addr).await?;
            crate::tls::connect(tcp, &connector, name).await
        }
        Transport::Quic => {
            let host = host_of(&door.addr).to_owned();
            let config = crate::quic::client_config(roots.iter().cloned())?;
            crate::quic::connect(addr, &host, config).await
        }
    }
}

/// AUTH with the grant's ticket and JOIN the room it pins, within
/// `window` ([`session::auth_and_join`]).
pub async fn join(
    conn: &mut Conn,
    grant: &JoinGrant,
    name: &str,
    window: Duration,
    other: impl FnMut(FrameBody),
) -> Result<Joined, ClientError> {
    session::auth_and_join(conn, &credentials(grant, name), grant.room, window, other).await
}

/// The host part of `host:port` (an IPv6 literal's brackets dropped).
fn host_of(addr: &str) -> &str {
    let host = addr.rsplit_once(':').map_or(addr, |(h, _)| h);
    host.trim_start_matches('[').trim_end_matches(']')
}

fn server_name(addr: &str) -> io::Result<ServerName<'static>> {
    ServerName::try_from(host_of(addr).to_owned()).map_err(|e| invalid(e.to_string()))
}

fn invalid(msg: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, msg)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grant(doors: Vec<Door>, key: Option<&str>) -> JoinGrant {
        JoinGrant {
            doors,
            udp_server_key: key.map(str::to_owned),
            ticket: "v4.public.t".into(),
            room: 4,
        }
    }

    #[tokio::test]
    async fn a_udp_door_needs_the_key_and_a_missing_door_is_named() {
        let udp = Door {
            transport: Transport::Udp,
            addr: "127.0.0.1:9".into(),
        };
        let g = grant(vec![udp], None);
        let e = connect(&g, Transport::Udp, &[])
            .await
            .err()
            .expect("refused");
        assert!(e.to_string().contains("udp_server_key"), "{e}");
        let e = connect(&g, Transport::Tcp, &[])
            .await
            .err()
            .expect("refused");
        assert!(e.to_string().contains("Tcp"), "{e}");
        assert_eq!(credentials(&g, "ann").ticket, b"v4.public.t");
    }

    #[test]
    fn the_host_of_a_door() {
        assert_eq!(host_of("play.example.com:7777"), "play.example.com");
        assert_eq!(host_of("[::1]:7777"), "::1");
        assert_eq!(host_of("127.0.0.1:7777"), "127.0.0.1");
    }
}
