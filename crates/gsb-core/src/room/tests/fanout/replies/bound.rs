//! The storm bound (F14): a congested connection — its latest batch
//! was dropped — accepts a request only while it owes fewer answers,
//! queued, carried and in flight together, than its in-flight cap;
//! past that, requests are refused unprocessed and unanswered.

use super::*;

/// Send a request-op action whose envelope does not decode.
fn malformed(tx: &Mailbox<Action>, conn: u64) {
    send(tx, conn, vec![0xFF, 0xFF, 0xFF]);
}

/// What `conn` owes: queued (undelivered included) + in flight.
fn owed(actor: &RoomActor<(), (), ()>, conn: u64) -> usize {
    let c = ConnectionId(conn);
    actor.queued.get(&c).map_or(0, Vec::len) + actor.pending.get(&c).map_or(0, |d| d.len())
}

/// Cap 2, pull 4. Once a batch carrying an answer dropped, the
/// connection accepts a request only while it owes fewer than 2
/// answers: step 3 takes one more (owes 2) and refuses the rest, every
/// later step refuses all — well-formed or not — without answering, and
/// the two owed answers arrive once the channel drains. Delivery ends
/// the congestion: the next request is answered as usual.
#[test]
fn a_congested_connection_owes_at_most_its_in_flight_cap() {
    let (mut a, _control) = room(2, 4);
    let (tx, mut rx) = join(&mut a, 1, 1);
    request(&tx, 1, 1, OP_LOCAL);
    step(&mut a, 1);
    request(&tx, 1, 2, OP_LOCAL);
    step(&mut a, 2);
    for id in 3..=6 {
        request(&tx, 1, id, OP_LOCAL);
    }
    step(&mut a, 3);
    assert_eq!(owed(&a, 1), 2, "3 accepted, 4..=6 refused");
    for tick in 4..=10 {
        for n in 0..3 {
            request(&tx, 1, tick * 10 + n, OP_LOCAL);
        }
        malformed(&tx, 1);
        step(&mut a, tick);
        assert_eq!(owed(&a, 1), 2, "step {tick}: nothing more accepted");
    }
    assert_eq!(a.m.requests_refused_congested, 3 + 7 * 4);
    assert_eq!(
        a.sample().requests_refused_congested,
        3 + 7 * 4,
        "and the sample carries it (F15)"
    );
    assert_eq!(a.m.requests_rejected_malformed, 0, "refused, not answered");
    assert_eq!(
        a.m.requests_rejected_conn_cap, 0,
        "a refusal is not an answered cap rejection (F15)"
    );
    assert_eq!(drain(&mut rx), [vec![1]]);
    step(&mut a, 11);
    assert_eq!(drain(&mut rx), [vec![2, 3]]);
    request(&tx, 1, 100, OP_LOCAL);
    step(&mut a, 12);
    assert_eq!(drain(&mut rx), [vec![100]], "flowing again");
}

/// The worst case: 2 requests in flight when a step's full pull (4)
/// is answered into a batch that drops — the connection owes 2 + 4 and
/// not one more while it stays congested.
#[tokio::test]
async fn the_storm_bound_is_the_cap_plus_one_ticks_pull() {
    let (mut a, _control) = room(2, 4);
    let (tx, mut rx) = join(&mut a, 1, 1);
    request(&tx, 1, 1, OP_EXT);
    request(&tx, 1, 2, OP_EXT);
    request(&tx, 1, 3, OP_LOCAL);
    step(&mut a, 1);
    for id in 4..=7 {
        request(&tx, 1, id, OP_LOCAL);
    }
    step(&mut a, 2);
    assert_eq!(owed(&a, 1), 2 + 4);
    for tick in 3..=8 {
        request(&tx, 1, tick * 10, OP_EXT);
        request(&tx, 1, tick * 10 + 1, OP_LOCAL);
        step(&mut a, tick);
        assert_eq!(owed(&a, 1), 2 + 4, "step {tick}");
    }
    assert_eq!(drain(&mut rx), [vec![3]]);
    step(&mut a, 9);
    assert_eq!(drain(&mut rx), [vec![4, 5, 6, 7]]);
}

/// In-flight requests count toward what a congested connection owes:
/// one undelivered answer plus one pending request reach the cap of 2.
#[tokio::test]
async fn in_flight_requests_count_toward_what_is_owed() {
    let (mut a, _control) = room(2, 4);
    let (tx, _rx) = join(&mut a, 1, 1);
    request(&tx, 1, 1, OP_LOCAL);
    step(&mut a, 1);
    request(&tx, 1, 2, OP_EXT);
    request(&tx, 1, 3, OP_LOCAL);
    step(&mut a, 2); // [3] dropped; 2 in flight
    request(&tx, 1, 4, OP_LOCAL);
    step(&mut a, 3);
    assert_eq!(a.m.requests_refused_congested, 1, "4 refused");
    assert_eq!(a.m.requests_rejected_conn_cap, 0);
    assert_eq!(a.queued.get(&ConnectionId(1)).map(Vec::len), Some(1));
}
