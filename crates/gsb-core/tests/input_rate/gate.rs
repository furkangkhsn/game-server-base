//! The connection actor's side of the input rate limit, against a
//! stand-in registry that answers every join with a [`Seat`] of the
//! test's choosing and hands the test the room end of its action
//! channel — what reaches that channel is exactly what the room would
//! pull.

use super::*;

use gsb_core::registry::Seat;
use gsb_core::room::Action;

/// The stand-in registry: each join is answered with the next rate of
/// `rates` and a fresh action channel whose receiver goes to `rooms`.
fn stand_in(
    rates: Vec<Option<InputRate>>,
) -> (
    Mailbox<RegistryMsg>,
    mpsc::UnboundedReceiver<mpsc::Receiver<Action>>,
) {
    let (tx, mut rx) = channel::<RegistryMsg>(64);
    let (rooms_tx, rooms) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        let mut rates = rates.into_iter();
        while let Some(msg) = rx.recv().await {
            if let RegistryMsg::SpawnPlayer { reply, .. } = msg {
                let (actions, room) = channel::<Action>(4096);
                let _ = rooms_tx.send(room);
                let input_rate = rates.next().expect("a rate per join");
                let _ = reply.send(Ok(Seat {
                    entity: 7,
                    actions,
                    input_rate,
                }));
            }
        }
    });
    (tx, rooms)
}

/// Everything in the room end of the channel: the ops, in order.
fn pulled(room: &mut mpsc::Receiver<Action>) -> Vec<u16> {
    let mut ops = Vec::new();
    while let Ok(a) = room.try_recv() {
        ops.push(a.op);
    }
    ops
}

/// The first ERROR's code in the next batch.
async fn error_code(c: &mut Client) -> i32 {
    let batch = tokio::time::timeout(WAIT, c.out.recv()).await;
    let batch = batch.expect("an answer in time").expect("out open");
    let f = batch
        .iter()
        .find(|f| f.op == op::base::ERROR)
        .expect("an ERROR frame");
    base::Error::decode(f.payload.as_ref())
        .expect("decode")
        .code
}

fn rpc_frame(id: u64) -> FrameBody {
    let env = base::RpcRequest {
        id,
        op: u32::from(GAME_OP),
        payload: Vec::new(),
    };
    FrameBody::new(op::base::RPC_REQ, env.encode_to_vec())
}

/// Only registered game-band input is metered: with the one token
/// spent, RPC requests (owed exactly one answer each — their volume has
/// its own caps) and heartbeats still pass; the game input does not.
#[tokio::test(start_paused = true)]
async fn only_game_input_is_metered() {
    let (reg, mut rooms) = stand_in(vec![InputRate::new(1, 1)]);
    let mut c = Client::login(&reg, 1).await;
    let mut room = rooms.recv().await.expect("the join's channel");
    c.send(game_frame()).await;
    for id in 1..=5 {
        c.send(rpc_frame(id)).await;
    }
    c.send(game_frame()).await;
    c.send(game_frame()).await;
    c.send(FrameBody::new(
        op::base::HEARTBEAT,
        Heartbeat { tick: 1 }.encode_to_vec(),
    ))
    .await;
    c.expect(op::base::HEARTBEAT_ACK).await;
    let rpc = op::base::RPC_REQ;
    assert_eq!(pulled(&mut room), [GAME_OP, rpc, rpc, rpc, rpc, rpc]);
    let s = c.close().await;
    assert_eq!((s.input_rate_limited, s.violations), (2, 0));
}

/// The gate sits behind the protocol checks, not in front of them: an
/// undefined opcode is still a hard violation (UnknownOpcode) with the
/// bucket empty, and a game op outside a room is still the race-class
/// NotInRoom — neither is swallowed as "rate-limited".
#[tokio::test(start_paused = true)]
async fn the_gate_does_not_mask_the_protocol_checks() {
    let (reg, mut rooms) = stand_in(vec![InputRate::new(1, 1)]);
    let mut c = Client::login(&reg, 1).await;
    let mut room = rooms.recv().await.expect("the join's channel");
    c.send(game_frame()).await; // the one token
    c.send(FrameBody::new(GAME_OP + 1, Vec::new())).await;
    assert_eq!(error_code(&mut c).await, 1, "UnknownOpcode");
    c.send(FrameBody::new(op::base::LEAVE_ROOM_REQ, Vec::new()))
        .await;
    c.expect(op::base::LEAVE_ROOM_RESULT).await;
    c.send(game_frame()).await;
    assert_eq!(error_code(&mut c).await, 6, "NotInRoom");
    assert_eq!(pulled(&mut room), [GAME_OP]);
    let s = c.close().await;
    assert_eq!((s.input_rate_limited, s.violations), (0, 2));
}

/// Leave the room and join again (the stand-in answers the next rate);
/// every frame sent before is handled by then.
async fn rejoin(c: &mut Client) {
    c.send(FrameBody::new(op::base::LEAVE_ROOM_REQ, Vec::new()))
        .await;
    c.expect(op::base::LEAVE_ROOM_RESULT).await;
    let join = base::JoinRoom { room_id: 1 }.encode_to_vec();
    c.send(FrameBody::new(op::base::JOIN_ROOM_REQ, join)).await;
    c.expect(op::base::JOIN_ROOM_RESULT).await;
}

/// Room hops at one instant buy nothing: the bucket is the
/// connection's, re-tuned by each join, never refilled by one — and an
/// unlimited room in between passes everything without resetting it.
/// Time does refill it.
#[tokio::test(start_paused = true)]
async fn a_rejoin_does_not_refill_the_bucket() {
    let limit = InputRate::new(10, 3);
    let (reg, mut rooms) = stand_in(vec![limit, None, limit]);
    let mut c = Client::login(&reg, 1).await;
    let mut passed = Vec::new();
    for _ in 0..2 {
        let mut room = rooms.recv().await.expect("a join's channel");
        for _ in 0..5 {
            c.send(game_frame()).await;
        }
        rejoin(&mut c).await;
        passed.push(pulled(&mut room).len());
    }
    let mut room = rooms.recv().await.expect("the last join's channel");
    for _ in 0..5 {
        c.send(game_frame()).await;
    }
    c.send(FrameBody::new(
        op::base::HEARTBEAT,
        Heartbeat { tick: 1 }.encode_to_vec(),
    ))
    .await;
    c.expect(op::base::HEARTBEAT_ACK).await;
    passed.push(pulled(&mut room).len());
    assert_eq!(passed, [3, 5, 0], "limited, unlimited, limited again");
    tokio::time::sleep(Duration::from_millis(100)).await;
    c.send(game_frame()).await;
    c.send(game_frame()).await;
    let s = c.close().await;
    assert_eq!(pulled(&mut room).len(), 1, "100 ms at 10/s is one token");
    assert_eq!(s.input_rate_limited, 2 + 5 + 1);
}
