//! The storm bound on the shard (F14; the room's rule): a congested
//! connection is let owe no more than its in-flight cap — undelivered
//! and in-flight together — and a connection whose batches go through
//! is never refused.

use super::*;

/// Cap 2, pull 4: one undelivered answer plus one in-flight request
/// reach the cap, so every later request — well-formed or not — is
/// refused without an answer until the owed answer is delivered.
#[tokio::test]
async fn a_congested_connection_owes_at_most_its_cap_on_the_shard() {
    let mut a = shard(2, 4);
    let (_wire, tx, mut rx) = join(&mut a, 1, 1);
    request(&tx, 1, 1, OP_LOCAL);
    assert!(a.step(&tinfo(1)));
    request(&tx, 1, 2, OP_EXT);
    request(&tx, 1, 3, OP_LOCAL);
    assert!(a.step(&tinfo(2))); // [3] dropped; 2 in flight
    for t in 3..=6 {
        request(&tx, 1, t * 10, OP_LOCAL);
        request(&tx, 1, 0, OP_LOCAL); // malformed: id 0
        assert!(a.step(&tinfo(t)));
        let c = ConnectionId(1);
        assert_eq!(a.queued.get(&c).map(Vec::len), Some(1), "step {t}");
        assert_eq!(a.pending.get(&c).map(|d| d.len()), Some(1), "step {t}");
    }
    assert_eq!(a.m.requests_refused_congested, 4 * 2);
    assert_eq!(
        a.sample().requests_refused_congested,
        4 * 2,
        "and the sample carries it (F15)"
    );
    assert_eq!(a.m.requests_rejected_malformed, 0, "refused, not answered");
    assert_eq!(
        a.m.requests_rejected_conn_cap, 0,
        "a refusal is not an answered cap rejection (F15)"
    );
    assert_eq!(drain(&mut rx), [vec![1]]);
    assert!(a.step(&tinfo(7)));
    assert_eq!(drain(&mut rx), [vec![3]]);
}

/// Nothing changes while batches go through: a connection at its cap
/// (one request in flight, cap 1) still has its local requests answered.
#[tokio::test]
async fn an_uncongested_connection_is_never_refused_on_the_shard() {
    let mut a = shard(1, 4);
    let (_wire, tx, mut rx) = join(&mut a, 1, 64);
    request(&tx, 1, 1, OP_EXT);
    request(&tx, 1, 2, OP_LOCAL);
    request(&tx, 1, 3, OP_LOCAL);
    assert!(a.step(&tinfo(1)));
    assert_eq!(drain(&mut rx), [vec![2, 3]]);
    assert_eq!(a.m.requests_rejected_conn_cap, 0);
    assert_eq!(a.m.requests_refused_congested, 0);
}
