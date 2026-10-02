//! The registry's side of the default action (BACKLOG B40), driven with
//! the raw `RegistryMsg::LeaveConn`: the connection is told it is out of
//! the room (`ConnIn::LeftRoom` — never a close), a despawned membership
//! frees its slot, a parked one MOVES to its own row under the park key
//! (slot held, released by the park's end), and a stale request settles
//! nothing. (The races around it: `leave_races.rs`.)

use super::*;
use gsb_core::registry::LeaveRequest;

pub(super) fn config() -> RoomConfig {
    RoomConfig {
        id: RoomId(1),
        tick_hz: 60.0,
        ..Default::default()
    }
}

pub(super) async fn open(tx: &Mailbox<RegistryMsg>, conn: u64) -> mpsc::Receiver<ConnIn> {
    let (inbox, rx) = mpsc::channel(16);
    tx.send(RegistryMsg::ConnOpened {
        conn: ConnectionId(conn),
        inbox,
        source: None,
    })
    .await
    .expect("registry alive");
    rx
}

pub(super) async fn join(tx: &Mailbox<RegistryMsg>, conn: u64, identity: &str) -> EntityId {
    let (reply, rx) = oneshot::channel();
    let (out, _out) = mpsc::channel(64);
    tx.send(RegistryMsg::SpawnPlayer {
        conn: ConnectionId(conn),
        room: RoomId(1),
        out,
        identity: identity.to_string(),
        reply,
        claims: None,
    })
    .await
    .expect("registry alive");
    let seat = tokio::time::timeout(WAIT, rx).await.expect("in time");
    seat.expect("reply").expect("joined").entity
}

pub(super) fn leave(conn: u64, entity: EntityId, park: bool) -> RegistryMsg {
    RegistryMsg::LeaveConn(LeaveRequest {
        conn: ConnectionId(conn),
        room: RoomId(1),
        entity,
        park: park.then(|| ConnectionId(conn).park_key()),
    })
}

/// What the registry told the connection, if anything arrives.
async fn told(inbox: &mut mpsc::Receiver<ConnIn>, within: Duration) -> Option<RoomId> {
    match tokio::time::timeout(within, inbox.recv()).await {
        Ok(Some(ConnIn::LeftRoom { room })) => Some(room),
        // The registry let go of the inbox (a dead transport's row).
        Ok(None) | Err(_) => None,
        Ok(other) => panic!("not a left-room notice: {other:?}"),
    }
}

/// The registry's connection-table size (one probe open flushes a sample).
async fn table_size(tx: &Mailbox<RegistryMsg>, metrics: &mut mpsc::Receiver<MetricsEvent>) -> u32 {
    let _probe = open(tx, 999).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let mut last = None;
    while let Ok(ev) = metrics.try_recv() {
        if let MetricsEvent::Registry(s) = ev {
            last = Some(s.conns);
        }
    }
    last.expect("a registry sample") - 1
}

pub(super) fn members(n: u32) -> RoomStatus {
    RoomStatus::Running { members: n }
}

/// A despawned membership: the connection is told it left (no close),
/// and the room no longer counts it.
#[tokio::test]
async fn a_leave_request_frees_a_despawned_slot_and_tells_the_connection() {
    let (disc, _d) = mpsc::unbounded_channel();
    let (tx, mut metrics) = start(logic::factory(Detach::Despawn, false, disc));
    create(&tx, config()).await;
    let mut inbox = open(&tx, 1).await;
    let entity = join(&tx, 1, "ana").await;

    tx.send(leave(1, entity, false)).await.expect("sent");
    assert_eq!(told(&mut inbox, WAIT).await, Some(RoomId(1)));
    assert_eq!(status(&tx, RoomId(1)).await, members(0));
    assert_eq!(table_size(&tx, &mut metrics).await, 1, "the live row stays");
}

