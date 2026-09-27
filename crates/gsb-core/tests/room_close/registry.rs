//! The registry's side of the verb, driven with the raw message: no
//! room ended anything here — the tests hand the registry the request a
//! room would send and read what the registry does with it.

use super::*;

/// Room 1: single, or two shards; `cap` = `max_players`.
fn config(cap: Option<usize>) -> RoomConfig {
    RoomConfig {
        id: RoomId(1),
        tick_hz: 60.0,
        max_players: cap,
        ..Default::default()
    }
}

/// Open `conn` (its inbox is returned) without joining anything.
async fn open(tx: &Mailbox<RegistryMsg>, conn: u64) -> mpsc::Receiver<ConnIn> {
    let (inbox, rx) = mpsc::channel(16);
    tx.send(RegistryMsg::ConnOpened {
        conn: ConnectionId(conn),
        inbox,
    })
    .await
    .expect("registry alive");
    rx
}

/// Join room 1 as `conn` with `identity`.
async fn join(tx: &Mailbox<RegistryMsg>, conn: u64, identity: &str) -> Result<EntityId, CoreError> {
    let (reply, rx) = oneshot::channel();
    let (out, _out) = mpsc::channel(64);
    tx.send(RegistryMsg::SpawnPlayer {
        conn: ConnectionId(conn),
        room: RoomId(1),
        out,
        identity: identity.to_string(),
        reply,
    })
    .await
    .expect("registry alive");
    let seat = tokio::time::timeout(WAIT, rx).await.expect("in time");
    seat.expect("reply").map(|s| s.entity)
}

fn request(conn: u64, entity: EntityId, parked: bool) -> RegistryMsg {
    RegistryMsg::CloseConn(CloseRequest {
        conn: ConnectionId(conn),
        room: RoomId(1),
        entity,
        parked,
        cause: ServerClose::IdleInput,
        reason: "input idle: test".into(),
    })
}

/// The verdict the registry relayed to the connection, if any arrives.
async fn told(
    inbox: &mut mpsc::Receiver<ConnIn>,
    within: Duration,
) -> Option<(ServerClose, String)> {
    match tokio::time::timeout(within, inbox.recv()).await {
        Ok(Some(ConnIn::ServerClosed { cause, reason })) => Some((cause, reason)),
        Ok(other) => panic!("not a close: {other:?}"),
        Err(_) => None,
    }
}

/// The registry's own connection-table size, read after one more open
/// makes it flush a sample.
async fn table_size(tx: &Mailbox<RegistryMsg>, metrics: &mut mpsc::Receiver<MetricsEvent>) -> u32 {
    let _probe = open(tx, 999).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let mut last = None;
    while let Ok(ev) = metrics.try_recv() {
        if let MetricsEvent::Registry(s) = ev {
            last = Some(s.conns);
        }
    }
    // The probe itself is one row.
    last.expect("a registry sample") - 1
}

fn members(n: u32) -> RoomStatus {
    RoomStatus::Running { members: n }
}

/// A despawned membership: the connection is told the request's verdict
/// and reason, and the room no longer counts it.
#[tokio::test]
async fn a_close_request_tells_the_connection_and_ends_the_membership() {
    let (disc, _d) = mpsc::unbounded_channel();
    let (tx, _m) = start(logic::factory(Detach::Despawn, false, disc));
    create(&tx, config(None)).await;
    let mut inbox = open(&tx, 1).await;
    let entity = join(&tx, 1, "ana").await.expect("joined");
    assert_eq!(status(&tx, RoomId(1)).await, members(1));

    tx.send(request(1, entity, false)).await.expect("sent");
    let (cause, reason) = told(&mut inbox, WAIT)
        .await
        .expect("the connection is told");
    assert_eq!(cause, ServerClose::IdleInput);
    assert_eq!(reason, "input idle: test");
    assert_eq!(
        status(&tx, RoomId(1)).await,
        members(0),
        "the membership is over"
    );
}

