//! B88: a client sends no game input and no RPC request before its
//! JOIN is answered — before it the session is in no room, and the
//! server answers such a frame `NotInRoom`. Over TCP a fast server
//! answers the JOIN before the first input falls due, so the smokes
//! cannot see a client that does not wait; this peer holds the answer
//! back for many input and RPC intervals, answers every early game or
//! RPC frame the way the server does, and counts them.

use std::time::{Duration, Instant};

use gsb_client::Recv;
use gsb_protocol::base::{Error, ErrorCode, JoinRoomResult, LeaveRoomResult};
use gsb_protocol::op;
use prost::Message;
use tokio::net::TcpListener;

use super::super::*;

/// How long the peer holds the JOIN's answer back: six input intervals
/// and twelve RPC intervals of the client below.
const HOLD: Duration = Duration::from_millis(600);

/// What the peer saw: `(game-band frames, RPC requests)` read before it
/// answered the JOIN, and whether any input or request came after it.
type Seen = (u32, u32, bool);

async fn holding_peer(listener: TcpListener) -> Seen {
    let (sock, _) = listener.accept().await.expect("accept");
    let mut conn = gsb_client::connect::tcp_stream(sock);
    let (mut early_game, mut early_rpc, mut later) = (0u32, 0u32, false);
    let mut answer_at: Option<Instant> = None;
    loop {
        let wait = match answer_at {
            Some(at) => at.saturating_duration_since(Instant::now()),
            None => Duration::from_secs(10),
        };
        match conn.recv(wait).await.expect("peer read") {
            Recv::Frame(f) if f.op == op::base::JOIN_ROOM_REQ => {
                answer_at = Some(Instant::now() + HOLD);
            }
            Recv::Frame(f) if f.op == op::base::LEAVE_ROOM_REQ => {
                let left = LeaveRoomResult::default().encode_to_vec();
                conn.send(op::base::LEAVE_ROOM_RESULT, &left)
                    .await
                    .expect("peer write");
            }
            Recv::Frame(f) if f.op == op::base::RPC_REQ || f.op >= op::GAME_BAND_START => {
                if answer_at.is_some() {
                    // Still holding the JOIN: answered as the server
                    // would answer a frame outside any room.
                    if f.op == op::base::RPC_REQ {
                        early_rpc += 1;
                    } else {
                        early_game += 1;
                    }
                    let e = Error {
                        code: ErrorCode::NotInRoom as i32,
                        message: "not in a room".into(),
                    };
                    conn.send(op::base::ERROR, &e.encode_to_vec())
                        .await
                        .expect("peer write");
                } else {
                    later = true;
                }
            }
            Recv::Frame(_) => {}
            Recv::Quiet if answer_at.is_some() => {
                // The hold is over: answer the JOIN.
                answer_at = None;
                let joined = JoinRoomResult { entity: 42 }.encode_to_vec();
                conn.send(op::base::JOIN_ROOM_RESULT, &joined)
                    .await
                    .expect("peer write");
            }
            Recv::Quiet => panic!("the client went quiet"),
            Recv::Closed => return (early_game, early_rpc, later),
        }
    }
}

/// Inputs every 100 ms and an RPC request every 50 ms, a JOIN answered
/// after 600 ms: nothing but AUTH and JOIN reaches the peer before the
/// answer, and both kinds of frame flow after it.
#[tokio::test]
async fn no_input_and_no_request_before_the_join_is_answered() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let peer = tokio::spawn(holding_peer(listener));
    let args = crate::Args::defaults();
    let p = ClientParams {
        tls: None,
        addr,
        room: 1,
        move_ms: Duration::from_millis(100),
        stagger_ms: 0.0,
        bot: crate::bot::bot_for(&args),
        deadline: Instant::now() + HOLD + Duration::from_millis(800),
        flood: false,
        kind: crate::Transport::Tcp,
        udp_key: None,
        capture: None,
        stall: None,
        rpc: Some(RpcPlan {
            rate: 20.0,
            burst: 1,
        }),
    };
    let rep = run_client(7, p).await;
    let (early_game, early_rpc, later) = peer.await.expect("peer");
    assert!(rep.joined && rep.left, "a whole session");
    assert_eq!(early_game, 0, "no game input before the JOIN's answer");
    assert_eq!(early_rpc, 0, "no RPC request before the JOIN's answer");
    assert_eq!(rep.errors.not_in_room, 0, "nothing answered NotInRoom");
    assert!(
        later && rep.moves > 0 && rep.rpc.sent > 0,
        "inputs and requests did flow after it"
    );
}
