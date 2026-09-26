//! The shard's hold on the tick clock (BACKLOG F16; the room's
//! `hold::paused`): under tokio's PAUSED clock the grace runs out when
//! the paused time passes it.

use super::*;

#[tokio::test(start_paused = true)]
async fn a_grace_runs_out_on_the_paused_clock_on_the_shard() {
    let knobs = Knobs::default();
    let mut a = lone(&knobs, timed(ExpireTo::Despawn), Some(CEILING));
    let p = join_and_detach(&mut a, 1);
    tokio::time::advance(GRACE - SEC).await;
    step(&mut a, 1);
    assert!(held(&a, p), "one second of grace left");
    tokio::time::advance(SEC).await;
    step(&mut a, 2);
    assert!(
        !a.conns.contains_key(&p),
        "the grace ran out on the paused clock"
    );
    assert_eq!(counts(&a), (1, 0, 0, 0));
}
