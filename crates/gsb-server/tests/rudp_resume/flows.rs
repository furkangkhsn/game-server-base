//! The scenarios, each a function of the door (and the room's shape),
//! so the rUDP run and its TCP twin are one flow with per-door
//! expectations.

use gsb_core::conn::ServerClose;
use gsb_protocol::base::ErrorCode;

use super::player::{Player, warm_during};
use super::rig::{self, Door, RegCounts, Rig, RoomCounts, Seen, Shape};

/// The park grace of the resume scenarios: far beyond the drop →
/// detect → reconnect gap, so a resume never races an expiry.
pub const LONG_GRACE: f64 = 60.0;

/// A player joined as `name` and seen in game (its entity in the
/// world, its own input moving it).
pub async fn in_game(rig: &Rig, door: Door, name: &str) -> Player {
    let mut p = Player::connect(door, rig.addr(), rig.key()).await;
    p.join(name).await;
    p.moves().await;
    p
}

/// Drop `old` silently — no LEAVE, nothing on the wire — and return a
/// guard holding its local port on rUDP, so the next client cannot be
/// handed the same one: the reconnect is from a NEW 4-tuple, the NAT
/// rebinding of today's address-keyed demux.
pub fn vanish(old: Player) -> (Option<std::net::UdpSocket>, Option<std::net::SocketAddr>) {
    let local = old.udp_local();
    drop(old);
    let guard = local.and_then(|a| std::net::UdpSocket::bind(("0.0.0.0", a.port())).ok());
    (guard, local)
}

/// A new connection from a new local address.
pub async fn reconnect(rig: &Rig, door: Door, old: Option<std::net::SocketAddr>) -> Player {
    let p = Player::connect(door, rig.addr(), rig.key()).await;
    if let (Some(old), Some(new)) = (old, p.udp_local()) {
        assert_ne!(old.port(), new.port(), "the reconnect is from a new port");
    }
    p
}

/// The registry and the close family after one vanished session and
/// its return: two connections opened, one closed (how the server
/// learned of it is the door's: [`Door::drop_close`]), one live; the
/// registry books a leave only where the room released the row
/// (`released`: the despawn report of a park that never started).
pub fn after_one_drop(s: &Seen, door: Door, released: u64) {
    assert_eq!(
        s.reg,
        RegCounts {
            opens: 2,
            closes: 1,
            conns: 1,
            joins: 2,
            leaves: released,
        },
        "{door:?}: {s:?}"
    );
    let want: Vec<_> = door.drop_close().into_iter().collect();
    s.assert_closes(&want);
}

/// Scenario 1 (and 4 on the sharded grid): the client vanishes, the
/// server notices on its own (TCP: EOF; rUDP: the idle sweep), the room
/// PARKS the entity; a new client — a new socket, a new handshake —
/// presents the same credentials and RESUMES it.
pub async fn vanish_then_resume(door: Door, shape: Shape) {
    let mut rig = Rig::start(rig::config(door, shape, LONG_GRACE)).await;
    let first = in_game(&rig, door, "rider").await;
    let entity = first.entity;
    let (_port, old) = vanish(first);

    // The server's own word that the session ended and the entity is
    // parked, not gone: one close, one parked row, the slot held.
    let parked = rig
        .until("the vanished session to be parked", |s| {
            s.reg.closes == 1 && s.room.detached == 1
        })
        .await;
    let drop_close: Vec<_> = door.drop_close().into_iter().collect();
    parked.assert_closes(&drop_close);
    assert_eq!(parked.room.resumes, 0, "{parked:?}");
    assert_eq!(rig.members().await, 1, "the park holds the slot");

    let mut again = reconnect(&rig, door, old).await;
    assert_eq!(
        again.join("rider").await,
        entity,
        "{door:?}/{shape:?}: the resume brings the SAME entity back"
    );
    again.moves().await;

    let s = warm_during(&mut again, async {
        rig.until("the resume to be counted", |s| {
            s.room.resumes == 1 && s.room.detached == 0
        })
        .await;
        rig.settle(2).await
    })
    .await;
    assert_eq!(
        s.room,
        RoomCounts {
            joins: 1,
            resumes: 1,
            ..Default::default()
        },
        "{door:?}/{shape:?}: one join, one resume, nothing parked, left or expired"
    );
    after_one_drop(&s, door, 0);
    let rows = match shape {
        Shape::Single => 1,
        Shape::Sharded => gsb_server::Config::default().shard_count as usize,
    };
    assert_eq!(rig.rows(), rows, "{shape:?}: room 1's metric rows");
    assert_eq!(rig.members().await, 1);
    rig.stop().await;
}

