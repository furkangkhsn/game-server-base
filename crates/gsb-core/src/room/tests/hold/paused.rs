//! The hold's clock is the tick clock (BACKLOG F16): under tokio's
//! PAUSED clock a grace and a ceiling run out when the paused time
//! passes them — not in real time. Before F16 the sweep read
//! `std::time::Instant`, which the paused clock does not move: a
//! paused-clock test could never see a hold end.

use super::*;

#[tokio::test(start_paused = true)]
async fn a_grace_runs_out_on_the_paused_clock() {
    let mut r = Rig::new(timed(ExpireTo::Despawn), Some(CEILING));
    let p = r.join_and_detach(1);
    tokio::time::advance(GRACE - SEC).await;
    r.step();
    assert!(r.held(p), "one second of grace left");
    tokio::time::advance(SEC).await;
    r.step();
    assert!(
        !r.actor.conns.contains_key(&p),
        "the grace ran out on the paused clock"
    );
    assert_eq!(r.ended(), vec![(p, ExpireTo::Despawn)]);
}

#[tokio::test(start_paused = true)]
async fn a_ceiling_overrides_a_veto_on_the_paused_clock() {
    let mut r = Rig::new(timed(ExpireTo::Despawn), Some(CEILING));
    let p = r.join_and_detach(1);
    r.veto.set(true);
    tokio::time::advance(CEILING - SEC).await;
    r.step();
    assert!(r.held(p), "vetoed, short of the ceiling");
    tokio::time::advance(SEC).await;
    r.step();
    assert!(!r.held(p), "forced at the ceiling, on the paused clock");
    assert_eq!(r.counts(), (1, 0, 1, 1));
}
