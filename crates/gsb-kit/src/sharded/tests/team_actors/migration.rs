//! A member walking across a seam, watched by allies on the shard it
//! leaves, the shard it enters and a far shard: never listed twice, never
//! missing for more than one tick; an enemy far away never sees it.

use super::*;

/// The longest run of consecutive barriers (from `from` on) whose view
/// lacked `wire`.
fn longest_gap(history: &[(u64, std::collections::BTreeSet<u64>)], wire: u64) -> usize {
    let (mut run, mut worst) = (0, 0);
    for (_, ids) in history {
        if ids.contains(&wire) {
            run = 0;
        } else {
            run += 1;
            worst = worst.max(run);
        }
    }
    worst
}

#[tokio::test(start_paused = true)]
async fn a_member_crossing_a_seam_is_never_doubled_and_blinks_at_most_one_tick() {
    let mut rig = Rig::new(true).await;
    let walker = rig.join(1, "0:-7:-50").await;
    let left = rig.join(2, "0:-50:-80").await; // shard 0, stays
    let right = rig.join(3, "0:50:-80").await; // shard 1
    let far = rig.join(4, "0:50:50").await; // shard 3
    let enemy = rig.join(5, "1:-50:50").await; // shard 2
    rig.steps(3).await;
    assert_eq!(rig.members, [2, 1, 1, 1]);
    let m = rig.clients[walker].wire;
    let from: Vec<usize> = rig.clients.iter().map(|c| c.history.len()).collect();
    for c in [left, right, far] {
        assert!(rig.clients[c].sees(m), "client {c} sees the walker");
    }

    // Two units a tick eastward: x = -5, -3, …, 9 — over the x = 0 seam.
    for i in 0..8 {
        rig.clients[walker].move_to(-5.0 + 2.0 * i as f32, -50.0);
        rig.step().await;
    }
    rig.steps(3).await;
    assert_eq!(rig.members, [1, 2, 1, 1], "the walker is shard 1's");

    for c in [left, right, far] {
        let history = &rig.clients[c].history[from[c]..];
        assert!(
            longest_gap(history, m) <= 1,
            "client {c} lost the walker for more than a tick: {history:?}"
        );
        assert_eq!(rig.clients[c].view.get(m), Some(&(9, -50)), "client {c}");
    }
    assert!(
        rig.clients.iter().all(|c| c.doubled == 0),
        "a frame listed a wire twice"
    );
    assert!(
        rig.clients[enemy]
            .history
            .iter()
            .all(|(_, ids)| !ids.contains(&m)),
        "the enemy never saw the walker"
    );
}
