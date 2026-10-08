//! The ledger's counting rules, one test each (the module docs list
//! them).

use super::*;

const LIMIT: Duration = Duration::from_secs(6);

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

/// One request started at `at`; its id (read back from its frame).
fn send(l: &mut Ledger, at: Instant) -> u64 {
    use prost::Message;
    let f = l.start(
        &Buy {
            kind: "potion".into(),
        },
        at,
    );
    gsb_protocol::base::RpcRequest::decode(&f.payload[..])
        .expect("an RpcRequest")
        .id
}

/// An answer to `id` (an `ok` one carries an empty — default — result).
fn ans(id: u64, ok: bool, reason: &str) -> RpcResponse {
    RpcResponse {
        id,
        ok,
        op: u32::from(gsb_demo::op::ECONOMY),
        reason: reason.into(),
        payload: Vec::new(),
        game_error: false,
    }
}

/// The first answer settles its request, counted once by its kind, and
/// an `ok` answer's latency is recorded.
#[test]
fn the_first_answer_settles_its_request() {
    let t0 = Instant::now();
    let mut l = Ledger::new(LIMIT);
    let a = send(&mut l, t0);
    let b = send(&mut l, t0 + ms(10));
    assert_eq!((a, b), (1, 2), "ids count from 1");
    l.answer(ans(b, false, gsb_core::rpc::CONN_CAP_REASON), t0 + ms(40));
    l.answer(ans(a, true, ""), t0 + ms(70));
    let t = l.finish(t0 + ms(100));
    assert_eq!(t.sent, 2);
    assert_eq!((t.ok, t.conn_cap), (1, 1));
    assert_eq!(t.answered(), 2);
    assert_eq!(t.ok_lat_us, vec![70_000], "only the ok answer's latency");
    assert_eq!((t.dup_answers, t.unmatched, t.late), (0, 0, 0));
    assert_eq!((t.unanswered, t.open), (0, 0));
}

/// A second answer to a settled request is a duplicate — and nothing
/// else: the kind counts and the latencies are the first answer's.
#[test]
fn a_second_answer_is_a_duplicate_only() {
    let t0 = Instant::now();
    let mut l = Ledger::new(LIMIT);
    let a = send(&mut l, t0);
    l.answer(ans(a, true, ""), t0 + ms(30));
    l.answer(ans(a, true, ""), t0 + ms(60));
    l.answer(ans(a, false, gsb_core::rpc::TIMEOUT_REASON), t0 + ms(90));
    let t = l.finish(t0 + ms(100));
    assert_eq!(t.dup_answers, 2);
    assert_eq!((t.ok, t.timeout, t.answered()), (1, 0, 1));
    assert_eq!(t.ok_lat_us, vec![30_000]);
    assert_eq!(t.unmatched, 0);
}

/// An answer to an id never sent — 0 (the core's malformed answer) or
/// past the last one — is unmatched, and settles nothing.
#[test]
fn an_answer_to_an_unsent_id_is_unmatched() {
    let t0 = Instant::now();
    let mut l = Ledger::new(LIMIT);
    let a = send(&mut l, t0);
    l.answer(ans(0, false, gsb_core::rpc::MALFORMED_REASON), t0 + ms(5));
    l.answer(ans(a + 1, true, ""), t0 + ms(5));
    l.answer(ans(u64::MAX, true, ""), t0 + ms(5));
    let t = l.finish(t0 + ms(10));
    assert_eq!(t.unmatched, 3);
    assert_eq!((t.answered(), t.malformed, t.dup_answers), (0, 0, 0));
    assert_eq!(t.open, 1, "the request itself is still waiting");
}

/// A request reaches its limit on the table's clock: unanswered when
/// no answer ever comes, late when one comes after (counted under no
/// kind since B161 — the table already ended it); one in flight at the
/// end is open. Client-side timeouts are the unanswered plus the late.
#[test]
fn the_limit_splits_unanswered_open_and_late() {
    let t0 = Instant::now();
    let mut l = Ledger::new(LIMIT);
    let _old = send(&mut l, t0);
    let slow = send(&mut l, t0);
    let _young = send(&mut l, t0 + ms(9_000));
    let on_time = send(&mut l, t0 + ms(1_000));
    l.answer(ans(slow, true, ""), t0 + LIMIT + ms(1));
    l.answer(ans(on_time, true, ""), t0 + ms(999) + LIMIT);
    let t = l.finish(t0 + ms(10_000));
    assert_eq!((t.unanswered, t.open, t.late), (1, 1, 1));
    assert_eq!(t.client_timeouts(), 2);
    assert_eq!(t.ok, 1, "a late answer settles nothing");
    assert_eq!(t.answered() + t.late + t.unanswered + t.open, t.sent);
}

/// An `ok` answer whose payload is not a `BuyResult` is undecodable,
/// under no kind and with no latency.
#[test]
fn an_answer_that_does_not_decode_is_counted_apart() {
    let t0 = Instant::now();
    let mut l = Ledger::new(LIMIT);
    let a = send(&mut l, t0);
    let mut bad = ans(a, true, "");
    bad.payload = vec![0xFF, 0xFF, 0xFF];
    l.answer(bad, t0 + ms(5));
    let t = l.finish(t0 + ms(10));
    assert_eq!((t.undecodable, t.ok, t.answered()), (1, 0, 0));
    assert!(t.ok_lat_us.is_empty());
    assert_eq!((t.open, t.unanswered, t.dup_answers), (0, 0, 0));
}

/// Every core rejection is told apart by its reason constant, anything
/// else is the game's own rejection, and `ok` wins over a reason.
#[test]
fn the_core_reasons_classify() {
    use gsb_core::rpc as core;
    let cases = [
        (core::TIMEOUT_REASON.to_string(), Kind::Timeout),
        (core::CONN_CAP_REASON.to_string(), Kind::ConnCap),
        (core::ROOM_CAP_REASON.to_string(), Kind::RoomCap),
        (core::DUPLICATE_REASON.to_string(), Kind::Dup),
        (core::MALFORMED_REASON.to_string(), Kind::Malformed),
        (core::no_handler_reason(0x3ee), Kind::NoHandler),
        ("unknown item `x`".to_string(), Kind::Logic),
        ("economy service gone".to_string(), Kind::Logic),
    ];
    for (reason, kind) in cases {
        assert_eq!(classify(false, &reason), kind, "{reason}");
        assert_eq!(classify(true, &reason), Kind::Ok, "{reason}");
    }
}

/// Summing tallies adds every field and keeps every latency.
#[test]
fn tallies_add_field_by_field() {
    let t0 = Instant::now();
    let mut l = Ledger::new(LIMIT);
    let a = send(&mut l, t0);
    let _ = send(&mut l, t0);
    l.answer(ans(a, true, ""), t0 + ms(2));
    l.answer(ans(a, true, ""), t0 + ms(3));
    l.answer(ans(9, true, ""), t0 + ms(3));
    let one = l.finish(t0 + LIMIT + ms(1));
    let mut sum = RpcTally::default();
    sum.add(&one);
    sum.add(&one);
    assert_eq!(sum.sent, 4);
    assert_eq!((sum.ok, sum.dup_answers, sum.unmatched), (2, 2, 2));
    assert_eq!(sum.unanswered, 2);
    assert_eq!(sum.ok_lat_us, vec![2_000, 2_000]);
}
