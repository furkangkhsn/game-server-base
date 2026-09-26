//! The ranked cut against a reference that sorts: over seeded sets with
//! many equal ranks, every budget and every members/rest split keeps
//! exactly the records a full sort per tier would.

use super::*;

/// The fixtures' wire value: the rank itself.
fn rank(value: &u32) -> u32 {
    *value
}

/// The reference: per tier, sort by [`best_first`] and keep the first
/// ones the budget leaves room for.
fn reference(set: &[(u64, u32)], members: usize, records: usize) -> Vec<bool> {
    let mut kept = vec![false; set.len()];
    let tiers = [
        (0..members, records),
        (members..set.len(), records.saturating_sub(members)),
    ];
    for (range, room) in tiers {
        let mut order: Vec<usize> = range.collect();
        order.sort_by(|&a, &b| best_first(&(set[a].1, set[a].0), &(set[b].1, set[b].0)));
        for &i in order.iter().take(room) {
            kept[i] = true;
        }
    }
    kept
}

#[test]
fn a_ranked_cut_keeps_what_a_full_sort_would() {
    let mut seed: u64 = 0xA29_0003;
    let mut next = move || {
        seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
        seed >> 33
    };
    let mut budget = Budget::<u32>::new(0);
    budget.rank = Some(rank);
    for n in [1usize, 2, 7, 40] {
        // Distinct wire ids in no particular order, ranks 0..4.
        let set: Vec<(u64, u32)> = (0..n)
            .map(|i| ((next() % 1_000) * 64 + i as u64, (next() % 4) as u32))
            .collect();
        for members in 0..=n {
            for records in 0..=n + 1 {
                budget.records = records;
                let kept = budget.cut(set.iter().map(|(w, v)| (*w, v)), members);
                let got: Vec<bool> = (0..n).map(|i| budget.keeps(kept, i)).collect();
                let want = reference(&set, members, records);
                assert_eq!(got, want, "n {n} members {members} records {records}");
                assert_eq!(got.iter().filter(|&&k| k).count(), n.min(records));
            }
        }
    }
}

/// Without a rank: the prefix, whatever the values.
#[test]
fn without_a_rank_the_cut_is_the_prefix() {
    let set = [(9, 3u32), (4, 0), (7, 2)];
    let mut budget = Budget::<u32>::new(2);
    let kept = budget.cut(set.iter().map(|(w, v)| (*w, v)), 1);
    assert_eq!(kept, Kept::Prefix(2));
    assert_eq!(
        (0..3).map(|i| budget.keeps(kept, i)).collect::<Vec<_>>(),
        [true, true, false]
    );
}
