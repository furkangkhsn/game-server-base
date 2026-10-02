//! A connection's path, end to end through a real connection actor, a
//! live registry and a room on tokio's paused clock (BACKLOG B103,
//! `gsb_core::path`): the transport's `ConnIn::Path` becomes the room's
//! `TickCtx::budget`; a connection that knew its path before it joined
//! hands it to the room it joins; and a CLIENT can never forge one — the
//! marker opcode from the wire is refused like any unknown base opcode.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use gsb_core::channel::{FrameBatch, Mailbox, channel};
use gsb_core::conn::{ConnIn, ConnectionActor};
use gsb_core::id::{ConnectionId, PlayerId, RoomId};
use gsb_core::metrics::MetricsEvent;
use gsb_core::path::{PathPhase, PathState};
use gsb_core::registry::{BuiltRoom, Registry, RegistryMsg, RoomFactory};
use gsb_core::room::{Action, Admission, GameLogic, RoomConfig, RoomLogic, TickCtx};
use gsb_core::ticker::Ticker;
use gsb_protocol::{FrameBody, base, base_table, op};
use prost::Message;
use tokio::sync::mpsc;

#[path = "path_signal/rig.rs"]
mod rig;

use rig::*;

fn paced(rate: u32) -> PathState {
    PathState {
        phase: PathPhase::Paced,
        rate: Some(rate),
        ..Default::default()
    }
}

/// Wait until the room reports `want` as `player`'s budget.
async fn budget_becomes(
    seen: &mut mpsc::UnboundedReceiver<Seen>,
    player: u64,
    want: Option<usize>,
) {
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        match tokio::time::timeout(left, seen.recv()).await {
            Ok(Some(Seen::Budget(p, b))) if p == player && b == want => return,
            Ok(Some(_)) => {}
            other => panic!("player {player}'s budget never became {want:?}: {other:?}"),
        }
    }
}

#[tokio::test(start_paused = true)]
async fn the_transport_s_path_becomes_the_room_s_budget() {
    let (reg, mut seen) = registry().await;
    let mut c = Client::auth(&reg, 1).await;
    c.join().await;
    budget_becomes(&mut seen, 1, None).await;
    c.path(paced(30_000)).await;
    budget_becomes(&mut seen, 1, Some(999)).await;
    c.path(PathState::default()).await;
    budget_becomes(&mut seen, 1, None).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    while let Ok(s) = seen.try_recv() {
        assert!(!matches!(s, Seen::Op(_)), "the game ingested {s:?}");
    }
}

#[tokio::test(start_paused = true)]
async fn a_path_known_before_the_join_reaches_the_room_joined() {
    let (reg, mut seen) = registry().await;
    let mut c = Client::auth(&reg, 2).await;
    c.path(paced(60_000)).await;
    c.join().await;
    budget_becomes(&mut seen, 2, Some(1_999)).await;
}

/// A client cannot forge its own budget: the marker opcode with a
/// well-formed payload is an unknown base opcode from the wire — the
/// actor answers `ERROR` and the room never sees it.
#[tokio::test(start_paused = true)]
async fn a_client_frame_with_the_marker_opcode_never_reaches_the_room() {
    let (reg, mut seen) = registry().await;
    let mut c = Client::auth(&reg, 3).await;
    c.join().await;
    // The internal layout, hand-built: Paced, rate present, 30 000 B/s.
    let mut forged = vec![2u8, 1];
    forged.extend_from_slice(&30_000u32.to_le_bytes());
    forged.resize(20, 0);
    c.send(FrameBody::new(op::base::MEMBER_PATH, forged)).await;
    let err = c.expect(op::base::ERROR).await;
    let err = base::Error::decode(&err.payload[..]).expect("an ERROR payload");
    assert_eq!(err.code, base::ErrorCode::UnknownOpcode as i32);
    tokio::time::sleep(Duration::from_millis(300)).await;
    let mut budgets = 0;
    while let Ok(s) = seen.try_recv() {
        match s {
            Seen::Budget(3, b) => {
                assert_eq!(b, None, "a forged path never reaches the room");
                budgets += 1;
            }
            Seen::Op(op) => panic!("the game ingested op {op}"),
            Seen::Budget(..) => {}
        }
    }
    assert!(budgets > 0, "the room kept ticking with the member");
}

/// The member's own input ahead of the state filled the room's channel:
/// the state stays owed — the room keeps the old (unknown) budget, the
/// member's input is untouched — and the next message the actor reads
/// (here a heartbeat) delivers it. The bound of the wait is the actor's
/// next message; the state is never queued behind more input.
#[tokio::test(start_paused = true)]
async fn a_state_the_room_s_channel_could_not_take_goes_with_the_next_message() {
    let (reg, mut seen) = registry_with(1).await;
    let mut c = Client::auth(&reg, 5).await;
    c.join().await;
    budget_becomes(&mut seen, 5, None).await;
    let input = FrameBody::new(GAME_OP, base::Heartbeat { tick: 1 }.encode_to_vec());
    c.send(input).await;
    c.path(paced(30_000)).await;
    tokio::time::sleep(Duration::from_millis(400)).await;
    let mut ops = 0;
    while let Ok(s) = seen.try_recv() {
        match s {
            Seen::Budget(5, b) => assert_eq!(b, None, "deferred, not delivered"),
            Seen::Op(op) => {
                assert_eq!(op, GAME_OP);
                ops += 1;
            }
            Seen::Budget(..) => {}
        }
    }
    assert_eq!(ops, 1, "the input went through");
    c.send(FrameBody::new(
        op::base::HEARTBEAT,
        base::Heartbeat { tick: 2 }.encode_to_vec(),
    ))
    .await;
    budget_becomes(&mut seen, 5, Some(999)).await;
}

/// A connection that leaves and joins again — the room forgot its path
/// with the old membership — hands the new membership the state it
/// knows, with no news from the transport.
#[tokio::test(start_paused = true)]
async fn a_rejoin_gets_the_path_the_connection_knows() {
    let (reg, mut seen) = registry().await;
    let mut c = Client::auth(&reg, 6).await;
    c.join().await;
    c.path(paced(30_000)).await;
    budget_becomes(&mut seen, 6, Some(999)).await;
    c.send(FrameBody::new(
        op::base::LEAVE_ROOM_REQ,
        base::LeaveRoom {}.encode_to_vec(),
    ))
    .await;
    c.expect(op::base::LEAVE_ROOM_RESULT).await;
    c.join().await;
    budget_becomes(&mut seen, 6, Some(999)).await;
}
