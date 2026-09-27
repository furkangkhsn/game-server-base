//! The DEFAULT action (`afk_action = leave_room`, BACKLOG B40) end to end:
//! the ceiling ends the membership, the connection stays open and is in
//! no room afterwards — as after its own `LEAVE_ROOM_REQ` — whatever the
//! disconnect policy decided:
//!
//! - nothing goes on the wire at the kick;
//! - a game frame is answered as any frame outside a room is (`ERROR 6`,
//!   race class — the connection stays);
//! - a `JOIN_ROOM_REQ` goes straight through (no `LEAVE_ROOM_REQ` first),
//!   and resumes a PARKED entity (the implicit resume);
//! - the registry row holds no affiliation: a despawn's slot is free at
//!   once, a park keeps its slot on its own row until the park ends, and
//!   nothing leaks when the connection later closes.

use super::*;
use rig::{CEILING, Client, GAME_OP, registry};

fn game_frame() -> FrameBody {
    FrameBody::new(GAME_OP, Heartbeat { tick: 1 }.encode_to_vec())
}

/// A park that ends toward despawn a few virtual seconds after it starts.
const SHORT_HOLD: Detach = Detach::Hold {
    grace: Some(Duration::from_secs(5)),
    to: ExpireTo::Despawn,
};

/// Past the ceiling (the sweep examines every member within a tick here).
async fn idle_out() {
    tokio::time::sleep(Duration::from_secs(CEILING + 2)).await;
}

/// The next frame's ERROR code (any other frame fails the test).
async fn next_error(c: &mut Client) -> base::ErrorCode {
    let batch = tokio::time::timeout(WAIT, c.out.recv())
        .await
        .expect("an answer in time")
        .expect("out open");
    let f = batch.first().expect("one frame");
    assert_eq!(f.op, op::base::ERROR, "{batch:?}");
    base::Error::decode(&f.payload[..]).expect("decodes").code()
}

fn members(n: u32) -> RoomStatus {
    RoomStatus::Running { members: n }
}

/// Despawn policy, one room: the slot is free at once, a game frame
/// gets `ERROR 6` (not silence, not a closed connection), and a direct
/// JOIN admits the player again.
#[tokio::test(start_paused = true)]
async fn a_despawned_member_is_out_of_the_room_and_joins_directly() {
    let (disc, mut disconnects) = mpsc::unbounded_channel();
    let reg = registry(
        logic::factory(Detach::Despawn, false, disc),
        AfkAction::LeaveRoom,
        None,
    )
    .await;
    let mut c = Client::login(&reg, 1, "ana").await;
    idle_out().await;

    assert!(disconnects.try_recv().is_ok(), "the ceiling ran the policy");
    assert!(c.received().is_empty(), "not one byte on the wire");
    assert_eq!(status(&reg, RoomId(1)).await, members(0), "slot freed");

    for _ in 0..3 {
        c.send(game_frame()).await;
        assert_eq!(next_error(&mut c).await, base::ErrorCode::NotInRoom);
    }
    assert!(!c.actor.is_finished(), "a race-class answer, not a close");
    c.join().await;
    assert_eq!(status(&reg, RoomId(1)).await, members(1));
    assert_eq!(c.verdict().await, None);
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(status(&reg, RoomId(1)).await, members(0), "no leak");
}

/// Despawn policy, the connection never comes back and closes later:
/// its registry row goes with it (it used to stay `detached` with the
/// slot, for good).
#[tokio::test(start_paused = true)]
async fn a_despawned_member_that_closes_later_leaves_no_row() {
    let (disc, _d) = mpsc::unbounded_channel();
    let reg = registry(
        logic::factory(Detach::Despawn, false, disc),
        AfkAction::LeaveRoom,
        None,
    )
    .await;
    let c = Client::login(&reg, 1, "ana").await;
    idle_out().await;
    assert_eq!(c.verdict().await, None);
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(status(&reg, RoomId(1)).await, members(0));
}

