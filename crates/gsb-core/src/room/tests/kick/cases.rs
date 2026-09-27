//! The kick's timing and edges: the broadcast-phase kick, each `Detach`
//! answer honoured, the no-op cases (not a live member here), the double
//! kick, the full mailbox, the bounded reason, and the room without a
//! registry.

use super::*;

const PARK: Detach = Detach::Hold {
    grace: Some(Duration::from_secs(600)),
    to: ExpireTo::Despawn,
};

/// A hold that is over at the first sweep, toward a bot.
const TO_BOT: Detach = Detach::Hold {
    grace: Some(Duration::ZERO),
    to: ExpireTo::AiHandover,
};

/// A kick asked in a broadcast-phase hook (`snapshot`) is applied at the
/// end of the tick: the member got this tick's batch, then its policy ran.
#[test]
fn a_kick_asked_while_broadcasting_is_applied_at_the_end_of_the_tick() {
    let mut r = Rig::new(81, Detach::Despawn, vec![kick(1, At::Snapshot, 1, "")]);
    let mut reg = r.registry(64);
    let (_e, _a) = r.join(ConnectionId(1), "ana");
    r.step(1);
    assert_eq!(
        r.events(),
        vec![
            Ev::Ingest(1),
            Ev::Update(1),
            Ev::Snapshot(1),
            Ev::Disconnect(PlayerId(1), "ana".into()),
            Ev::Leave(PlayerId(1)),
        ]
    );
    assert_eq!(r.batches(ConnectionId(1)), 1, "this tick's batch went out");
    r.step(2);
    let got = closes(&mut reg);
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].reason, "kicked", "an empty reason still says why");
}

/// A policy that PARKS the kicked member: the entity stays (no leave),
/// the row is detached with its socket half released (else the socket
/// could never close), and the close says `parked`, so the registry
/// keeps the row for a resume.
#[test]
fn a_parked_kick_keeps_the_entity_and_says_parked() {
    let mut r = Rig::new(82, PARK, vec![kick(1, At::Update, 1, "afk")]);
    let mut reg = r.registry(64);
    let (_e, _a) = r.join(ConnectionId(1), "ana");
    r.step(1);
    r.step(2);
    let ev = r.events();
    assert!(ev.contains(&Ev::Disconnect(PlayerId(1), "ana".into())));
    assert!(
        !ev.contains(&Ev::Leave(PlayerId(1))),
        "parked, not despawned"
    );
    let row = &r.actor.conns[&PlayerId(1)];
    assert!(
        row.detached && row.out.is_closed(),
        "parked, socket half released"
    );
    let got = closes(&mut reg);
    assert_eq!(got.len(), 1);
    assert!(got[0].parked, "{got:?}");
}

/// A policy that hands the kicked member to a bot: the park ends in AI
/// handover and the close still says parked (the bot holds the slot).
#[test]
fn an_ai_handover_kick_is_played_on_by_the_bot() {
    let mut r = Rig::new(83, TO_BOT, vec![kick(1, At::Ingest, 1, "afk")]);
    let mut reg = r.registry(64);
    let (_e, _a) = r.join(ConnectionId(1), "ana");
    r.step(1);
    r.step(2);
    assert!(r.actor.conns[&PlayerId(1)].bot_fed, "the bot took it over");
    let got = closes(&mut reg);
    assert_eq!(got.len(), 1);
    assert!(got[0].parked, "the membership lives on: {got:?}");
}

/// Not a live member here → no policy call, no close: an unknown
/// player, a row parked by a transport death, and a member whose own
/// leave the room processed first.
#[test]
fn a_kick_of_anyone_but_a_live_member_is_a_no_op() {
    let plan = vec![
        kick(1, At::Ingest, 99, "unknown"),
        kick(1, At::Ingest, 1, "parked"),
        kick(2, At::Ingest, 2, "left"),
    ];
    let mut r = Rig::new(84, PARK, plan);
    let mut reg = r.registry(64);
    let (e1, _a1) = r.join(ConnectionId(1), "ana");
    let (e2, _a2) = r.join(ConnectionId(2), "bora");
    r.actor.handle_control(RoomControl::Detach {
        conn: ConnectionId(1),
        entity: e1,
        identity: "ana".into(),
    });
    assert_eq!(r.disconnects(), 1, "the transport death parked it");
    r.step(1);
    r.actor.handle_control(RoomControl::Leave {
        conn: ConnectionId(2),
        entity: e2,
    });
    r.step(2);
    r.step(3);
    assert_eq!(r.disconnects(), 0, "no kick reached the policy");
    assert!(
        r.actor.conns[&PlayerId(1)].detached,
        "the park is untouched"
    );
    assert!(closes(&mut reg).is_empty(), "and none asked for a close");
    assert!(r.actor.close_requests.is_empty());
}

