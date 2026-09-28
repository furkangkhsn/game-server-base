//! The QUIC door's socket takes the configured kernel buffers (BACKLOG
//! B4): the socket handed to quinn is read back with `getsockopt`, a
//! connection still flows over it, and an out-of-range size refuses the
//! bind before any socket exists.

use super::*;
use crate::listen::UdpBuffers;

/// A request under the kernel's cap reaches the endpoint's socket
/// (Linux reads it back doubled), and the door still serves a
/// connection end to end.
#[tokio::test]
async fn the_endpoint_socket_takes_the_configured_buffers() {
    let pki = mint_pki("buffers");
    let mut transport = transport_for(&pki);
    let asked = UdpBuffers {
        recv: Some(24_576),
        send: Some(28_672),
    };
    let server_config = load_server_config(&transport.config).expect("identity");
    let any: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let (endpoint, (recv, send)) =
        crate::quic::listener::bind_endpoint(server_config, any, asked).expect("bind");
    let factor = if cfg!(target_os = "linux") { 2 } else { 1 };
    assert_eq!(recv, factor * 24_576, "receive buffer");
    assert_eq!(send, factor * 28_672, "send buffer");
    endpoint.close(0u32.into(), b"");

    transport.config.buffers = asked;
    let listener = Arc::new(transport).bind(any).await.expect("bind");
    let bound = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { listener.accept().await.map(|_| ()) });
    let (_reader, mut writer) = connect(bound, "localhost", pki.ca_pem.as_bytes())
        .await
        .expect("a connection over the tuned socket");
    // A stream is announced by its first frame.
    writer
        .send(FrameBody::new(7, b"b4".as_slice()))
        .await
        .expect("client send");
    tokio::time::timeout(std::time::Duration::from_secs(5), server)
        .await
        .expect("accepted in time")
        .expect("task")
        .expect("accepted");
}

/// A size the socket builder refuses fails the bind as `InvalidInput`.
#[tokio::test]
async fn an_out_of_range_buffer_refuses_the_bind() {
    let pki = mint_pki("buffers-bad");
    let mut transport = transport_for(&pki);
    transport.config.buffers = UdpBuffers {
        recv: Some(1),
        send: None,
    };
    let Err(err) = Arc::new(transport)
        .bind("127.0.0.1:0".parse().unwrap())
        .await
    else {
        panic!("a one-byte buffer must refuse the bind");
    };
    assert_eq!(err.kind(), io::ErrorKind::InvalidInput, "{err}");
}