/// Park policy, one room: the park holds its slot, the client's game
/// frames are answered `ERROR 6` (they used to pile up in the parked
/// row's channel), and a DIRECT join resumes the parked entity (it used
/// to get `ERROR 3`, a hard violation).
#[tokio::test(start_paused = true)]
async fn a_parked_member_joins_directly_and_resumes_its_entity() {
    let (disc, _d) = mpsc::unbounded_channel();
    let reg = registry(
        logic::factory(HOLD, false, disc),
        AfkAction::LeaveRoom,
        None,
    )
    .await;
    let mut c = Client::login(&reg, 1, "ana").await;
    let entity = c.entity;
    idle_out().await;

    assert!(c.received().is_empty(), "not one byte on the wire");
    assert_eq!(status(&reg, RoomId(1)).await, members(1), "the park holds");
    c.send(game_frame()).await;
    assert_eq!(next_error(&mut c).await, base::ErrorCode::NotInRoom);

    c.join().await;
    assert_eq!(c.entity, entity, "the parked entity, resumed");
    assert_eq!(status(&reg, RoomId(1)).await, members(1));
    assert_eq!(c.verdict().await, None);
}

/// Park policy, the connection closes without coming back: the park
/// keeps its slot until the hold ends, then everything is released.
#[tokio::test(start_paused = true)]
async fn a_park_left_behind_holds_its_slot_until_the_hold_ends() {
    let (disc, _d) = mpsc::unbounded_channel();
    let reg = registry(
        logic::factory(SHORT_HOLD, false, disc),
        AfkAction::LeaveRoom,
        None,
    )
    .await;
    let c = Client::login(&reg, 1, "ana").await;
    idle_out().await;
    assert_eq!(c.verdict().await, None);
    assert_eq!(status(&reg, RoomId(1)).await, members(1), "the park holds");
    tokio::time::sleep(Duration::from_secs(6)).await;
    assert_eq!(
        status(&reg, RoomId(1)).await,
        members(0),
        "and then lets go"
    );
}

/// Park policy, ANOTHER connection of the same player resumes the park
/// while the kicked one is still open: one member, one entity.
#[tokio::test(start_paused = true)]
async fn another_session_resumes_the_park_left_behind() {
    let (disc, _d) = mpsc::unbounded_channel();
    let reg = registry(
        logic::factory(HOLD, false, disc),
        AfkAction::LeaveRoom,
        None,
    )
    .await;
    let c = Client::login(&reg, 1, "ana").await;
    let entity = c.entity;
    idle_out().await;

    let back = Client::login(&reg, 2, "ana").await;
    assert_eq!(back.entity, entity, "the park, resumed by the new session");
    assert_eq!(status(&reg, RoomId(1)).await, members(1));
    assert_eq!(c.verdict().await, None, "the old one was not in the room");
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(status(&reg, RoomId(1)).await, members(1));
}

/// Despawn policy on the grid (cap 1): the member slot comes back at
/// once, so the next player is admitted — and the kicked one, closing
/// later, leaks nothing.
#[tokio::test(start_paused = true)]
async fn a_sharded_despawn_frees_the_member_slot() {
    let (disc, _d) = mpsc::unbounded_channel();
    let reg = registry(
        logic::factory(Detach::Despawn, true, disc),
        AfkAction::LeaveRoom,
        Some(1),
    )
    .await;
    let c = Client::login(&reg, 1, "ana").await;
    idle_out().await;
    assert_eq!(status(&reg, RoomId(1)).await, members(0), "slot freed");
    let next = Client::login(&reg, 2, "bo").await;
    assert_ne!(next.entity, 0, "the freed slot admitted the next player");
    assert_eq!(c.verdict().await, None);
    assert_eq!(status(&reg, RoomId(1)).await, members(1));
}

/// Park policy on a FULL grid (cap 1): the park keeps the one slot, yet
/// the kicked player's direct join resumes it (the grid's cap does not
/// count a resume of the identity's own park twice).
#[tokio::test(start_paused = true)]
async fn a_sharded_park_resumes_on_a_full_grid() {
    let (disc, _d) = mpsc::unbounded_channel();
    let reg = registry(
        logic::factory(HOLD, true, disc),
        AfkAction::LeaveRoom,
        Some(1),
    )
    .await;
    let mut c = Client::login(&reg, 1, "ana").await;
    let entity = c.entity;
    idle_out().await;
    assert!(c.received().is_empty(), "not one byte on the wire");
    assert_eq!(status(&reg, RoomId(1)).await, members(1), "the park holds");

    c.join().await;
    assert_eq!(c.entity, entity, "the parked entity, resumed");
    assert_eq!(status(&reg, RoomId(1)).await, members(1));
    assert_eq!(c.verdict().await, None);
    assert_eq!(status(&reg, RoomId(1)).await, members(1), "parked again");
}