/// A parked membership moves to a row of its own under the park key:
/// the slot stays held, the connection's row is free — and the park's
/// end (its report names the key) releases the park row alone.
#[tokio::test]
async fn a_parked_leave_request_moves_the_membership_to_the_park_row() {
    let (disc, _d) = mpsc::unbounded_channel();
    let (tx, mut metrics) = start(logic::factory(HOLD, false, disc));
    create(&tx, config()).await;
    let mut inbox = open(&tx, 1).await;
    let entity = join(&tx, 1, "ana").await;

    tx.send(leave(1, entity, true)).await.expect("sent");
    assert_eq!(told(&mut inbox, WAIT).await, Some(RoomId(1)));
    assert_eq!(status(&tx, RoomId(1)).await, members(1), "the park holds");
    assert_eq!(table_size(&tx, &mut metrics).await, 2, "two rows");

    tx.send(RegistryMsg::DetachDespawned {
        conn: ConnectionId(1).park_key(),
        room: RoomId(1),
    })
    .await
    .expect("sent");
    assert_eq!(status(&tx, RoomId(1)).await, members(0), "the park let go");
    tx.send(RegistryMsg::ConnClosed {
        conn: ConnectionId(1),
        verdict: None,
    })
    .await
    .expect("sent");
    assert_eq!(table_size(&tx, &mut metrics).await, 0, "nothing left");
}

/// The transport died before the request landed: the row held only the
/// membership, so it becomes the park row (one row, slot held) and
/// nobody is told anything.
#[tokio::test]
async fn a_parked_leave_request_after_the_transport_died_rekeys_the_row() {
    let (disc, mut disconnects) = mpsc::unbounded_channel();
    let (tx, mut metrics) = start(logic::factory(HOLD, false, disc));
    create(&tx, config()).await;
    let mut inbox = open(&tx, 1).await;
    let entity = join(&tx, 1, "ana").await;
    tx.send(RegistryMsg::ConnClosed {
        conn: ConnectionId(1),
        verdict: None,
    })
    .await
    .expect("sent");
    tokio::time::timeout(WAIT, disconnects.recv())
        .await
        .expect("the room ran the policy")
        .expect("open");

    tx.send(leave(1, entity, true)).await.expect("sent");
    assert_eq!(told(&mut inbox, Duration::from_millis(200)).await, None);
    assert_eq!(status(&tx, RoomId(1)).await, members(1));
    assert_eq!(
        table_size(&tx, &mut metrics).await,
        1,
        "one row: the park's"
    );
}

/// A request for another membership than the row's current one settles
/// nothing and tells nobody.
#[tokio::test]
async fn a_stale_leave_request_is_a_no_op() {
    let (disc, _d) = mpsc::unbounded_channel();
    let (tx, _m) = start(logic::factory(Detach::Despawn, false, disc));
    create(&tx, config()).await;
    let mut inbox = open(&tx, 1).await;
    let entity = join(&tx, 1, "ana").await;

    tx.send(leave(1, entity + 99, false)).await.expect("sent");
    tx.send(leave(7, entity, false)).await.expect("sent");
    assert_eq!(told(&mut inbox, Duration::from_millis(200)).await, None);
    assert_eq!(status(&tx, RoomId(1)).await, members(1), "nothing settled");
}

/// One park per key: a second park of the same connection while the
/// first one's row still holds the key settles as a despawn — its slot
/// comes back (on the grid: the member count the cap reads), and the
/// park row that holds the key is untouched.
#[tokio::test]
async fn a_taken_park_key_settles_as_a_despawn() {
    let (disc, _d) = mpsc::unbounded_channel();
    let (tx, mut metrics) = start(logic::factory(HOLD, true, disc));
    create(
        &tx,
        RoomConfig {
            max_players: Some(2),
            ..config()
        },
    )
    .await;
    let mut inbox = open(&tx, 1).await;
    let first = join(&tx, 1, "ana").await;
    tx.send(leave(1, first, true)).await.expect("sent");
    assert!(told(&mut inbox, WAIT).await.is_some());
    // Anonymous rejoin: no resume, the park row stays.
    let second = join(&tx, 1, "").await;
    assert_eq!(status(&tx, RoomId(1)).await, members(2));

    tx.send(leave(1, second, true)).await.expect("sent");
    assert!(told(&mut inbox, WAIT).await.is_some());
    assert_eq!(status(&tx, RoomId(1)).await, members(1), "the first park");
    assert_eq!(table_size(&tx, &mut metrics).await, 2);
    let _two = open(&tx, 2).await;
    join(&tx, 2, "bo").await;
}
