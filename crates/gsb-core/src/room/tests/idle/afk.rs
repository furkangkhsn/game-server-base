//! The ceiling's ACTION (BACKLOG E6): `afk_action = LeaveRoom` (the
//! default) asks for no close (its leave request — B40 — is `leave.rs`'s
//! subject); `Disconnect` asks it to close the member's connection AFTER the
//! disconnect policy ran, saying whether the policy parked the entity.
//! Parked and bot-fed rows are off the input clock, so they are never
//! closed. The request survives a full registry mailbox.

use super::*;
use crate::conn::ServerClose;
use crate::registry::{CloseRequest, RegistryMsg};
use crate::room::AfkAction;

fn afk(id: u64, action: AfkAction) -> RoomConfig {
    RoomConfig {
        afk_action: action,
        ..cfg(id, Some(5))
    }
}

/// Give the rig's room a registry mailbox of `cap` slots.
fn registry(r: &mut Rig, cap: usize) -> mpsc::Receiver<RegistryMsg> {
    let (tx, rx) = channel(cap);
    r.actor.registry = Some(tx);
    rx
}

/// The close requests the registry received so far.
fn closes(rx: &mut mpsc::Receiver<RegistryMsg>) -> Vec<CloseRequest> {
    let mut v = Vec::new();
    while let Ok(msg) = rx.try_recv() {
        if let RegistryMsg::CloseConn(req) = msg {
            v.push(req);
        }
    }
    v
}

const PARK: Detach = Detach::Hold {
    grace: Some(Duration::from_secs(600)),
    to: ExpireTo::AiHandover,
};

/// The default action closes nothing: the ceiling ends the membership
/// (the policy ran) and asks the registry for no close — the socket
/// stays open (the leave it does ask for is `leave.rs`'s subject).
#[test]
fn leave_room_asks_for_no_close() {
    let mut r = Rig::new(afk(60, AfkAction::LeaveRoom), Detach::Despawn);
    let mut reg = registry(&mut r, 64);
    let (_e, _a) = r.join(ConnectionId(1), "ana");
    for k in 1..=5u64 {
        r.step_at(k, 10 + k);
    }
    assert_eq!(r.disconnects().len(), 1, "the policy ran");
    assert!(closes(&mut reg).is_empty(), "no close was asked for");
    assert!(r.actor.close_requests.is_empty());
}

/// `Disconnect`: the policy runs first (it owns the entity), then the
/// registry is asked to close — for the membership that ended (its
/// entity), as despawned, with the idle-input verdict and a reason
/// naming the ceiling.
#[test]
fn disconnect_asks_for_the_close_after_the_policy_ran() {
    let mut r = Rig::new(afk(61, AfkAction::Disconnect), Detach::Despawn);
    let mut reg = registry(&mut r, 64);
    let (entity, _a) = r.join(ConnectionId(1), "ana");
    r.step_at(1, 3);
    assert!(closes(&mut reg).is_empty(), "below the ceiling, nothing");
    r.step_at(2, 30);
    assert_eq!(r.disconnects(), vec![(PlayerId(1), "ana".to_string())]);
    let got = closes(&mut reg);
    assert_eq!(got.len(), 1, "{got:?}");
    let req = &got[0];
    assert_eq!(
        (req.conn, req.room, req.entity, req.parked, req.cause),
        (
            ConnectionId(1),
            RoomId(61),
            entity,
            false,
            ServerClose::IdleInput
        )
    );
    assert!(
        req.reason.starts_with("input idle") && req.reason.contains("5 s"),
        "{}",
        req.reason
    );
    // Asked once: the member is gone, nothing expires it again.
    for k in 3..=10u64 {
        r.step_at(k, 30 + k);
    }
    assert!(closes(&mut reg).is_empty(), "one membership, one request");
}

