//! The rUDP door's one socket takes the configured kernel buffers
//! (BACKLOG B4): read back with `getsockopt`, and an out-of-range size
//! refuses the bind before any socket exists.

use super::*;
use crate::listen::{UdpBuffers, buffer_sizes};
use crate::udp::transport::bind_socket;

/// A request under the kernel's cap reaches the socket the demux reads
/// (Linux reads it back doubled); unset, the system default stays.
#[tokio::test]
async fn the_door_socket_takes_the_configured_buffers() {
    let any: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let asked = UdpBuffers {
        recv: Some(24_576),
        send: Some(28_672),
    };
    let (recv, send) = buffer_sizes(&bind_socket(any, asked).expect("bind")).expect("sizes");
    let factor = if cfg!(target_os = "linux") { 2 } else { 1 };
    assert_eq!(recv, factor * 24_576, "receive buffer");
    assert_eq!(send, factor * 28_672, "send buffer");
    let plain = std::net::UdpSocket::bind(any).expect("bind");
    assert_eq!(
        buffer_sizes(&bind_socket(any, UdpBuffers::default()).expect("bind")).expect("sizes"),
        buffer_sizes(&plain).expect("sizes"),
        "unset: the system default"
    );
}

/// The transport binds through it: a tuned door still handshakes, and a
/// size the builder refuses fails the bind as `InvalidInput`.
#[tokio::test]
async fn the_transport_binds_with_its_buffers() {
    let cfg = UdpTransportConfig {
        buffers: UdpBuffers {
            recv: Some(1 << 20),
            send: None,
        },
        ..Default::default()
    };
    let (_listener, addr, mut eps, _accept) = bound_transport(cfg).await;
    let raw = UdpSocket::bind("127.0.0.1:0").await.expect("bind");
    raw_handshake(&raw, addr, 0xB4B4_B4B4_B4B4_B4B4).await;
    tokio::time::timeout(Duration::from_secs(3), eps.recv())
        .await
        .expect("one session")
        .expect("endpoint");

    let bad = UdpTransportConfig {
        buffers: UdpBuffers {
            recv: None,
            send: Some(0),
        },
        ..Default::default()
    };
    let Err(err) = Arc::new(UdpTransport { config: bad })
        .bind("127.0.0.1:0".parse().unwrap())
        .await
    else {
        panic!("a zero send buffer must refuse the bind");
    };
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput, "{err}");
}
