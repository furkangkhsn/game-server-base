//! Capacity and fairness guardrails: a full room rejects, and one
//! flooder cannot evict another connection's input.

use super::*;

// ── capacity + fairness guardrails (behaviour lock) ──────────────

/// Capacity guardrail: at `max_players` the next join is rejected with
/// `CoreError::RoomFull` — no entity, no action channel, no room
/// state — while the room (and its members) keeps working.
#[tokio::test]
async fn join_rejected_when_room_is_full() {
    let (dt_tx, _dts) = mpsc::channel(16);
    let (op_tx, mut ops) = mpsc::channel(16);
    let mut h = Harness::new(
        1,
        RoomConfig {
            id: RoomId(1),
            max_players: Some(2),
            ..Default::default()
        },
        RecLogic {
            dts: dt_tx,
            ops: op_tx,
        },
    );
    let period = Duration::from_secs_f64(1.0 / 30.0);

    // Two joins fill the room.
    let (_e1, _a1) = h.join(ConnectionId(1), 1).await;
    let (_e2, _a2) = h.join(ConnectionId(2), 1).await;

    // The third join is structurally rejected (the reply carries the
    // error; nothing is recorded in the room).
    let (out3_tx, _out3_rx) = mpsc::channel::<FrameBatch>(8);
    let (reply3_tx, reply3_rx) =
        oneshot::channel::<Result<(EntityId, Mailbox<Action>), CoreError>>();
    h.control
        .send(RoomControl::Join {
            conn: ConnectionId(3),
            out: out3_tx,
            reply: reply3_tx,
        })
        .await
        .expect("control alive");
    for _ in 0..2 {
        h.tick(period);
    }
    match tokio::time::timeout(Duration::from_secs(2), reply3_rx)
        .await
        .expect("timed out waiting for the rejection")
        .expect("reply dropped")
    {
        Err(CoreError::RoomFull(id)) => {
            assert_eq!(id, 1, "the error names the rejecting room")
        }
        other => panic!("expected RoomFull, got {other:?}"),
    }

    // Existing members are unaffected: the surviving member's action
    // still reaches the ingest on the next step.
    _a1
        .send(Action {
            conn: ConnectionId(1),
            player: PlayerId(1),
            op: 0x1500,
            payload: bytes::Bytes::new(),
        })
        .await
        .expect("member's action channel alive");
    h.tick(period);
    let op = tokio::time::timeout(Duration::from_secs(2), ops.recv())
        .await
        .expect("timed out waiting for the member op")
        .expect("ops closed");
    assert_eq!(op, 0x1500, "the surviving member's action is still ingested");
    h.shutdown().await;
}

