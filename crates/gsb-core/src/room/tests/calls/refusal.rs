//! Refusals (SERVICES.md §5.3, spike `refusal`): a full mailbox, a gone
//! service, a used-up share — decided at FLUSH, delivered at the NEXT
//! tick's RESPONSES through `take_resolved` like every other ending;
//! an `.answers` client gets the engine's reason.

use super::*;

fn two_players_two_transfers(rig: &mut Rig) -> (Joined, Joined) {
    let a = rig.join(1, "alice");
    let b = rig.join(2, "bea");
    a.request(1, 1001, &transfer("x", 10));
    b.request(1, 1001, &transfer("x", 10));
    (a, b)
}

/// Mailbox of one: the second call of the tick is refused `Full`. Not in
/// the issuing tick: at the next RESPONSES, with its reserve, and the
/// client hears "service busy".
#[test]
fn a_full_mailbox_refuses_at_flush_and_resolves_next_tick() {
    let mut rig = Rig::new(RigCfg {
        bank_capacity: 1,
        bank_delay: 5,
        ..Default::default()
    });
    let (mut a, mut b) = two_players_two_transfers(&mut rig);
    rig.step();
    assert_eq!(rig.counts().calls_refused_full, 1);
    assert!(
        !rig.seen().iter().any(|s| matches!(s, Seen::Resolved(..))),
        "never in the issuing tick"
    );
    assert!(a.answers().is_empty() && b.answers().is_empty());
    rig.step();
    let seen = rig.seen();
    assert!(seen.contains(&Seen::Resolved(
        Owner(2),
        10,
        Err(CallError::Refused(crate::calls::Refusal::Full))
    )));
    assert!(
        seen.contains(&Seen::Wallet(
            PlayerId(2),
            Wallet {
                gold: 500,
                reserved: 0
            }
        )),
        "the reserve came back on the same path as any ending"
    );
    let got = b.answers();
    assert_eq!(got.len(), 1);
    assert!(!got[0].ok && !got[0].game_error, "an engine answer");
    assert_eq!(got[0].reason, crate::rpc::REFUSED_FULL_REASON);
    assert!(a.answers().is_empty(), "the first call is still in flight");
    let c = rig.counts();
    assert_eq!((c.calls_sent, c.results_refused), (1, 1));
}

/// A gone service (its mailbox closed): `Gone`, next tick.
#[test]
fn a_gone_service_refuses_gone() {
    let mut rig = Rig::new(RigCfg::default());
    let mut a = rig.join(1, "alice");
    rig.bank.inbox.close();
    a.request(1, 1001, &transfer("x", 10));
    rig.step();
    assert_eq!(rig.counts().calls_refused_gone, 1);
    rig.step();
    assert_eq!(a.answers()[0].reason, crate::rpc::REFUSED_GONE_REASON);
    assert!(rig.seen().contains(&Seen::Resolved(
        Owner(1),
        10,
        Err(CallError::Refused(crate::calls::Refusal::Gone))
    )));
}

/// Share one: one call in flight to the bank per room — the second is
/// refused `Share` (the bank never sees it), next tick.
#[test]
fn a_used_up_share_refuses_share() {
    let mut rig = Rig::new(RigCfg {
        share: 1,
        bank_delay: 5,
        ..Default::default()
    });
    let (mut a, mut b) = two_players_two_transfers(&mut rig);
    rig.step();
    assert_eq!(rig.counts().calls_refused_share, 1);
    assert_eq!(rig.bank.seen.len(), 1, "the refused call never left");
    rig.step();
    assert_eq!(b.answers()[0].reason, crate::rpc::REFUSED_SHARE_REASON);
    assert!(a.answers().is_empty());
}

/// A service that drops the call unanswered (a panic, a stop): the call
/// ends `Abandoned` — counted, delivered, the client told.
#[test]
fn a_dropped_call_ends_abandoned() {
    let mut rig = Rig::new(RigCfg::default());
    let mut a = rig.join(1, "alice");
    a.request(1, 1001, &transfer("x", 10));
    rig.step_room_only();
    // The bank takes the call and drops it.
    let Ok(Mail::Call(env)) = rig.bank.inbox.try_recv() else {
        panic!("the call was sent");
    };
    drop(env);
    rig.step();
    assert_eq!(a.answers()[0].reason, crate::rpc::ABANDONED_REASON);
    assert_eq!(rig.counts().results_abandoned, 1);
    assert!(
        rig.seen()
            .contains(&Seen::Resolved(Owner(1), 10, Err(CallError::Abandoned)))
    );
}
