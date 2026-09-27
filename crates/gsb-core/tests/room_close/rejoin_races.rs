//! A close verdict that a fresh rejoin overtakes (BACKLOG B43). The room
//! ended the membership (the input-idle ceiling under `afk_action =
//! disconnect`, or the game's kick) and despawned it; its close request
//! waits behind a registry mailbox that stays full for the room's
//! `try_send` (the relay of `rejoin_rig.rs` holds it), while the client's
//! game frame hits the closed action channel (`ERROR 6`) and its awaited
//! `JOIN_ROOM_REQ` gets through: a new membership, a NEW entity. The late
//! request names the old membership — and still closes the connection:
//! the verdict is the connection's (`ConnectionId`s are never reused),
//! only the table settlement is the membership's. The new membership ends
//! the way every membership of a closing connection ends — the
//! transport-death path, the game's `on_disconnect`, once — and
//! nothing is left behind: no member, no row, the slot free.

use super::*;
use rejoin_rig::{Hook, factory, held_registry, table_size};
use rig::{CEILING, Client, GAME_OP};

/// How long the verdict may take to land once the request is handed
/// over: well inside the ceiling, so the NEW membership's own idle
/// expiry cannot be what closes the socket.
const LANDS: Duration = Duration::from_millis(500);

fn config(ceiling: Option<u64>) -> RoomConfig {
    RoomConfig {
        id: RoomId(1),
        max_idle_input_secs: ceiling,
        afk_action: AfkAction::Disconnect,
        max_players: Some(1),
        ..Default::default()
    }
}

fn game_frame() -> FrameBody {
    FrameBody::new(GAME_OP, Heartbeat { tick: 1 }.encode_to_vec())
}

/// Every hook the logic ran so far.
fn hooks(rx: &mut mpsc::UnboundedReceiver<Hook>) -> Vec<Hook> {
    std::iter::from_fn(|| rx.try_recv().ok()).collect()
}

/// The race, from the held request on: the client finds itself out of
/// the room, rejoins as a new entity, and only then does the request
/// reach the registry. Returns the frames the client got until its
/// connection closed.
async fn rejoin_then_land(
    reg: &Mailbox<RegistryMsg>,
    held: &mut mpsc::UnboundedReceiver<CloseRequest>,
    c: &mut Client,
    cause: ServerClose,
) -> Vec<FrameBody> {
    let req = tokio::time::timeout(WAIT, held.recv())
        .await
        .expect("the room asked in time")
        .expect("relay open");
    assert_eq!(
        (req.conn, req.entity, req.cause),
        (ConnectionId(1), c.entity, cause)
    );
    let old = c.entity;

    // The first frame hits the closed action channel (a silent detach),
    // the second is answered as any frame outside a room.
    c.send(game_frame()).await;
    c.send(game_frame()).await;
    let answer = tokio::time::timeout(WAIT, c.out.recv())
        .await
        .expect("an answer")
        .expect("out open");
    let error = base::Error::decode(&answer[0].payload[..]).expect("an ERROR");
    assert_eq!(
        error.code(),
        base::ErrorCode::NotInRoom,
        "the client is out"
    );
    c.join().await;
    assert_ne!(c.entity, old, "a fresh membership: the race is on");

    reg.send(RegistryMsg::CloseConn(req))
        .await
        .expect("registry alive");
    let mut frames = Vec::new();
    loop {
        match tokio::time::timeout(LANDS, c.out.recv()).await {
            Ok(Some(batch)) => frames.extend(batch),
            Ok(None) => return frames,
            Err(_) => panic!("the verdict did not close the connection: {frames:?}"),
        }
    }
}

/// The last frame is the one notice — ERROR 9 with `reason`.
fn ends_with(frames: &[FrameBody], reason: &str) {
    let last = frames.last().expect("at least the notice");
    assert_eq!(last.op, op::base::ERROR, "{frames:?}");
    let error = base::Error::decode(&last.payload[..]).expect("an ERROR");
    assert_eq!(error.code(), base::ErrorCode::ServerClosed);
    assert_eq!(error.message, reason);
    assert_eq!(frames.iter().filter(|f| f.op == op::base::ERROR).count(), 1);
}

/// What is left once the connection is gone: each membership ended once,
/// the same way — `on_disconnect` (the new one by the close), then the
/// despawn's `on_leave` — no member, no row, and the one slot takes a new
/// player.
async fn nothing_left(
    reg: &Mailbox<RegistryMsg>,
    metrics: &mut mpsc::Receiver<MetricsEvent>,
    log: &mut mpsc::UnboundedReceiver<Hook>,
    sharded: bool,
) {
    tokio::time::sleep(Duration::from_millis(100)).await;
    let (p1, p2) = (PlayerId(1), PlayerId(2));
    assert_eq!(
        hooks(log),
        vec![
            Hook::Join(p1, 1),
            Hook::Disconnect(p1),
            Hook::Leave(p1),
            Hook::Join(p2, 2),
            Hook::Disconnect(p2),
            Hook::Leave(p2),
        ],
        "sharded={sharded}"
    );
    assert_eq!(
        status(reg, RoomId(1)).await,
        RoomStatus::Running { members: 0 },
        "sharded={sharded}"
    );
    assert_eq!(
        table_size(reg, metrics).await,
        0,
        "sharded={sharded}: no row"
    );
    let other = Client::login(reg, 2, "bo").await;
    assert_eq!(other.entity, 3, "sharded={sharded}: the slot is free");
}

/// The idle ceiling under `afk_action = disconnect`.
#[tokio::test(start_paused = true)]
async fn an_idle_close_lands_after_a_fresh_rejoin() {
    for sharded in [false, true] {
        let (log_tx, mut log) = mpsc::unbounded_channel();
        let (reg, mut held, mut metrics) =
            held_registry(factory(sharded, false, log_tx), config(Some(CEILING))).await;
        let mut c = Client::login(&reg, 1, "ana").await;
        tokio::time::sleep(Duration::from_secs(CEILING + 1)).await;

        let frames = rejoin_then_land(&reg, &mut held, &mut c, ServerClose::IdleInput).await;
        ends_with(
            &frames,
            &format!(
                "input idle: no game input for {CEILING} s (the room's \
                 max_idle_input_secs; afk_action = disconnect)"
            ),
        );
        assert_eq!(c.verdict().await, Some(ServerClose::IdleInput));
        nothing_left(&reg, &mut metrics, &mut log, sharded).await;
    }
}

/// The game's kick (E8): a kicked client cannot evade the kick by
/// rejoining while the request waits.
#[tokio::test(start_paused = true)]
async fn a_kick_lands_after_a_fresh_rejoin() {
    for sharded in [false, true] {
        let (log_tx, mut log) = mpsc::unbounded_channel();
        let (reg, mut held, mut metrics) =
            held_registry(factory(sharded, true, log_tx), config(None)).await;
        let mut c = Client::login(&reg, 1, "ana").await;
        c.send(game_frame()).await;

        let frames = rejoin_then_land(&reg, &mut held, &mut c, ServerClose::Kicked).await;
        ends_with(&frames, "kicked: cheating");
        assert_eq!(c.verdict().await, Some(ServerClose::Kicked));
        nothing_left(&reg, &mut metrics, &mut log, sharded).await;
    }
}
