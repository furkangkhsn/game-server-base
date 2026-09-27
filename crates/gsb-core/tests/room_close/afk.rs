//! The input-idle ceiling's action end to end (real connection actors,
//! a live registry, the paused clock — the ceiling reads the tick clock,
//! so a virtual second is a second of idleness).

use super::*;
use rig::{CEILING, Client, GAME_OP, registry};

/// The exact notice the opt-in path sends: ERROR 9 and the reason.
fn notice() -> Vec<u8> {
    base::Error::new(
        base::ErrorCode::ServerClosed,
        format!(
            "input idle: no game input for {CEILING} s (the room's \
             max_idle_input_secs; afk_action = disconnect)"
        ),
    )
    .encode_to_vec()
}

/// The frames end with the notice, and nothing follows it.
fn ends_with_the_notice(frames: &[FrameBody]) {
    let last = frames.last().expect("at least the notice");
    assert_eq!(last.op, op::base::ERROR, "{frames:?}");
    assert_eq!(
        &last.payload[..],
        &notice()[..],
        "the notice, byte for byte"
    );
    let errors = frames.iter().filter(|f| f.op == op::base::ERROR).count();
    assert_eq!(errors, 1, "one notice: {frames:?}");
}

fn game_frame() -> FrameBody {
    FrameBody::new(GAME_OP, Heartbeat { tick: 1 }.encode_to_vec())
}

/// **The default is today, byte for byte.** `leave_room`: the ceiling
/// runs the policy and ends the membership, the client is sent NOTHING
/// and its socket stays open; its next game frame finds the room gone,
/// and it joins again. No verdict is booked.
#[tokio::test(start_paused = true)]
async fn the_default_ends_the_membership_and_keeps_the_socket() {
    let (disc, mut disconnects) = mpsc::unbounded_channel();
    let reg = registry(
        logic::factory(Detach::Despawn, false, disc),
        AfkAction::LeaveRoom,
        None,
    )
    .await;
    let mut c = Client::login(&reg, 1, "ana").await;
    tokio::time::sleep(Duration::from_secs(CEILING + 2)).await;

    assert_eq!(
        disconnects.try_recv().ok(),
        Some((PlayerId(1), "ana".to_string())),
        "the ceiling ran the policy"
    );
    assert!(c.received().is_empty(), "not one byte on the wire");
    assert!(!c.actor.is_finished(), "the socket stays open");
    assert_eq!(
        status(&reg, RoomId(1)).await,
        RoomStatus::Running { members: 1 }
    );

    c.send(game_frame()).await;
    c.join().await;
    assert_eq!(c.verdict().await, None, "nothing was the server's verdict");
}

/// `disconnect`: the policy runs (here it despawns), then the client gets
/// ERROR 9 with the reason and the close; the session is booked as
/// `idle_input`, and the room no longer counts it.
#[tokio::test(start_paused = true)]
async fn disconnect_sends_the_notice_then_closes() {
    let (disc, mut disconnects) = mpsc::unbounded_channel();
    let reg = registry(
        logic::factory(Detach::Despawn, false, disc),
        AfkAction::Disconnect,
        None,
    )
    .await;
    let mut c = Client::login(&reg, 1, "ana").await;
    tokio::time::sleep(Duration::from_secs(CEILING + 2)).await;

    let frames = c.until_closed().await;
    ends_with_the_notice(&frames);
    assert_eq!(
        disconnects.try_recv().ok(),
        Some((PlayerId(1), "ana".to_string())),
        "on_disconnect ran"
    );
    assert_eq!(c.verdict().await, Some(ServerClose::IdleInput));
    assert_eq!(
        status(&reg, RoomId(1)).await,
        RoomStatus::Running { members: 0 }
    );
}

/// A policy that parks the idle member: the socket closes all the same,
/// the park keeps its slot, and the player's reconnect resumes the SAME
/// entity (the park is the resume target, §14.3).
#[tokio::test(start_paused = true)]
async fn disconnect_keeps_the_park_and_the_player_resumes_it() {
    let (disc, _d) = mpsc::unbounded_channel();
    let reg = registry(
        logic::factory(HOLD, false, disc),
        AfkAction::Disconnect,
        None,
    )
    .await;
    let mut c = Client::login(&reg, 1, "ana").await;
    let entity = c.entity;
    tokio::time::sleep(Duration::from_secs(CEILING + 2)).await;

    ends_with_the_notice(&c.until_closed().await);
    assert_eq!(c.verdict().await, Some(ServerClose::IdleInput));
    assert_eq!(
        status(&reg, RoomId(1)).await,
        RoomStatus::Running { members: 1 },
        "the park holds its slot"
    );
    let back = Client::login(&reg, 2, "ana").await;
    assert_eq!(back.entity, entity, "the parked entity, resumed");
    assert_eq!(
        status(&reg, RoomId(1)).await,
        RoomStatus::Running { members: 1 }
    );
}

/// The sharded room: the shard asks, the registry closes, and the
/// member slot the grid's cap counts is handed back — a full room (cap
/// 1) admits the next player.
#[tokio::test(start_paused = true)]
async fn a_sharded_disconnect_frees_the_member_slot() {
    let (disc, mut disconnects) = mpsc::unbounded_channel();
    let reg = registry(
        logic::factory(Detach::Despawn, true, disc),
        AfkAction::Disconnect,
        Some(1),
    )
    .await;
    let mut c = Client::login(&reg, 1, "ana").await;
    tokio::time::sleep(Duration::from_secs(CEILING + 2)).await;

    ends_with_the_notice(&c.until_closed().await);
    assert!(disconnects.try_recv().is_ok(), "the shard ran the policy");
    assert_eq!(c.verdict().await, Some(ServerClose::IdleInput));
    let next = Client::login(&reg, 2, "bo").await;
    assert_ne!(next.entity, 0, "the freed slot admitted the next player");
}
