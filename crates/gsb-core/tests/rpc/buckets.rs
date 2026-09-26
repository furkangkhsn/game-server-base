//! Reject-bucket wiring: which reject lands in which counter.
//!
//! The six `requests_rejected_*` counters are an operational signal --
//! each answers a distinct question (is the room cap actually binding?),
//! so a reject landing in the WRONG bucket is a silent misdiagnosis: the
//! counter that "confidently" says "cap not binding" is the one an
//! operator reads when sizing. The smoke tests only check that all six
//! are zero in the happy path, which catches a wire-queue shift but not
//! a crossed wiring. These tests pin the wiring: each one triggers
//! exactly ONE terminal reject decision in a fresh room and asserts that
//! the matching counter increments while the other five stay at zero
//! (the second half is the load-bearing part -- a counter that bumps all
//! six, or the wrong one, fails here).

use super::*;

/// The six RPC reject buckets of one room sample (cumulative counters).
#[derive(Debug, Clone, Copy)]
struct Rejects {
    malformed: u64,
    dup: u64,
    no_handler: u64,
    logic: u64,
    conn_cap: u64,
    room_cap: u64,
    /// The congested connections' unanswered refusals (F15): not a
    /// reject bucket, but a request refused there must never be counted
    /// in one — and an answered reject never there — so it takes part
    /// in the "only the triggered one moved" sum.
    refused: u64,
}

impl Rejects {
    fn of(s: &RoomSample) -> Self {
        Self {
            malformed: s.requests_rejected_malformed,
            dup: s.requests_rejected_dup,
            no_handler: s.requests_rejected_no_handler,
            logic: s.requests_rejected_logic,
            conn_cap: s.requests_rejected_conn_cap,
            room_cap: s.requests_rejected_room_cap,
            refused: s.requests_refused_congested,
        }
    }

    /// Assert that exactly the triggered bucket moved: `moved` grew by
    /// `n` and the other five — and the refusal count — are still zero
    /// (fresh harness, cumulative counters — the test triggered one
    /// reject path and nothing else touches these seven).
    fn assert_only(&self, name: &str, n: u64, moved: u64) {
        assert_eq!(
            moved, n,
            "{name}: the triggered bucket must increment by {n}"
        );
        let total = self.malformed
            + self.dup
            + self.no_handler
            + self.logic
            + self.conn_cap
            + self.room_cap
            + self.refused;
        assert_eq!(
            total, n,
            "{name}: only the triggered bucket may move (a reject counted in \
             another bucket is a wiring bug); got {self:?}"
        );
    }
}

/// Bucket `malformed`: an undecodable envelope payload (a client bug) and
/// a decodable one with `id = 0` (it cannot correlate) both count here.
#[tokio::test]
async fn reject_bucket_malformed() {
    let mut h = Harness::new(bucket_cfg()).await;
    h.join(ConnectionId(1)).await;

    // Source 1: garbage payload under the envelope op (decode failure).
    h.raw_envelope(ConnectionId(1), &[0xFF, 0xFF, 0xFF]).await;
    h.tick();
    let replies = h
        .private_replies(ConnectionId(1), Duration::from_secs(2))
        .await;
    assert_eq!(replies, vec![(0, false)]);

    // Source 2: well-formed envelope, `id = 0`.
    h.request(ConnectionId(1), 0, OP_LOCAL, &[]).await;
    h.tick();
    let replies = h
        .private_replies(ConnectionId(1), Duration::from_secs(2))
        .await;
    assert_eq!(replies, vec![(0, false)]);

    let r = Rejects::of(&h.latest_room_sample());
    r.assert_only("malformed", 2, r.malformed);
    h.shutdown().await;
}

/// Bucket `dup`: a duplicate id that is still in flight is rejected
/// without re-processing. Trigger: an external request (registers
/// pending) plus a second request under the SAME id in the same tick —
/// the duplicate check sits above the decision, so even a room-local op
/// is rejected here (the reply is `ok = false`, the pending one keeps
/// running).
#[tokio::test]
async fn reject_bucket_dup() {
    let mut h = Harness::new(bucket_cfg()).await;
    h.join(ConnectionId(1)).await;

    h.request(ConnectionId(1), 101, OP_EXT, &[]).await; // registers pending
    h.request(ConnectionId(1), 101, OP_LOCAL, &[]).await; // same id: dup
    h.tick();
    let replies = h
        .private_replies(ConnectionId(1), Duration::from_secs(2))
        .await;
    assert_eq!(replies, vec![(101, false)]);

    let r = Rejects::of(&h.latest_room_sample());
    r.assert_only("dup", 1, r.dup);
    h.shutdown().await;
}

/// Bucket `no_handler`: the logic does not handle the request's op
/// (`handle_request` returns `None`). Not hard to trigger in this
/// harness: `RpcLogic` returns `None` for every op except its four known
/// ones, so `OP_UNKNOWN` reaches the core's "no handler" rejection.
#[tokio::test]
async fn reject_bucket_no_handler() {
    let mut h = Harness::new(bucket_cfg()).await;
    h.join(ConnectionId(1)).await;

    h.request(ConnectionId(1), 105, OP_UNKNOWN, &[]).await;
    h.tick();
    let replies = h
        .private_replies(ConnectionId(1), Duration::from_secs(2))
        .await;
    assert_eq!(replies, vec![(105, false)]);

    let r = Rejects::of(&h.latest_room_sample());
    r.assert_only("no_handler", 1, r.no_handler);
    h.shutdown().await;
}

