//! `run_client` over a stream ends its session at the server's EOF
//! (BACKLOG B26): it used to loop on the dead wire — every receive
//! returning at once — until a later input write failed, or until the
//! deadline when the bot had nothing to send.

use std::time::{Duration, Instant};

use gsb_client::Recv;
use gsb_protocol::base::JoinRoomResult;
use gsb_protocol::op;
use prost::Message;
use tokio::net::TcpListener;

use super::*;

/// Answer the join, then close the connection: the server going away.
async fn join_then_close(listener: TcpListener) {
    let (sock, _) = listener.accept().await.expect("accept");
    let mut conn = gsb_client::connect::tcp_stream(sock);
    loop {
        match conn.recv(Duration::from_secs(10)).await.expect("peer read") {
            Recv::Frame(f) if f.op == op::base::JOIN_ROOM_REQ => break,
            Recv::Frame(_) => {}
            other => panic!("no join: {other:?}"),
        }
    }
    let joined = JoinRoomResult { entity: 42 }.encode_to_vec();
    conn.send(op::base::JOIN_ROOM_RESULT, &joined)
        .await
        .expect("peer write");
}

#[tokio::test]
async fn a_stream_eof_ends_the_client() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let peer = tokio::spawn(join_then_close(listener));
    let args = crate::Args::defaults();
    let window = Duration::from_secs(10);
    let p = ClientParams {
        tls: None,
        addr,
        room: 1,
        // Inputs far apart: a client still looping on the dead wire is
        // ended only by the second failed-looking write (or the deadline).
        move_ms: Duration::from_secs(3),
        stagger_ms: 0.0,
        bot: crate::bot::bot_for(&args),
        deadline: Instant::now() + window,
        flood: false,
        kind: crate::Transport::Tcp,
        capture: None,
        stall: None,
    };
    let t0 = Instant::now();
    let rep = run_client(5, p).await;
    peer.await.expect("peer");
    assert!(rep.joined, "the join landed before the close");
    assert_eq!(rep.entity, 42);
    assert!(
        t0.elapsed() < Duration::from_secs(2),
        "the client ended at the EOF, not {:?} later",
        t0.elapsed()
    );
    assert_eq!(rep.moves, 0, "nothing written after the EOF");
    assert!(!rep.left, "no leave result from a closed server");
}
