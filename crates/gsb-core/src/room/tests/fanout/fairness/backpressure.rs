//! Back-pressure: a full outbound channel drops that tick's batch,
//! counts it, and the connection recovers on the next one.

use super::*;

/// Batch-buffer reuse (the floor turn): the fan-out hands each tick's
/// batch to the outbound channel with `mem::take` and, on a full
/// channel, restores it through `TrySendError::into_inner`. This
/// locks the recovery path: after a stretch in which the outbound
/// channel stayed full (emissions dropped), the channel must hold
/// exactly its capacity of intact batches — and, once space appears,
/// the NEXT emission must arrive (no wedged connection, no lost
/// buffer, nothing past capacity).
#[tokio::test]
async fn full_outbound_channel_drops_then_recovers() {
    // Wider steps pipe than the test's tick count: the room's
    // `try_send` on it is best-effort (a full pipe would silently
    // lose the late step numbers and starve `wait_steps`).
    let (step_tx, mut steps) = mpsc::channel(256);
    let (tick_tx, tick_rx) = broadcast::channel(64);
    let (control, control_rx) = channel(128);
    let actor = RoomActor::new(
        RoomConfig {
            id: RoomId(4),
            ..Default::default()
        },
        (),
        Box::new(FairLogic {
            last_world: 0,
            last_emitted: HashMap::new(),
            step_no: 0,
            steps: step_tx,
        }),
        tick_rx,
        control_rx,
        1,
        null_metrics_tx(),
        None,
    );
    let handle = tokio::spawn(actor.run());
    let t0 = Instant::now();
    let mut next_tick = 0;
    let mut tick = || {
        next_tick += 1;
        let at = t0 + Duration::from_secs_f64(next_tick as f64 / 30.0);
        tick_tx
            .send(TickInfo {
                tick: next_tick,
                at,
            })
            .expect("room subscriber alive");
    };

    // One connection, an outbound channel of capacity 64.
    let (out_tx, mut rx) = mpsc::channel::<FrameBatch>(64);
    let (reply_tx, reply_rx) = oneshot::channel::<Result<(EntityId, Mailbox<Action>), CoreError>>();
    control
        .send(RoomControl::Join {
            conn: ConnectionId(1),
            out: out_tx,
            reply: reply_tx,
        })
        .await
        .expect("control alive");
    tick();
    tokio::time::timeout(Duration::from_secs(2), reply_rx)
        .await
        .expect("join reply timeout")
        .expect("join reply dropped")
        .expect("join accepted (room not full)");

    // The world changes on every tick ⇒ one emission per tick:
    // 69 more ticks ⇒ 70 emissions total against 64 slots. The
    // channel keeps the OLDEST 64 (new ones fail `try_send` and are
    // dropped — the documented one-snapshot-of-staleness cost).
    // Yield between sends: on a current-thread runtime the room task
    // cannot consume the broadcast while the test runs, and a full
    // broadcast buffer would overwrite the oldest ticks.
    for _ in 0..69 {
        tick();
        tokio::task::yield_now().await;
    }
    wait_steps(&mut steps, 70).await;

    let all = drain_all(&mut rx).await;
    assert_eq!(all.len(), 64, "the channel holds exactly its capacity");
    let seq: Vec<u64> = all
        .iter()
        .map(|b| {
            assert_eq!(b[0].op, 0x7020, "snapshot opcode");
            u64::from_le_bytes(
                b[0].payload
                    .get(0..8)
                    .expect("8-byte payload")
                    .try_into()
                    .expect("8-byte payload"),
            )
        })
        .collect();
    assert_eq!(seq, (1..=64).collect::<Vec<_>>(), "intact, in order");

    // Space reappears: the next emission must arrive (the batch
    // buffer survived the full-channel stretch).
    tick();
    wait_steps(&mut steps, 71).await;
    let recovered = drain_all(&mut rx).await;
    assert_eq!(recovered.len(), 1, "the post-full emission arrives");
    assert_eq!(
        recovered[0][0].payload.as_ref(),
        71u64.to_le_bytes(),
        "and it is tick 71's snapshot"
    );

    control
        .send(RoomControl::Shutdown)
        .await
        .expect("control alive");
    tick();
    tokio::time::timeout(Duration::from_secs(2), handle)
        .await
        .expect("room did not shut down")
        .expect("room task panicked");
}

#[tokio::test]
async fn unchanged_group_is_silent_until_keepalive() {
    // Keep-alive every 3 steps (10 Hz under a 30 Hz room).
    let (step_tx, mut steps) = mpsc::channel(64);
    let mut room = GLRoom::new(
        RoomConfig {
            id: RoomId(2),
            keepalive_hz: 10.0,
            ..Default::default()
        },
        GroupLogic {
            player_entity: HashMap::new(),
            next: 0,
            dirty: std::collections::HashSet::new(),
            step_no: 0,
            steps: step_tx,
        },
    );

    let (ent, mut a_rx) = room.join(ConnectionId(1)).await;
    wait_steps(&mut steps, 1).await;
    // Step 1 (the join tick): the membership change shipped a snapshot.
    let first = next_batch_full(&mut a_rx).await;
    assert_eq!(
        batch_frames(&first),
        vec![(0x7010, ent.to_le_bytes().to_vec())]
    );

    // Steps 2..8: no change. The room stays silent — except on the
    // keep-alive steps (3 and 6), which re-send the cached snapshot.
    for _ in 0..7 {
        room.step().await;
    }
    wait_steps(&mut steps, 8).await;

    let mut got = vec![first];
    while let Ok(batch) = a_rx.try_recv() {
        got.push(batch);
    }
    assert_eq!(
        got.len(),
        3,
        "one emission (step 1) + two keep-alive re-sends (steps 3, 6), \
         nothing else"
    );
    for batch in &got {
        assert_eq!(
            batch_frames(batch),
            vec![(0x7010, ent.to_le_bytes().to_vec())],
            "keep-alive re-sends the cached snapshot bytes"
        );
    }

    room.shutdown().await;
}

