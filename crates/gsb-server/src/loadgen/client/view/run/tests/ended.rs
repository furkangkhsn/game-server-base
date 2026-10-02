//! B128: `run_client` over rUDP ends its session when the CLIENT declares
//! it over — the rUDP twin of `a_stream_eof_ends_the_client`.
//! It used to read silence until the deadline (rUDP has no EOF). A
//! scripted plaintext server answers the handshake and the JOIN, then
//! never ACKs anything: the client's AUTH and JOIN stay outstanding and
//! its reliable band dies at the 5 s bound — its reads report `Closed`,
//! the client stops at once, and the end is counted once, by reason.

use std::time::{Duration, Instant};

use gsb_protocol::base::JoinRoomResult;
use gsb_protocol::op;
use prost::Message;
use tokio::net::UdpSocket;

use super::super::*;
use super::HANG_GUARD;

/// Answer the plaintext handshake and the JOIN (one REL frame, seq 1),
/// then read on without ever ACKing.
async fn silent_after_join(sock: UdpSocket) {
    use gsb_net::udp::{KIND_ACK, KIND_HELLO, KIND_REL};
    let mut buf = [0u8; 2048];
    let (_, from) = sock.recv_from(&mut buf).await.expect("challenge request");
    assert_eq!(buf[0], KIND_HELLO);
    let mut hello = buf[..18].to_vec();
    hello[9..17].copy_from_slice(&7u64.to_le_bytes());
    sock.send_to(&hello, from).await.expect("challenge");
    sock.recv_from(&mut buf).await.expect("proof");
    sock.send_to(&[KIND_ACK, 1, 0, 0, 0], from)
        .await
        .expect("accept");
    loop {
        let (n, _) = sock.recv_from(&mut buf).await.expect("read");
        let join = op::base::JOIN_ROOM_REQ.to_le_bytes();
        if n >= 7 && buf[0] == KIND_REL && buf[5..7] == join {
            break;
        }
    }
    let mut d = vec![KIND_REL, 1, 0, 0, 0];
    d.extend(op::base::JOIN_ROOM_RESULT.to_le_bytes());
    d.extend(JoinRoomResult { entity: 42 }.encode_to_vec());
    sock.send_to(&d, from).await.expect("join result");
    loop {
        sock.recv_from(&mut buf).await.expect("read");
    }
}

/// A client that missed the end reads on (an ended session's reads
/// return at once, yielding through the runtime's budget) until the
/// hang guard.
#[tokio::test]
async fn an_ended_rudp_session_ends_the_client_counted_once() {
    let sock = UdpSocket::bind("127.0.0.1:0").await.expect("bind");
    let addr = sock.local_addr().expect("addr");
    let peer = tokio::spawn(silent_after_join(sock));
    let args = crate::Args::defaults();
    let never = Duration::from_secs(3600);
    let p = ClientParams {
        tls: None,
        addr,
        room: 1,
        move_ms: never,
        stagger_ms: 0.0,
        bot: crate::bot::bot_for(&args),
        deadline: Instant::now() + never,
        flood: false,
        kind: crate::Transport::Udp,
        udp_key: None,
        capture: None,
        stall: None,
        rpc: None,
    };
    let rep = tokio::time::timeout(HANG_GUARD, run_client(6, p))
        .await
        .expect("the client ended with its session: nothing else could end it");
    peer.abort();
    assert!(rep.joined, "the join landed before the band died");
    assert_eq!(rep.entity, 42);
    assert!(!rep.left, "no leave on an ended session");
    assert_eq!(
        rep.udp_ends,
        UdpEnds::from_values([1, 0, 0]),
        "one end, under its reason"
    );
    assert_eq!(rep.gave_up, 2, "the AUTH and the JOIN, never ACKed");
    assert_eq!(rep.errors.total(), 0, "an end is no client error");
}
