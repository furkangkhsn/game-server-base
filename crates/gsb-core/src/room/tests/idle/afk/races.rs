//! The close request's race with the park it names (BACKLOG B41): a
//! request that waits behind a full registry mailbox is re-checked when
//! it leaves, so it never tells the registry to keep a row for a park
//! that already ended — the despawn report of that park goes out in
//! phase 0c, AHEAD of the request's phase 0d, and the registry drops a
//! report for a row it does not yet know is detached.

use super::*;

/// A hold that is over at the first sweep (the sweep reads the real tick
/// clock), toward `to`.
fn zero(to: ExpireTo) -> Detach {
    Detach::Hold {
        grace: Some(Duration::ZERO),
        to,
    }
}

/// A room with the ceiling under `Disconnect`, a policy that parks with
/// `hold`, and a one-slot registry mailbox that is full: member 1's close
/// request is refused at the kick (step 1) and waits.
fn kicked_behind_a_full_mailbox(id: u64, hold: Detach) -> (Rig, mpsc::Receiver<RegistryMsg>) {
    let mut r = Rig::new(afk(id, AfkAction::Disconnect), hold);
    let mut reg = registry(&mut r, 1);
    let tx = r.actor.registry.clone().expect("attached");
    tx.try_send(RegistryMsg::Shutdown).expect("the one slot");
    let (_e, _a) = r.join(ConnectionId(1), "ana");
    r.step_at(1, 30);
    assert_eq!(r.disconnects().len(), 1, "the policy ran on time");
    assert!(r.actor.close_requests[0].parked, "queued as a park");
    assert!(matches!(reg.try_recv(), Ok(RegistryMsg::Shutdown)));
    (r, reg)
}

/// What the registry received over steps `from..=to`, one step at a
/// time (drained between steps — the one slot takes one message a step).
fn order(r: &mut Rig, reg: &mut mpsc::Receiver<RegistryMsg>, from: u64, to: u64) -> Vec<String> {
    let mut kinds = Vec::new();
    for k in from..=to {
        r.step_at(k, 30 + k);
        while let Ok(msg) = reg.try_recv() {
            kinds.push(match msg {
                RegistryMsg::DetachDespawned { conn, .. } => format!("report {}", conn.0),
                RegistryMsg::CloseConn(req) => format!("close parked={}", req.parked),
                other => format!("{other:?}"),
            });
        }
    }
    kinds
}

/// The park ENDS (despawn) while its close request waits: the hold's
/// report goes first, and the request then closes a despawn — a
/// `parked` close arriving after the report would mark the row detached
/// with nothing left to release it (its slot held for the room's life).
#[test]
fn a_park_that_ended_first_closes_as_a_despawn() {
    let (mut r, mut reg) = kicked_behind_a_full_mailbox(67, zero(ExpireTo::Despawn));
    let kinds = order(&mut r, &mut reg, 2, 6);
    assert!(r.actor.conns.is_empty(), "the zero hold ended in a despawn");
    assert_eq!(kinds, vec!["report 1", "close parked=false"]);
    assert!(r.actor.close_requests.is_empty());
}

/// The park is handed to a bot while its close request waits: the room
/// still holds the membership (the bot plays it, the slot stays taken),
/// so the request still says parked — only an ENDED park flips it.
#[test]
fn a_park_handed_to_a_bot_still_closes_as_parked() {
    let (mut r, mut reg) = kicked_behind_a_full_mailbox(68, zero(ExpireTo::AiHandover));
    let kinds = order(&mut r, &mut reg, 2, 6);
    assert!(r.actor.conns[&PlayerId(1)].bot_fed, "the bot took it over");
    assert_eq!(kinds, vec!["close parked=true"]);
}