// -----------------------------------------------------------------
// F5: keep-alive rate above the room rate. The registry rejects such
// a config at room creation; direct construction (library use) must
// still never be silent: the constructor warns once naming both
// rates, and the cadence clamps to every step (the observable
// behavior: an unchanged group re-sends on *every* step).
// -----------------------------------------------------------------

#[tokio::test]
async fn keepalive_above_tick_warns_at_construction_and_clamps_to_every_step() {
    // Thread-local subscriber (NOT the process-global default: other
    // tests in this binary may run concurrently on other threads, and
    // the never-emitted test owns the global slot).
    let (warn_tx, mut warns) = mpsc::channel::<String>(64);
    let (tick_tx, tick_rx) = broadcast::channel(64);
    let (control, control_rx) = channel(64);
    let (step_tx, mut steps) = mpsc::channel(64);

    // Direct construction with keepalive_hz = 60 under a 30 Hz room —
    // the misconfiguration the registry would have rejected.
    let actor = tracing::subscriber::with_default(
        WarnCapture {
            tx: warn_tx.clone(),
        },
        || {
            RoomActor::new(
                RoomConfig {
                    id: RoomId(25),
                    keepalive_hz: 60.0,
                    ..Default::default()
                },
                (),
                Box::new(GroupLogic {
                    player_entity: HashMap::new(),
                    next: 0,
                    dirty: std::collections::HashSet::new(),
                    step_no: 0,
                    steps: step_tx,
                }),
                tick_rx,
                control_rx,
                1,
                null_metrics_tx(),
                None,
            )
        },
    );
    let handle = tokio::spawn(actor.run());
    let t0 = Instant::now();

    // The construction-time warn fired exactly once and names both
    // rates (synchronous: the warn! runs inside the constructor).
    let w = warns
        .try_recv()
        .expect("misconfigured construction must warn");
    assert!(
        w.contains("keepalive_hz=60") && w.contains("tick_hz=30"),
        "warn must name both rates: {w}"
    );
    assert!(
        warns.try_recv().is_err(),
        "the construction warn must fire exactly once: {w}"
    );

    // Clamped behavior: the join's own emission, then EVERY step is a
    // keep-alive step (interval 1) re-sending the cached snapshot.
    let (out_tx, mut a_rx) = mpsc::channel::<FrameBatch>(64);
    let (reply_tx, reply_rx) = oneshot::channel::<Result<(EntityId, Mailbox<Action>), CoreError>>();
    control
        .send(RoomControl::Join {
            conn: ConnectionId(26),
            out: out_tx,
            reply: reply_tx,
        })
        .await
        .expect("control alive");
    for n in 1..=6u64 {
        tick_tx
            .send(TickInfo {
                tick: n,
                at: t0 + Duration::from_secs_f64(n as f64 / 30.0),
            })
            .expect("room subscriber alive");
    }
    let _ = reply_rx
        .await
        .expect("join reply dropped")
        .expect("join accepted (room not full)");
    wait_steps(&mut steps, 6).await;

    let got = drain_all(&mut a_rx).await;
    assert_eq!(
        got.len(),
        6,
        "join emission + 5 keep-alive re-sends (one per step, no \
         silence at all): {got:?}"
    );
    for (i, batch) in got.iter().enumerate() {
        assert_eq!(
            batch_frames(batch),
            vec![(0x7010, 1u64.to_le_bytes().to_vec())],
            "step {i} must carry the cached snapshot (entity 1)"
        );
    }

    drop(tick_tx); // ticker closed → the room exits cleanly
    tokio::time::timeout(Duration::from_secs(2), handle)
        .await
        .expect("room did not exit on closed ticker")
        .expect("room task panicked");

    // No false positives: legitimate ratios must not warn. keep-alive
    // == tick is exactly "one per step" (as configured); 1 Hz is the
    // default setup.
    for keepalive in [30.0, 1.0] {
        let (_tick2, tick_rx2) = broadcast::channel(8);
        let (_control2, control_rx2) = channel(8);
        let (step2_tx, _step2_rx) = mpsc::channel::<u64>(8);
        tracing::subscriber::with_default(
            WarnCapture {
                tx: warn_tx.clone(),
            },
            || {
                let _actor2 = RoomActor::new(
                    RoomConfig {
                        id: RoomId(27),
                        keepalive_hz: keepalive,
                        ..Default::default()
                    },
                    (),
                    Box::new(GroupLogic {
                        player_entity: HashMap::new(),
                        next: 0,
                        dirty: std::collections::HashSet::new(),
                        step_no: 0,
                        steps: step2_tx,
                    }),
                    tick_rx2,
                    control_rx2,
                    1,
                    null_metrics_tx(),
                    None,
                );
            },
        );
    }
    assert!(
        warns.try_recv().is_err(),
        "keepalive_hz <= tick_hz must not warn"
    );
}

// -----------------------------------------------------------------
// F4 diagnostic: a group that has members but has never emitted
// (snapshot → false on its first tick, although a fresh group's first
// tick is a membership change and must emit) must be warned about —
// exactly once, naming the group. The diagnostic previously had no
// test; a violating logic (e.g. the shared-ledger misuse the room
// cannot distinguish from legitimate silence) stays invisible without
// one.
// -----------------------------------------------------------------
