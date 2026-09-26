//! A dropped one-shot full on the REAL actors (F11): a member joins an
//! established team while its out channel is full, so the batch with its
//! one-shot full is dropped. Keep-alive is off in this rig, so before
//! the drop signal nothing would ever heal it (the client drops every
//! delta: no baseline); now the next tick re-sends the full.

use std::collections::BTreeSet;

use super::*;

/// What the stalled joiner's view holds one tick after the drop, and its
/// client counters then.
async fn stalled_joiner_converges(mut rig: Rig) {
    let first = rig.join(1, "0:-50:-50").await;
    let second = rig.join(2, "0:-60:-50").await;
    rig.steps(2).await;
    let late = rig.join_stalled(3, "0:-40:-50").await;
    let dropped = rig.dropped();
    assert!(dropped >= 1, "the one-shot full was dropped");

    rig.step().await;
    let wires: Vec<u64> = [first, second, late]
        .iter()
        .map(|&c| rig.clients[c].wire)
        .collect();
    let c = &rig.clients[late];
    assert_eq!(
        c.view.ids().collect::<BTreeSet<_>>(),
        wires.iter().copied().collect(),
        "the next tick re-sent the full: the whole team in view"
    );
    assert_eq!(
        c.view.counters().gap_drops,
        0,
        "no delta without a baseline"
    );
    assert_eq!(rig.dropped(), dropped, "nothing else was dropped");

    // The allies' views are untouched by the drop: they see the late
    // joiner too.
    rig.step().await;
    for &i in &[first, second] {
        assert!(rig.clients[i].sees(wires[2]), "client {i}");
    }
}

#[tokio::test(start_paused = true)]
async fn a_dropped_one_shot_full_is_resent_on_the_shard() {
    stalled_joiner_converges(Rig::new(true).await).await;
}

#[tokio::test(start_paused = true)]
async fn a_dropped_one_shot_full_is_resent_on_the_room() {
    stalled_joiner_converges(Rig::single(true).await).await;
}