/// A request for another membership than the row's current one — a
/// different entity, a different room: the connection left it or joined
/// again while the request waited behind a full mailbox (BACKLOG B43) —
/// settles nothing, and still closes the connection: the verdict judged
/// the connection, whose id is never reused. A request for a released
/// connection is a no-op.
#[tokio::test]
async fn a_close_request_for_an_earlier_membership_settles_nothing_but_closes() {
    let (disc, _d) = mpsc::unbounded_channel();
    let (tx, _m) = start(logic::factory(Detach::Despawn, false, disc));
    create(&tx, config(None)).await;
    let mut inbox = open(&tx, 1).await;
    let entity = join(&tx, 1, "ana").await.expect("joined");

    tx.send(request(7, entity, false))
        .await
        .expect("sent (unknown conn)");
    tx.send(request(1, entity + 99, false)).await.expect("sent");
    tx.send(RegistryMsg::CloseConn(CloseRequest {
        room: RoomId(2),
        ..match request(1, entity, false) {
            RegistryMsg::CloseConn(r) => r,
            _ => unreachable!(),
        }
    }))
    .await
    .expect("sent");
    for _ in 0..2 {
        let (cause, _) = told(&mut inbox, WAIT).await.expect("the verdict stands");
        assert_eq!(cause, ServerClose::IdleInput);
    }
    assert_eq!(status(&tx, RoomId(1)).await, members(1), "nothing settled");
}

/// A parked membership keeps its slot after the close (the park holds
/// it, §4) — and the row is marked detached at once, so a hold that ends
/// BEFORE the connection's own close is processed still releases it.
#[tokio::test]
async fn a_parked_close_request_keeps_the_slot_until_the_park_ends() {
    let (disc, _d) = mpsc::unbounded_channel();
    let (tx, mut metrics) = start(logic::factory(HOLD, false, disc));
    create(&tx, config(None)).await;
    let mut inbox = open(&tx, 1).await;
    let entity = join(&tx, 1, "ana").await.expect("joined");

    tx.send(request(1, entity, true)).await.expect("sent");
    assert!(
        told(&mut inbox, WAIT).await.is_some(),
        "the connection is told"
    );
    assert_eq!(
        status(&tx, RoomId(1)).await,
        members(1),
        "the park holds its slot"
    );

    // The hold ends toward despawn before the transport's close lands.
    tx.send(RegistryMsg::DetachDespawned {
        conn: ConnectionId(1),
        room: RoomId(1),
    })
    .await
    .expect("sent");
    assert_eq!(
        status(&tx, RoomId(1)).await,
        members(0),
        "the ended park lets go"
    );
    assert_eq!(
        table_size(&tx, &mut metrics).await,
        0,
        "and so does its row"
    );
}

/// The transport died first: its close kept the row for a park (here the
/// room really parked). A request saying the membership was DESPAWNED
/// then releases the row outright — nothing else ever would.
#[tokio::test]
async fn a_close_request_after_the_transport_died_releases_the_row() {
    let (disc, mut disconnects) = mpsc::unbounded_channel();
    let (tx, mut metrics) = start(logic::factory(HOLD, false, disc));
    create(&tx, config(None)).await;
    let _inbox = open(&tx, 1).await;
    let entity = join(&tx, 1, "ana").await.expect("joined");
    tx.send(RegistryMsg::ConnClosed {
        conn: ConnectionId(1),
    })
    .await
    .expect("sent");
    tokio::time::timeout(WAIT, disconnects.recv())
        .await
        .expect("the room ran the policy")
        .expect("open");
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        status(&tx, RoomId(1)).await,
        members(1),
        "parked: slot held"
    );

    tx.send(request(1, entity, false)).await.expect("sent");
    assert_eq!(status(&tx, RoomId(1)).await, members(0));
    assert_eq!(
        table_size(&tx, &mut metrics).await,
        0,
        "the row is released"
    );
}

/// On the grid the registry's member count is the room's cap: a
/// despawned membership hands its slot back at once, a parked one keeps
/// it until the park ends.
#[tokio::test]
async fn a_sharded_close_request_settles_the_member_slot() {
    let (disc, _d) = mpsc::unbounded_channel();
    let (tx, _m) = start(logic::factory(HOLD, true, disc));
    create(&tx, config(Some(1))).await;
    let mut one = open(&tx, 1).await;
    let entity = join(&tx, 1, "ana").await.expect("joined");
    let _two = open(&tx, 2).await;
    assert!(matches!(
        join(&tx, 2, "bo").await,
        Err(CoreError::RoomFull(1))
    ));

    tx.send(request(1, entity, true)).await.expect("sent");
    assert!(told(&mut one, WAIT).await.is_some());
    assert!(
        matches!(join(&tx, 2, "bo").await, Err(CoreError::RoomFull(1))),
        "a parked membership still holds the room's one slot"
    );

    // The park ends in a despawn (the room's report would say so; here
    // the request's despawn arm for the detached row does the same).
    tx.send(request(1, entity, false)).await.expect("sent");
    join(&tx, 2, "bo").await.expect("the slot came back");
}