/// Bucket `logic`: the logic's own `RequestDecision::Reject` (a normal
/// rejection with the logic's reason). Trigger: `OP_REJECT`.
#[tokio::test]
async fn reject_bucket_logic() {
    let mut h = Harness::new(bucket_cfg()).await;
    h.join(ConnectionId(1)).await;

    h.request(ConnectionId(1), 102, OP_REJECT, &[]).await;
    h.tick();
    let replies = h
        .private_replies(ConnectionId(1), Duration::from_secs(2))
        .await;
    assert_eq!(replies, vec![(102, false)]);

    let r = Rejects::of(&h.latest_room_sample());
    r.assert_only("logic", 1, r.logic);
    h.shutdown().await;
}

/// Bucket `conn_cap`: the per-connection pending cap. Trigger: cap = 1,
/// two external requests from the same connection in ONE tick — the
/// first registers (0 in flight < 1), the second sees 1 in flight ≥ 1
/// and is rejected on the per-connection cap while the room cap (default
/// 2000) is nowhere near.
#[tokio::test]
async fn reject_bucket_conn_cap() {
    let mut h = Harness::new(RoomConfig {
        id: RoomId(1),
        tick_hz: 30.0,
        metrics_cadence_hz: 30.0,
        max_pending_requests_per_conn: 1,
        ..Default::default()
    })
    .await;
    h.join(ConnectionId(1)).await;

    h.request(ConnectionId(1), 111, OP_EXT, &[]).await;
    h.request(ConnectionId(1), 112, OP_EXT, &[]).await;
    h.tick(); // first registers, second hits the per-connection cap
    let replies = h
        .private_replies(ConnectionId(1), Duration::from_secs(2))
        .await;
    assert_eq!(replies, vec![(112, false)]);

    let r = Rejects::of(&h.latest_room_sample());
    r.assert_only("conn_cap", 1, r.conn_cap);
    h.shutdown().await;
}

/// Bucket `room_cap`: the room-wide pending cap. Trigger: room cap = 1,
/// conn 1's external request fills the room's single slot; conn 2's
/// request on the next tick sees the room cap reached while its OWN
/// per-connection count is still zero — so it must land in the room
/// bucket, not the per-connection one (the `conn_cap == 0` assert is the
/// half that catches a crossed wiring).
#[tokio::test]
async fn reject_bucket_room_cap() {
    let mut h = Harness::new(RoomConfig {
        id: RoomId(1),
        tick_hz: 30.0,
        metrics_cadence_hz: 30.0,
        max_pending_requests: 1,
        ..Default::default()
    })
    .await;
    h.join(ConnectionId(1)).await;
    h.join(ConnectionId(2)).await;

    h.request(ConnectionId(1), 121, OP_EXT, &[]).await;
    h.tick(); // conn 1 fills the room's single slot
    let _r1 = h.next_resolver().await; // registered pending

    h.request(ConnectionId(2), 122, OP_EXT, &[]).await;
    h.tick(); // room cap reached; conn 2's own count is 0
    let replies = h
        .private_replies(ConnectionId(2), Duration::from_secs(2))
        .await;
    assert_eq!(replies, vec![(122, false)]);

    let r = Rejects::of(&h.latest_room_sample());
    r.assert_only("room_cap", 1, r.room_cap);
    h.shutdown().await;
}

/// Priority when a request is past BOTH caps: it counts against the
/// per-connection bucket (the room checks `over_conn_cap` first — the
/// client's own quota is the actionable one). Trigger: both caps = 1;
/// conn 1's first request fills its slot AND the room's; its second
/// request is over both and must land in `conn_cap`, not `room_cap`.
#[tokio::test]
async fn reject_bucket_both_caps_prefers_conn_cap() {
    let mut h = Harness::new(RoomConfig {
        id: RoomId(1),
        tick_hz: 30.0,
        metrics_cadence_hz: 30.0,
        max_pending_requests_per_conn: 1,
        max_pending_requests: 1,
        ..Default::default()
    })
    .await;
    h.join(ConnectionId(1)).await;

    h.request(ConnectionId(1), 131, OP_EXT, &[]).await;
    h.tick(); // fills its slot, which IS the room's slot
    let _r1 = h.next_resolver().await;

    h.request(ConnectionId(1), 132, OP_EXT, &[]).await;
    h.tick(); // over both caps
    let replies = h
        .private_replies(ConnectionId(1), Duration::from_secs(2))
        .await;
    assert_eq!(replies, vec![(132, false)]);

    let r = Rejects::of(&h.latest_room_sample());
    r.assert_only("conn_cap (priority over room cap)", 1, r.conn_cap);
    h.shutdown().await;
}