/// A policy that PARKS the idle member: the close says so, so the
/// registry keeps the row for the park (a reconnect resumes it).
#[test]
fn disconnect_of_a_parked_member_says_parked() {
    let mut r = Rig::new(afk(62, AfkAction::Disconnect), PARK);
    let mut reg = registry(&mut r, 64);
    let (_e, _a) = r.join(ConnectionId(1), "ana");
    r.step_at(1, 30);
    let got = closes(&mut reg);
    assert_eq!(got.len(), 1);
    assert!(got[0].parked, "the policy parked the entity");
    let row = &r.actor.conns[&PlayerId(1)];
    assert!(row.detached);
    assert!(
        row.out.is_closed(),
        "the park keeps the row, not the socket's queue (else the socket \
         could never close)"
    );
}

/// Without a registry (a standalone room) the default parks exactly as
/// before: the row keeps its outbound half. (With one, the park is left
/// behind under the park key with both halves released — `leave.rs`.)
#[test]
fn leave_room_parks_as_before() {
    let mut r = Rig::new(afk(66, AfkAction::LeaveRoom), PARK);
    let (_e, _a) = r.join(ConnectionId(1), "ana");
    r.step_at(1, 30);
    let row = &r.actor.conns[&PlayerId(1)];
    assert!(row.detached && !row.out.is_closed());
}

/// A row parked by a transport death — and then handed to a bot — is
/// never on the input clock: `Disconnect` never closes it (it has no
/// connection left to close, and the bot's input is not input).
#[test]
fn parked_and_bot_fed_rows_are_never_closed() {
    // A zero grace: the hold (on the real tick clock) is over at the
    // first sweep, and the bot takes the row.
    let hold = Detach::Hold {
        grace: Some(Duration::ZERO),
        to: ExpireTo::AiHandover,
    };
    let mut r = Rig::new(afk(63, AfkAction::Disconnect), hold);
    let mut reg = registry(&mut r, 64);
    let (entity, _a) = r.join(ConnectionId(1), "ana");
    r.actor.handle_control(RoomControl::Detach {
        conn: ConnectionId(1),
        entity,
        identity: "ana".into(),
    });
    assert_eq!(r.disconnects().len(), 1, "the transport death parked it");
    for k in 1..=40u64 {
        r.step_at(k, 10 * k);
    }
    assert!(r.actor.conns[&PlayerId(1)].bot_fed, "the bot took it over");
    assert!(r.disconnects().is_empty(), "the ceiling never saw the row");
    assert!(closes(&mut reg).is_empty(), "nothing to close");
}

/// The full-mailbox rule: a registry that cannot take the request this
/// tick gets it on a later one — it is never dropped.
#[test]
fn a_full_registry_mailbox_gets_the_request_on_a_later_tick() {
    let mut r = Rig::new(afk(64, AfkAction::Disconnect), PARK);
    let mut reg = registry(&mut r, 1);
    r.actor
        .registry
        .as_ref()
        .expect("attached")
        .try_send(RegistryMsg::Shutdown)
        .expect("the one slot");
    let (_e, _a) = r.join(ConnectionId(1), "ana");
    r.step_at(1, 30);
    assert_eq!(r.disconnects().len(), 1, "the policy ran on time");
    assert_eq!(r.actor.close_requests.len(), 1, "the request waits");
    assert!(matches!(reg.try_recv(), Ok(RegistryMsg::Shutdown)));
    r.step_at(2, 31);
    assert_eq!(closes(&mut reg).len(), 1, "delivered once there was room");
    assert!(r.actor.close_requests.is_empty());
}

/// A standalone room (no registry — the direct-drive harnesses) has
/// nobody to ask: nothing accumulates.
#[test]
fn a_room_without_a_registry_queues_nothing() {
    let mut r = Rig::new(afk(65, AfkAction::Disconnect), Detach::Despawn);
    let (_e, _a) = r.join(ConnectionId(1), "ana");
    r.step_at(1, 30);
    assert_eq!(r.disconnects().len(), 1);
    assert!(r.actor.close_requests.is_empty());
}