/// Fairness guardrail: a connection that floods its own action
/// channel can no longer evict ANYONE ELSE's actions. The READ phase
/// is a bounded pull (per-connection budget + room pull budget), not
/// a merged list with an oldest-drop: the victim's one action per tick
/// is ingested on every tick, and the flooder's excess stays in its
/// OWN channel (deferred; the room drops nothing).
///
/// Under the old semantics (merged list, `drain(..over)` = oldest) the
/// merged list is built in `conns` iteration order and the overflow
/// drops its HEAD: with one flooded connection and one quiet one, the
/// quiet connection's actions sit in a small contiguous block of the
/// list, and the flooder's backlog determines which block overflows —
/// i.e. a single flooder could evict the other connection's actions
/// (which block was dropped depended on the hash order, so even the
/// victim was arbitrary). The per-connection pull budget removes the
/// interaction entirely: every connection's ingest is bounded by its
/// own budget, whoever it is.
#[tokio::test]
async fn flooder_cannot_evict_other_connections_actions() {
    let (dt_tx, _dts) = mpsc::channel(64);
    // Wide: the room ingests 188 ops (20 victim + 168 flood) and the
    // logic forwards each via try_send — the observation channel must
    // not be the thing that overflows in this test.
    let (op_tx, mut ops) = mpsc::channel(512);
    let mut h = Harness::new(
        1,
        RoomConfig {
            id: RoomId(1),
            // The fairness probe: a tight pull budget and a tight
            // per-connection budget (the old code had neither; the
            // merged list grew with the flooder's backlog).
            max_pending_actions: 16,
            max_actions_per_conn_per_tick: 8,
            ..Default::default()
        },
        RecLogic {
            dts: dt_tx,
            ops: op_tx,
        },
    );
    let period = Duration::from_secs_f64(1.0 / 30.0);

    // The quiet connection ("victim") and the flooded connection
    // ("flooder"); which block of the old merged list overflowed
    // depended on the HashMap iteration order, so neither role is
    // special — the new per-connection budgets make it a non-issue.
    let (_ev, victim) = h.join(ConnectionId(1), 1).await;
    let (_ef, flood) = h.join(ConnectionId(2), 1).await;

    // The flooder fills its own action channel to capacity (256):
    // the flood backlog the room would have merged (and overflowed)
    // under the old READ.
    let mut stuffed = 0usize;
    while let Ok(()) = flood.try_send(Action {
        conn: ConnectionId(2),
        player: PlayerId(2),
        op: 0x3000,
        payload: bytes::Bytes::new(),
    }) {
        stuffed += 1;
    }
    assert_eq!(stuffed, 256, "the flood backlog is the channel capacity");

    // 20 ticks: the victim sends exactly one action per tick.
    for t in 0..20u16 {
        victim
            .try_send(Action {
                conn: ConnectionId(1),
                player: PlayerId(1),
                op: 0x2000 + t,
                payload: bytes::Bytes::new(),
            })
            .expect("victim channel never full (one op per tick)");
        h.tick(period);
    }
    // One more tick so the last queued op is pulled and ingested.
    h.tick(period);

    // Collect everything ingested (the ticks are fire-and-forget, so
    // wait until the full expected volume has landed: 20 victim ops +
    // 8 floods/tick × 21 ticks = 168). Ingest ORDER between the two
    // connections is HashMap-driven and not asserted; OWNERSHIP is
    // what the guardrail guarantees.
    let mut victim_ops = 0u32;
    let mut flood_ops = 0u32;
    let mut total = 0u32;
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while total < 188 && std::time::Instant::now() < deadline {
        match ops.try_recv() {
            Ok(op) => {
                total += 1;
                if (0x2000..0x2014).contains(&op) {
                    victim_ops += 1;
                } else if op == 0x3000 {
                    flood_ops += 1;
                }
            }
            Err(_) => tokio::time::sleep(Duration::from_millis(5)).await,
        }
    }
    assert_eq!(
        total,
        188,
        "the room ingested the full expected volume (20 + 168)"
    );
    assert_eq!(
        victim_ops,
        20,
        "EVERY victim op was ingested — the flood evicted none of them"
    );
    assert_eq!(
        flood_ops,
        168,
        "the room pulled exactly 8 flood ops per tick (its per-conn budget)"
    );
    // The flooder's excess was DEFERRED in its own channel: the room
    // pulled 8/tick × 21 ticks = 168 (the ingested volume above), so
    // 88 of the original 256 are still queued — the room dropped
    // nothing. Observe it by filling the free slots: exactly 168
    // sends fit (= the amount pulled), the 169th hits Full.
    let mut free = 0usize;
    // (Full ends the loop: the backlog is exactly 256 − free.)
    while let Ok(()) = flood.try_send(Action {
        conn: ConnectionId(2),
        player: PlayerId(2),
        op: 0x3001,
        payload: bytes::Bytes::new(),
    }) {
        free += 1;
    }
    assert_eq!(
        free,
        8 * 21,
        "exactly the per-tick pull (8/tick × 21 ticks) freed slots; \
         the rest is still deferred in the flooder's own channel"
    );
    h.shutdown().await;
}