/// Scenario 5 (B3, `udp_migration` on): the client's address changes
/// mid-game — its socket is replaced, a network change — and the
/// SESSION moves with it after path validation: no new handshake, no
/// resume, no close. The registry sees one connection throughout, the
/// room one join and nothing parked, and the same entity keeps moving by
/// the new socket's input (RECONNECT §5: the migration expectation next
/// to the fallback's).
pub async fn migrate(door: Door) {
    let mut rig = Rig::start(rig::config(door, Shape::Single, LONG_GRACE)).await;
    let mut p = in_game(&rig, door, "nomad").await;
    let entity = p.entity;
    let before = p.udp_local().expect("rUDP");
    let after = p.rebind().await;
    assert_ne!(before.port(), after.port(), "a new local socket");
    p.moves().await;
    let s = warm_during(&mut p, async {
        rig.until("the migration to be counted", |s| s.migrations == 1)
            .await;
        rig.settle(2).await
    })
    .await;
    assert_eq!(
        s.room,
        RoomCounts {
            joins: 1,
            ..Default::default()
        },
        "{door:?}: one join, no resume, nothing parked"
    );
    assert_eq!(
        s.reg,
        RegCounts {
            opens: 1,
            closes: 0,
            conns: 1,
            joins: 1,
            leaves: 0,
        },
        "{door:?}: one connection throughout: {s:?}"
    );
    s.assert_closes(&[]);
    assert_eq!(s.migrations, 1);
    assert_eq!(p.entity, entity);
    p.moves().await;
    assert_eq!(rig.members().await, 1);
    rig.stop().await;
}

/// Scenario 2 (F32): the new session arrives while the old one is still
/// LIVE on the server (rUDP: before its idle sweep; the old client
/// keeps its socket). Latest wins: the old session is closed with
/// `ERROR` 9, booked as `superseded`, and its membership — the same
/// entity — is handed to the new one.
pub async fn takeover(door: Door) {
    let mut rig = Rig::start(rig::config(door, Shape::Single, LONG_GRACE)).await;
    let mut first = in_game(&rig, door, "twin").await;
    let entity = first.entity;

    let mut second = Player::connect(door, rig.addr(), rig.key()).await;
    if let (Some(a), Some(b)) = (first.udp_local(), second.udp_local()) {
        assert_ne!(a, b, "two live sockets, two 4-tuples");
    }
    assert_eq!(
        second.join("twin").await,
        entity,
        "{door:?}: handed over, not left"
    );
    let notice = first.closed_by_server().await;
    assert_eq!(
        (notice.code, notice.raw),
        (ErrorCode::ServerClosed, 9),
        "{door:?}: {notice}"
    );
    assert!(notice.message.contains("superseded"), "{door:?}: {notice}");
    second.moves().await;

    // The old connection's teardown has reached the registry; its late
    // DETACH (if any reached the room) found nothing to park.
    let s = warm_during(&mut second, async {
        rig.until("the superseded session to close", |s| s.reg.closes == 1)
            .await;
        rig.settle(3).await
    })
    .await;
    assert_eq!(
        s.room,
        RoomCounts {
            joins: 1,
            resumes: 1,
            ..Default::default()
        },
        "{door:?}: the handover is a resume; no second entity, no park"
    );
    assert_eq!(
        s.reg,
        RegCounts {
            opens: 2,
            closes: 1,
            conns: 1,
            joins: 2,
            leaves: 0,
        },
        "{door:?}: a handover is no leave: {s:?}"
    );
    s.assert_closes(&[ServerClose::Superseded]);
    assert_eq!(rig.members().await, 1, "one identity, one member");
    second.moves().await;
    rig.stop().await;
}