/// A bot-fed row (a park handed to the AI) is not a live member either.
#[test]
fn a_kick_of_a_bot_fed_row_is_a_no_op() {
    let mut r = Rig::new(89, TO_BOT, vec![kick(2, At::Update, 1, "bot")]);
    let mut reg = r.registry(64);
    let (e1, _a1) = r.join(ConnectionId(1), "ana");
    r.actor.handle_control(RoomControl::Detach {
        conn: ConnectionId(1),
        entity: e1,
        identity: "ana".into(),
    });
    r.step(1);
    assert!(r.actor.conns[&PlayerId(1)].bot_fed, "the bot took it over");
    assert_eq!(r.disconnects(), 1, "the transport death's policy call");
    r.step(2);
    r.step(3);
    assert_eq!(r.disconnects(), 0, "the kick reached nothing");
    assert!(r.actor.conns[&PlayerId(1)].bot_fed);
    assert!(closes(&mut reg).is_empty());
}

/// Two kicks of one member in one tick: one policy call, one close, the
/// first reason.
#[test]
fn a_double_kick_closes_once_with_the_first_reason() {
    let plan = vec![
        kick(1, At::Ingest, 1, "first"),
        kick(1, At::Update, 1, "second"),
        kick(1, At::Snapshot, 1, "third"),
    ];
    let mut r = Rig::new(85, PARK, plan);
    let mut reg = r.registry(64);
    let (_e, _a) = r.join(ConnectionId(1), "ana");
    r.step(1);
    r.step(2);
    assert_eq!(r.disconnects(), 1);
    let got = closes(&mut reg);
    assert_eq!(got.len(), 1, "{got:?}");
    assert_eq!(got[0].reason, "kicked: first");
}

/// E6's full-mailbox rule: the request waits, in the queue, for a tick
/// with room — it is never dropped. (The despawn arm's report shares the
/// mailbox and leaves first, in phase 0c: one slot, one message a tick.)
#[test]
fn a_full_registry_mailbox_gets_the_kick_on_a_later_tick() {
    let mut r = Rig::new(86, Detach::Despawn, vec![kick(1, At::Ingest, 1, "x")]);
    let mut reg = r.registry(1);
    let tx = r.actor.registry.clone().expect("attached");
    tx.try_send(RegistryMsg::Shutdown).expect("the one slot");
    let (_e, _a) = r.join(ConnectionId(1), "ana");
    r.step(1);
    r.step(2);
    assert_eq!(r.disconnects(), 1, "the policy ran on time");
    assert_eq!(r.actor.close_requests.len(), 1, "the request waits");
    assert!(matches!(reg.try_recv(), Ok(RegistryMsg::Shutdown)));
    let mut got = Vec::new();
    for k in 3..=5 {
        r.step(k);
        while let Ok(msg) = reg.try_recv() {
            got.push(match msg {
                RegistryMsg::DetachDespawned { .. } => "report".to_string(),
                RegistryMsg::CloseConn(req) => req.reason,
                other => format!("{other:?}"),
            });
        }
    }
    assert_eq!(
        got,
        vec!["report", "kicked: x"],
        "delivered once there was room"
    );
    assert!(r.actor.close_requests.is_empty());
}

/// The game's reason reaches the request bounded, cut on a `char`
/// boundary (a 2-byte character straddles the cap here).
#[test]
fn the_reason_is_bounded_on_a_char_boundary() {
    let long = format!("{}ğğğ", "a".repeat(crate::room::KICK_REASON_MAX_BYTES - 1));
    let mut r = Rig::new(87, Detach::Despawn, vec![kick(1, At::Ingest, 1, &long)]);
    let mut reg = r.registry(64);
    let (_e, _a) = r.join(ConnectionId(1), "ana");
    r.step(1);
    r.step(2);
    let got = closes(&mut reg);
    let want = format!(
        "kicked: {}",
        "a".repeat(crate::room::KICK_REASON_MAX_BYTES - 1)
    );
    assert_eq!(got[0].reason, want);
}

/// A standalone room (no registry) still ends the membership through the
/// policy; there is nobody to ask for the close, so nothing queues.
#[test]
fn a_room_without_a_registry_ends_the_membership_only() {
    let mut r = Rig::new(88, Detach::Despawn, vec![kick(1, At::Ingest, 1, "x")]);
    let (_e, _a) = r.join(ConnectionId(1), "ana");
    r.step(1);
    assert_eq!(r.disconnects(), 1);
    assert!(!r.actor.conns.contains_key(&PlayerId(1)));
    assert!(r.actor.close_requests.is_empty());
}
