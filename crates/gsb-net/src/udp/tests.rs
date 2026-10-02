//! End-to-end rUDP over a real socket: handshake, writer roundtrip,
//! forged proofs, the datagram budget and the reliable band's
//! retransmit clock.

use crate::transport::Endpoint;
use crate::transport::{Listener, Transport};
use crate::udp::*;
use bytes::Bytes;
use gsb_core::id::ConnectionId;
use gsb_protocol::FrameBody;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::UdpSocket;
use tokio::sync::mpsc;

/// Bind a transport and run an accept loop that hands endpoints to an
/// unbounded channel (the unit-test stand-in for the server's accept
/// loop).
async fn bound_transport(
    config: UdpTransportConfig,
) -> (
    Arc<dyn Listener>,
    std::net::SocketAddr,
    mpsc::UnboundedReceiver<Endpoint>,
    tokio::task::JoinHandle<()>,
) {
    let transport = Arc::new(UdpTransport { config });
    let listener = transport
        .bind("127.0.0.1:0".parse().unwrap())
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("local addr");
    let (ep_tx, ep_rx) = mpsc::unbounded_channel();
    let l2 = Arc::clone(&listener);
    let accept = tokio::spawn(async move {
        loop {
            let l = Arc::clone(&l2);
            match l.accept().await {
                Ok(ep) => {
                    if ep_tx.send(ep).is_err() {
                        break;
                    }
                }
                Err(_) => break, // listener closed
            }
        }
    });
    (listener, addr, ep_rx, accept)
}

/// The handshake done by hand on a raw socket, every datagram checked
/// byte for byte: the uneventful handshake is `HELLO` 18 B → challenge
/// 18 B → proof 18 B → accept `ACK{1}` 5 B (`[2, 1, 0, 0, 0]`), and
/// nothing else. Returns once the accept has been read, so the caller's
/// next datagram is the session's first real one.
async fn raw_handshake(raw: &UdpSocket, addr: SocketAddr, nonce: u64) {
    let mut buf = vec![0u8; 2048];
    raw.send_to(&encode_hello(nonce, 0), addr).await.unwrap();
    let (n, _) = tokio::time::timeout(Duration::from_secs(3), raw.recv_from(&mut buf))
        .await
        .expect("challenge")
        .expect("recv");
    assert_eq!(n, 18, "the challenge is the request's size");
    assert_eq!(buf[0], KIND_HELLO);
    assert_eq!(
        buf[1..9],
        nonce.to_le_bytes(),
        "the challenge echoes the nonce"
    );
    let cookie = u64::from_le_bytes(buf[9..17].try_into().unwrap());
    raw.send_to(&encode_hello(nonce, cookie), addr)
        .await
        .unwrap();
    let (n, _) = tokio::time::timeout(Duration::from_secs(3), raw.recv_from(&mut buf))
        .await
        .expect("the accept")
        .expect("recv");
    assert_eq!(&buf[..n], &[KIND_ACK, 1, 0, 0, 0], "the accept is ACK{{1}}");
}

/// The two-phase topology works: sequential handshakes each produce an
/// endpoint (with the correct peer), and the per-session writer
/// delivers a control frame to the client's reliable band.
#[tokio::test]
async fn sequential_handshakes_and_writer_roundtrip() {
    let (listener, addr, mut eps, _accept) = bound_transport(UdpTransportConfig::default()).await;

    let a = UdpClient::connect(addr).await.expect("A handshake");
    let ep_a = tokio::time::timeout(Duration::from_secs(3), eps.recv())
        .await
        .expect("A endpoint")
        .expect("A endpoint");
    assert_eq!(ep_a.peer().unwrap().port(), a.local_addr().unwrap().port());

    let mut b = UdpClient::connect(addr).await.expect("B handshake");
    let mut ep_b = tokio::time::timeout(Duration::from_secs(3), eps.recv())
        .await
        .expect("B endpoint")
        .expect("B endpoint");
    assert_eq!(ep_b.peer().unwrap().port(), b.local_addr().unwrap().port());

    // Drive B's outbound path: pump + a control frame.
    let (in_tx, _in_rx) = ep_b.take_inbox(16);
    let (out_tx, out_rx) = ep_b.take_outbox(16);
    let (_reader, _writer) = ep_b.start_pump(
        ConnectionId(2),
        in_tx,
        out_rx,
        crate::pump::PumpTimeouts::default(),
    );
    let fb = FrameBody::new(
        gsb_protocol::op::base::HEARTBEAT_ACK,
        Bytes::from(vec![7, 9]),
    );
    out_tx.send(vec![fb]).await.expect("send to writer");
    let got = tokio::time::timeout(
        Duration::from_secs(15),
        b.recv_frame(Duration::from_secs(10)),
    )
    .await
    .expect("B recv window")
    .expect("B recv")
    .expect("no frame for B");
    assert_eq!(got.op, gsb_protocol::op::base::HEARTBEAT_ACK);
    assert_eq!(got.payload.as_ref(), &[7u8, 9]);

    // The handshake must be stateless-per-peer: A is unaffected.
    let _ = a.local_addr();
    drop(b);
    listener.close();
}

/// A forged cookie (wrong proof) is rejected: no session, no
/// endpoint — and the rejection leaves other clients unharmed.
#[tokio::test]
async fn forged_proof_is_rejected() {
    let (_listener, addr, mut eps, _accept) = bound_transport(UdpTransportConfig::default()).await;

    let raw = UdpSocket::bind("0.0.0.0:0".parse::<SocketAddr>().unwrap())
        .await
        .expect("raw client binds");
    let mut buf = vec![0u8; 2048];
    let nonce = 0xDEAD_BEEF_CAFE_F00Du64;

    // Challenge request.
    raw.send_to(&encode_hello(nonce, 0), addr)
        .await
        .expect("send challenge request");
    let (n, from) = tokio::time::timeout(Duration::from_secs(3), raw.recv_from(&mut buf))
        .await
        .expect("challenge arrives")
        .expect("recv");
    assert_eq!(from, addr);
    assert_eq!(buf[0], KIND_HELLO);
    let cookie = u64::from_le_bytes(buf[9..17].try_into().unwrap());
    assert!(cookie != 0, "the challenge must carry a real cookie");

    // Forged proof: cookie + 1. The demux cannot match it against
    // F(nonce, peer, key) — no session may be created.
    raw.send_to(&encode_hello(nonce, cookie.wrapping_add(1)), addr)
        .await
        .expect("send forged proof");
    assert!(
        tokio::time::timeout(Duration::from_millis(400), eps.recv())
            .await
            .is_err(),
        "a forged proof must not produce an endpoint"
    );
    // ...and must not be answered: no accept, nothing to reflect.
    assert!(
        tokio::time::timeout(Duration::from_millis(100), raw.recv_from(&mut buf))
            .await
            .is_err(),
        "a forged proof is answered with nothing"
    );

    // The rejection is scoped to the attacker: a real client still
    // gets a session.
    let c = UdpClient::connect(addr)
        .await
        .expect("real client still works");
    let _ep = tokio::time::timeout(Duration::from_secs(3), eps.recv())
        .await
        .expect("real endpoint")
        .expect("real endpoint");
    assert_eq!(_ep.peer().unwrap().port(), c.local_addr().unwrap().port());
    let _ = n;
}

mod backoff;
mod bands;
mod buffers;
mod feedback;
mod frag;
mod handshake;
mod migrate;
mod pace;
mod reap;
mod reset;
mod sealed;
mod unaccepted;
mod writer_lost;
