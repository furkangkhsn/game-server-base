//! The end of an rUDP session through [`Conn::recv`] (BACKLOG B128):
//! the frames already received come first, then `Recv::Closed` — at
//! once, and on every read after — as a stream's EOF. A scripted
//! plaintext server on a real socket answers the handshake, sends two
//! control frames out of order (so one is still buffered when the end
//! comes) and never ACKs anything: the client's reliable band dies.
//!
//! The other ends (a stateless reset, a record-layer limit) take the same
//! path — `Conn` reads only `UdpClient::is_established`; the client's own
//! tests drive each of them (`gsb_net`, `udp::client::tests::end`).

use std::time::{Duration, Instant};

use gsb_protocol::op::base::HEARTBEAT;
use tokio::net::UdpSocket;

use super::*;

const W: Duration = Duration::from_secs(2);

/// A REL datagram: `[1][u32 LE seq][u16 LE op][payload]`.
fn rel(seq: u32, op: u16, payload: &[u8]) -> Vec<u8> {
    let mut d = vec![gsb_net::udp::KIND_REL];
    d.extend(seq.to_le_bytes());
    d.extend(op.to_le_bytes());
    d.extend(payload);
    d
}

/// Answer the plaintext handshake (challenge, then the accept `ACK{1}`)
/// and return the client's address.
async fn handshake(server: &UdpSocket) -> std::net::SocketAddr {
    let mut buf = [0u8; 256];
    let (n, from) = server.recv_from(&mut buf).await.expect("challenge request");
    assert_eq!((n, buf[0]), (18, gsb_net::udp::KIND_HELLO));
    let mut hello = buf[..18].to_vec();
    hello[9..17].copy_from_slice(&7u64.to_le_bytes());
    server.send_to(&hello, from).await.expect("challenge");
    let (_, _) = server.recv_from(&mut buf).await.expect("proof");
    let accept = [gsb_net::udp::KIND_ACK, 1, 0, 0, 0];
    server.send_to(&accept, from).await.expect("accept");
    from
}

#[tokio::test]
async fn a_dead_rudp_session_is_closed_after_its_received_frames() {
    let server = UdpSocket::bind("127.0.0.1:0").await.expect("bind");
    let addr = server.local_addr().expect("bound");
    let client = tokio::spawn(crate::connect::udp(addr, None));
    let from = handshake(&server).await;
    let mut conn = client.await.expect("task").expect("handshake");

    server
        .send_to(&rel(2, 21, b"b"), from)
        .await
        .expect("seq 2");
    server
        .send_to(&rel(1, 20, b"a"), from)
        .await
        .expect("seq 1");
    match conn.recv(W).await.expect("recv") {
        Recv::Frame(f) => assert_eq!(f.op, 20),
        other => panic!("want seq 1, got {other:?}"),
    }
    // Seq 2 is now received but unread. The server never ACKs: the
    // band's backlog bound ends the session.
    let mut sent = 0;
    while conn.send(HEARTBEAT, b"hb").await.is_ok() {
        sent += 1;
        assert!(sent <= 1024, "the backlog bound never ended the session");
    }
    assert!(!conn.udp_client().expect("rUDP").is_established());

    match conn.recv(W).await.expect("recv") {
        Recv::Frame(f) => assert_eq!(f.op, 21, "received before the end"),
        other => panic!("want seq 2, got {other:?}"),
    }
    for _ in 0..2 {
        let t0 = Instant::now();
        let end = conn.recv(W).await.expect("recv");
        assert!(matches!(end, Recv::Closed), "{end:?}");
        assert!(t0.elapsed() < W / 4, "at once, not after the window");
    }
    assert!(
        conn.send(HEARTBEAT, b"hb").await.is_err(),
        "nothing is sent on an ended session"
    );
}
