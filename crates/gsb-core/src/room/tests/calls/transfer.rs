//! Sending gold to a friend (SERVICES.md §6.1, spike `transfer`): a
//! synchronous reserve, one bank call that `.answers` the client, a
//! double click answered BUSY in the same tick, a re-sent id answered
//! as a duplicate — and ONE charge at the bank.

use super::*;

/// The bank answers 3 drains after the call. Step 2: two clicks (ids 10,
/// 11) — the first reserves and calls, the second is BUSY at once. Step
/// 3: id 10 again — the engine's duplicate answer (the game never sees
/// it). Steps 2–4: the wallet is (500, reserve 100). Step 5's RESPONSES:
/// the client gets balance 400, the system releases the reserve: (400,
/// 0). One charge; the bank saw the sender's verified identity.
#[test]
fn a_transfer_reserves_charges_once_and_answers_the_client() {
    let mut rig = Rig::new(RigCfg {
        bank_delay: 3,
        ..Default::default()
    });
    let mut alice = rig.join(1, "alice");
    rig.step();
    let _ = rig.seen();

    alice.request(10, 1001, &transfer("bob", 100));
    alice.request(11, 1001, &transfer("bob", 100));
    rig.step();
    let got = alice.answers();
    assert_eq!(got.len(), 1, "only the double click is answered now");
    assert_eq!(got[0].id, 11);
    assert_eq!(game_error(&got[0]), BUSY);
    assert_eq!(
        rig.wallet(1),
        Some(Wallet {
            gold: 500,
            reserved: 100
        })
    );

    alice.request(10, 1001, &transfer("bob", 100));
    rig.step();
    let got = alice.answers();
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].reason, crate::rpc::DUPLICATE_REASON);
    assert_eq!(rig.room.m.requests_rejected_dup, 1);
    let seen = rig.seen();
    assert!(
        !seen.iter().any(|s| matches!(s, Seen::Request(..))),
        "the game never saw the duplicate: {seen:?}"
    );

    rig.step();
    assert!(
        alice.answers().is_empty(),
        "the bank answered after this step"
    );
    assert_eq!(
        rig.wallet(1),
        Some(Wallet {
            gold: 500,
            reserved: 100
        })
    );

    rig.step();
    let got = alice.answers();
    assert_eq!(got.len(), 1);
    assert_eq!((got[0].id, got[0].op), (10, 1001));
    assert_eq!(balance(&got[0]), 400);
    assert_eq!(
        rig.wallet(1),
        Some(Wallet {
            gold: 400,
            reserved: 0
        })
    );

    assert_eq!(rig.bank.charges, 1, "one charge");
    assert_eq!(rig.bank.seen.len(), 1);
    assert_eq!(rig.bank.seen[0].1.as_deref(), Some("alice"), "cx.player()");
    let c = rig.counts();
    assert_eq!(c.calls_issued, 1);
    assert_eq!(c.calls_sent, 1);
    assert_eq!(c.results_ok, 1);
    assert_eq!(c.responses_delivered, 1);
    assert_eq!(c.answers_sent, 1);
    assert_eq!(c.requests_answered, 1, "the BUSY reply");
    assert_eq!(c.requests_untaken + c.requests_unanswered, 0);
}

/// The result never shows in the tick that issued the call, even when
/// the service answers at once: issued at step 1's FLUSH, answered by the
/// drain right after it, resolved at step 2's RESPONSES.
#[test]
fn a_result_is_never_seen_in_the_tick_that_issued_the_call() {
    let mut rig = Rig::new(RigCfg::default());
    let mut alice = rig.join(1, "alice");
    alice.request(1, 1001, &transfer("bob", 50));
    rig.step();
    assert!(alice.answers().is_empty());
    assert!(!rig.seen().iter().any(|s| matches!(s, Seen::Resolved(..))));
    rig.step();
    assert_eq!(balance(&alice.answers()[0]), 450);
    assert!(
        rig.seen().contains(&Seen::Resolved(Owner(1), 50, Ok(450))),
        "resolved with its owner and reserve"
    );
}
