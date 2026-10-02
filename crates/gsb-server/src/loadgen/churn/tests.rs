//! The churn client's byte accounting against a scripted TCP peer that
//! counts what really crossed the socket (BACKLOG B26): every frame the
//! client wrote — AUTH, BOTH join attempts, the inputs — and every frame
//! it read, the join phase's replies included, at the length-prefixed
//! frame's size.

use std::time::{Duration, Instant};

use gsb_client::frame::wire_len;
use gsb_client::{Conn, Recv};
use gsb_protocol::base::{Error, ErrorCode, JoinRoomResult};
use gsb_protocol::op;
use prost::Message;
use tokio::net::TcpListener;

use super::*;

/// What the peer counted: `(bytes it read, bytes it wrote, joins read)`.
type Tally = (u64, u64, u32);

async fn send(conn: &mut Conn, sent: &mut u64, op: u16, payload: &[u8]) {
    conn.send(op, payload).await.expect("peer write");
    *sent += wire_len(payload.len()) as u64;
}

/// One session: the first JOIN gets the gentle stale-resume refusal
/// (code 4 — the client retries on the same connection), the second an
/// extra frame and the result; then game frames, and the peer reads
/// until the client drops.
async fn peer(listener: TcpListener) -> Tally {
    let (sock, _) = listener.accept().await.expect("accept");
    let mut conn = gsb_client::connect::tcp_stream(sock);
    let (mut read, mut sent, mut joins) = (0u64, 0u64, 0u32);
    loop {
        match conn.recv(Duration::from_secs(10)).await.expect("peer read") {
            Recv::Frame(f) => {
                read += wire_len(f.payload.len()) as u64;
                if f.op != op::base::JOIN_ROOM_REQ {
                    continue;
                }
                joins += 1;
                if joins == 1 {
                    let refusal = Error {
                        code: ErrorCode::RoomOpFailed as i32,
                        message: "stale resume".into(),
                    };
                    send(
                        &mut conn,
                        &mut sent,
                        op::base::ERROR,
                        &refusal.encode_to_vec(),
                    )
                    .await;
                } else {
                    send(&mut conn, &mut sent, 0x0F01, &[7; 33]).await;
                    let joined = JoinRoomResult { entity: 42 };
                    let result = joined.encode_to_vec();
                    send(&mut conn, &mut sent, op::base::JOIN_ROOM_RESULT, &result).await;
                    for n in 1..=5usize {
                        send(&mut conn, &mut sent, 0x0F02, &vec![1; n * 10]).await;
                    }
                }
            }
            Recv::Closed => return (read, sent, joins),
            Recv::Quiet => panic!("the client went quiet without dropping"),
        }
    }
}

/// The churn client's one session, its run window `window` long: its
/// report, and the peer's tally — `None` when the client never started a
/// cycle (the window was spent before its first check).
async fn session(window: Duration) -> (ClientReport, Option<Tally>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let peer = tokio::spawn(peer(listener));
    let args = crate::Args::defaults();
    let p = ClientParams {
        tls: None,
        addr,
        room: 1,
        move_ms: Duration::from_millis(100),
        stagger_ms: 0.0,
        bot: crate::bot::bot_for(&args),
        // ONE session: the cycle outlasts the deadline, so the client
        // plays until shortly before it, drops, and sleeps it out.
        deadline: Instant::now() + window,
        flood: false,
        kind: crate::Transport::Tcp,
        udp_key: None,
        capture: None,
        stall: None,
        rpc: None,
    };
    let rep = run_churn_client(3, p, Duration::from_secs(60), 0).await;
    if rep.churn_cycles == 0 {
        peer.abort(); // it waits for a client that never came
        return (rep, None);
    }
    (rep, Some(peer.await.expect("peer")))
}

/// Every frame the session wrote and read, at its size on the wire.
///
/// Read from a run in which the session played (`moves > 0`): a run
/// window (1.5 s) a starved or frozen client spent on its join holds no
/// input — that run is repeated with twice the window, the accounting
/// checked on every run all the same (BACKLOG F52; the F30 pattern).
#[tokio::test]
async fn churn_bytes_are_the_frames_on_the_wire() {
    let mut window = Duration::from_millis(1500);
    loop {
        let (rep, tally) = session(window).await;
        if let Some((read, sent, joins)) = tally {
            assert_eq!(joins, 2, "the refused join was retried");
            assert_eq!(rep.churn_cycles, 1);
            assert_eq!(rep.fresh_joins + rep.resumed, 0, "a first session");
            assert_eq!(rep.bytes_out, read, "client out = what the peer read");
            assert_eq!(rep.bytes_in, sent, "client in = what the peer wrote");
            if rep.moves > 0 {
                return; // the session played: the run is evidence
            }
        }
        window *= 2;
        assert!(
            window <= Duration::from_millis(24_000),
            "no run held an input"
        );
    }
}
