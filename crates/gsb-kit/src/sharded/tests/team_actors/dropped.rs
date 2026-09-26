//! A dropped one-shot full on the REAL actors (F11): a member joins an
//! established team while its out channel is full, so the batch with its
//! one-shot full is dropped. Keep-alive is off in this rig, so before
//! the drop signal nothing would ever heal it (the client drops every
//! delta: no baseline); now the next tick re-sends the full — and after
//! a long stall, the tick after the first batch that gets through.

use std::collections::BTreeSet;

use super::*;

/// What the stalled joiner's view holds one tick after the drop, and its
/// client counters then.
async fn stalled_joiner_converges(mut rig: Rig) {
    let first = rig.join(1, "0:-50:-50").await;
    let second = rig.join(2, "0:-60:-50").await;
    rig.steps(2).await;
    let late = rig.join_stalled(3, "0:-40:-50").await;
    let late = rig.release(late);
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

/// The same joiner, stalled for 40 ticks while an ally walks (every
/// batch dropped): its re-sends are paced (1, 2, 4, 8, 16, 32 ticks
/// apart — the next slot would be ~20 ticks after the stall ends), but
/// the first batch that gets through releases the wait: the view is
/// whole, the ally where it walked to, two ticks after the stall (the
/// ally still walking — a silent group ships nothing to get through,
/// and misses nothing while it waits).
async fn long_stalled_joiner_converges_once_it_reads(mut rig: Rig) {
    let first = rig.join(1, "0:-50:-50").await;
    let late = rig.join_stalled(3, "0:-40:-50").await;
    let before = rig.dropped();
    for i in 0..40 {
        rig.clients[first].move_to(-50.0 + i as f32, -50.0);
        rig.step().await;
    }
    let dropped = rig.dropped();
    assert!(
        dropped >= before + 40,
        "every batch of the stall: {dropped}"
    );
    let late = rig.release(late);
    // The ally keeps walking: the first batch through (a delta — the
    // re-send still waits) reports the resume; the next carries the full.
    for i in 40..42 {
        rig.clients[first].move_to(-50.0 + i as f32, -50.0);
        rig.step().await;
    }
    let (a, b) = (rig.clients[first].wire, rig.clients[late].wire);
    let c = &rig.clients[late];
    assert_eq!(
        c.view.ids().collect::<BTreeSet<_>>(),
        [a, b].into_iter().collect()
    );
    assert_eq!(c.view.get(a), Some(&(-9, -50)), "the ally's last spot");
    assert_eq!(rig.dropped(), dropped, "nothing dropped since");
}

#[tokio::test(start_paused = true)]
async fn a_long_stall_heals_the_tick_after_it_ends_on_the_shard() {
    long_stalled_joiner_converges_once_it_reads(Rig::new(true).await).await;
}

#[tokio::test(start_paused = true)]
async fn a_long_stall_heals_the_tick_after_it_ends_on_the_room() {
    long_stalled_joiner_converges_once_it_reads(Rig::single(true).await).await;
}
