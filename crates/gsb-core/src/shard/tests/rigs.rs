//! The rigs the shard suites drive: a single-shard harness, a bare
//! actor, and the two/three-shard border rigs.

use super::*;

/// The harness: two shards off a manual ticker; the test feeds ticks
/// and observes each shard's world content (and the migration
/// reports) through the obs channel.
pub(in crate::shard::tests) struct Harness {
    pub(in crate::shard::tests) tick_tx: broadcast::Sender<TickInfo>,
    pub(in crate::shard::tests) shard_txs: [Mailbox<ShardMsg<TState, TStrip>>; 2],
    pub(in crate::shard::tests) obs: mpsc::Receiver<Obs>,
    pub(in crate::shard::tests) ops: mpsc::Receiver<(PlayerId, u16)>,
    #[allow(dead_code)]
    pub(in crate::shard::tests) handles: Vec<tokio::task::JoinHandle<()>>,
    pub(in crate::shard::tests) t: u64,
    /// Migration reports: (tick, from-shard) → wires.
    pub(in crate::shard::tests) migrated: HashMap<(u64, usize), Vec<u64>>,
    /// The latest completed tick's content, per shard.
    pub(in crate::shard::tests) content: [Vec<(u64, f32, f32, i8)>; 2],
}

pub(in crate::shard::tests) fn metrics_null() -> mpsc::Sender<MetricsEvent> {
    let (tx, _rx) = mpsc::channel(1);
    tx
}

impl Harness {
    pub(in crate::shard::tests) fn new() -> Self {
        let (tick_tx, _) = broadcast::channel(64);
        let (tx0, rx0) = channel::<ShardMsg<TState, TStrip>>(128);
        let (tx1, rx1) = channel::<ShardMsg<TState, TStrip>>(128);
        let (obs_tx, obs_rx) = mpsc::channel(4096);
        let (ops_tx, ops_rx) = mpsc::channel(4096);
        let (dummy_tx, _dummy_rx) = channel::<ShardMsg<TState, TStrip>>(1);
        let cfg = RoomConfig {
            id: RoomId(7),
            keepalive_hz: 0.0, // silence the keep-alive re-sends
            metrics_cadence_hz: 0.0,
            ..Default::default()
        };
        let h0 = tokio::spawn(
            ShardActor::new(
                cfg.clone(),
                0,
                TWorld::default(),
                Box::new(TLogic {
                    index: 0,
                    next_serial: 0,
                    capacity: SHARD_SERIAL_CAPACITY,
                    player_ent: HashMap::new(),
                    ent_player: HashMap::new(),
                    last_tick: 0,
                    obs: obs_tx.clone(),
                    ops: ops_tx.clone(),
                }),
                tick_tx.subscribe(),
                rx0,
                // Indexed by the receiver's shard index (the actor's
                // lookup); slot 0 (itself) is the dummy.
                vec![dummy_tx.clone(), tx1.clone()],
                1,
                metrics_null(),
                None, // no result sink in the protocol harness
            )
            .run(),
        );
        let h1 = tokio::spawn(
            ShardActor::new(
                cfg,
                1,
                TWorld::default(),
                Box::new(TLogic {
                    index: 1,
                    next_serial: 0,
                    capacity: SHARD_SERIAL_CAPACITY,
                    player_ent: HashMap::new(),
                    ent_player: HashMap::new(),
                    last_tick: 0,
                    obs: obs_tx.clone(),
                    ops: ops_tx,
                }),
                tick_tx.subscribe(),
                rx1,
                vec![tx0.clone(), dummy_tx],
                1,
                metrics_null(),
                None, // no result sink in the protocol harness
            )
            .run(),
        );
        drop(obs_tx);
        Harness {
            tick_tx,
            shard_txs: [tx0, tx1],
            obs: obs_rx,
            ops: ops_rx,
            handles: vec![h0, h1],
            t: 0,
            migrated: HashMap::new(),
            content: [Vec::new(), Vec::new()],
        }
    }

    /// Feed one tick to the manual ticker and wait until BOTH shards
    /// have reported their content for it (the obs channel carries the
    /// per-shard, per-tick observations; a tick is "done" when both
    /// shards reported it). Returns the two shards' content.
    pub(in crate::shard::tests) async fn tick(&mut self) -> [Vec<(u64, f32, f32, i8)>; 2] {
        self.t += 1;
        let t = self.t;
        self.tick_tx
            .send(TickInfo {
                tick: t,
                at: Instant::now(),
            })
            .expect("at least one subscriber");
        let mut content: [Vec<(u64, f32, f32, i8)>; 2] = [Vec::new(), Vec::new()];
        // Track which shards have REPORTED this tick (an empty shard
        // reports an empty vec, so emptiness is not a done-signal).
        let mut reported = [false; 2];
        let deadline = Instant::now() + Duration::from_secs(5);
        while !reported.iter().all(|&r| r) {
            let wait = deadline.saturating_duration_since(Instant::now());
            let item = match tokio::time::timeout(wait, self.obs.recv()).await {
                Ok(x) => x.expect("obs channel closed"),
                Err(_) => {
                    panic!(
                        "shards did not both process tick {} in time; \
                         reported={reported:?} got={content:?}",
                        t
                    );
                }
            };
            match item {
                Obs::Content(s, tick, c) if tick == t && !reported[s] => {
                    content[s] = c;
                    reported[s] = true;
                }
                Obs::Migrate(s, tick, wire) => {
                    self.migrated.entry((tick, s)).or_default().push(wire);
                }
                _ => {}
            }
        }
        self.content = content;
        self.content.clone()
    }

    /// Join `conn` to `shard` (the registry's home-shard routing is
    /// the server's business; the test picks the shard directly).
    /// The join is processed on the next tick; returns (wire, the
    /// action channel, the snapshot out channel).
    pub(in crate::shard::tests) async fn join(
        &mut self,
        shard: usize,
        conn: ConnectionId,
        epoch: u64,
    ) -> (u64, mpsc::Sender<Action>, mpsc::Receiver<FrameBatch>) {
        let (reply_tx, reply_rx) = oneshot::channel();
        let (out_tx, out_rx) = mpsc::channel(64);
        self.shard_txs[shard]
            .send(ShardMsg::Join {
                conn,
                epoch,
                identity: String::new(),
                out: out_tx,
                reply: reply_tx,
            })
            .await
            .expect("shard channel open");
        // Drive the tick that processes the join (its CONTROL phase
        // sends the reply), then collect the reply. In the test logic
        // the entity id IS the wire id.
        let _ = self.tick().await;
        let (wire, actions) = reply_rx
            .await
            .expect("join reply delivered")
            .expect("join succeeded");
        (wire, actions, out_rx)
    }

    /// Send one input action (the connection actor's role).
    pub(in crate::shard::tests) async fn act(
        &self,
        actions: &mpsc::Sender<Action>,
        conn: ConnectionId,
        op: u16,
    ) {
        actions
            .send(Action {
                conn,
                // Test identity policy (matches TLogic::on_join): the
                // conn id doubles as the player id.
                player: PlayerId(conn.0),
                op,
                payload: bytes::Bytes::new(),
            })
            .await
            .expect("action channel open");
    }

    /// The registry's leave broadcast: to ALL shards (exactly one owns
    /// the connection; the others no-op on the entity-id guard).
    pub(in crate::shard::tests) async fn leave(
        &self,
        conn: ConnectionId,
        entity: EntityId,
        epoch: u64,
    ) {
        for shard in 0..2 {
            self.shard_txs[shard]
                .send(ShardMsg::Leave {
                    conn,
                    entity,
                    epoch,
                })
                .await
                .expect("shard channel open");
        }
    }

    /// The ops the shards ingested, in order (drain).
    pub(in crate::shard::tests) async fn ops_drained(&mut self) -> Vec<(PlayerId, u16)> {
        let mut out = Vec::new();
        while let Ok(op) = self.ops.try_recv() {
            out.push(op);
        }
        out
    }
}

// -----------------------------------------------------------------
// Table-pruning locks: the connection tables must not grow forever
// with connection churn (`conn_epoch` pruned on Leave; tombstones
// TTL'd + swept). These drive a BARE actor (not spawned): the
// harness above can only see observable wire behavior — the right
// level for the protocol invariants, but too coarse for "is this
// exact map entry gone". A child module may touch the private
// tables directly; the assertions below are the behavior locks.
// -----------------------------------------------------------------

/// An unspawned shard actor for the table tests above/below.
pub(in crate::shard::tests) fn bare_shard(index: usize) -> ShardActor<TWorld, (), TState, TStrip> {
    bare_shard_capped(index, SHARD_SERIAL_CAPACITY)
}

/// [`bare_shard`] whose logic may draw only `capacity` serials.
pub(in crate::shard::tests) fn bare_shard_capped(
    index: usize,
    capacity: u64,
) -> ShardActor<TWorld, (), TState, TStrip> {
    let (_tick_tx, tick_rx) = broadcast::channel(64);
    let (_self_tx, rx) = channel::<ShardMsg<TState, TStrip>>(16);
    // Two dummy neighbor slots (TLogic::neighbors targets 0 and 1).
    let (n0, _n0_rx) = channel::<ShardMsg<TState, TStrip>>(8);
    let (n1, _n1_rx) = channel::<ShardMsg<TState, TStrip>>(8);
    let (obs, _obs_rx) = mpsc::channel(16);
    let (ops, _ops_rx) = mpsc::channel(16);
    ShardActor::new(
        RoomConfig {
            id: RoomId(9),
            keepalive_hz: 0.0,
            metrics_cadence_hz: 0.0,
            ..Default::default()
        },
        index,
        TWorld::default(),
        Box::new(TLogic {
            index,
            next_serial: 0,
            capacity,
            player_ent: HashMap::new(),
            ent_player: HashMap::new(),
            last_tick: 0,
            obs,
            ops,
        }),
        tick_rx,
        rx,
        vec![n0, n1],
        1,
        metrics_null(),
        None, // no result sink
    )
}

pub(in crate::shard::tests) fn tinfo(tick: u64) -> TickInfo {
    TickInfo {
        tick,
        at: Instant::now(),
    }
}

/// A tick stamped `late_by` in the PAST, so the actor's measured tick
/// latency (`step start − t.at`) is at least `late_by`: the test sets the
/// latency it wants to observe instead of racing the scheduler for it.
pub(in crate::shard::tests) fn tinfo_late_by(tick: u64, late_by: Duration) -> TickInfo {
    TickInfo {
        tick,
        at: Instant::now()
            .checked_sub(late_by)
            .expect("the monotonic clock is further from its epoch than the test's offset"),
    }
}

/// Drive one Join through `handle_msg`; returns the minted entity.
pub(in crate::shard::tests) async fn join_direct(
    a: &mut ShardActor<TWorld, (), TState, TStrip>,
    conn: ConnectionId,
    epoch: u64,
    tick: u64,
) -> EntityId {
    let (reply_tx, reply_rx) = oneshot::channel();
    let (out_tx, _out_rx) = mpsc::channel::<FrameBatch>(8);
    assert!(
        a.handle_msg(
            ShardMsg::Join {
                conn,
                epoch,
                identity: String::new(),
                out: out_tx,
                reply: reply_tx
            },
            tick
        ),
        "a join must never stop the actor"
    );
    reply_rx
        .await
        .expect("join reply delivered")
        .expect("join ok")
        .0
}

pub(in crate::shard::tests) fn ghost_migrate(
    conn: ConnectionId,
    epoch: u64,
    entity: EntityId,
    at_tick: u64,
) -> ShardMsg<TState, TStrip> {
    let (out, _out_rx) = mpsc::channel::<FrameBatch>(8);
    let (_act_tx, act_rx) = mpsc::channel::<Action>(8);
    ShardMsg::Migrate {
        from: 0,
        at_tick,
        wire: entity,
        state: TState {
            x: -7.0,
            y: 0.0,
            mode: 0,
        },
        player: Some(Box::new(PlayerMigration {
            // Test identity policy: the conn id doubles as the player.
            player: PlayerId(conn.0),
            conn,
            epoch,
            entity,
            out,
            actions: act_rx,
            detached: false,
            detach_deadline: None,
            detach_ceiling: None,
            expire_to: crate::room::ExpireTo::Despawn,
            bot_fed: false,
            session_epoch: 0,
            identity: String::new(),
            last_input: None,
            path: None,
        })),
    }
}

/// A TLogic for the border rig; its observation channels are dead
/// (every send is `let _ =` ignored) — the tests read protocol state,
/// not world observations.
pub(in crate::shard::tests) fn rig_logic(index: usize) -> TLogic {
    let (obs, _obs_rx) = mpsc::channel(16);
    let (ops, _ops_rx) = mpsc::channel(16);
    TLogic {
        index,
        next_serial: 0,
        capacity: SHARD_SERIAL_CAPACITY,
        player_ent: HashMap::new(),
        ent_player: HashMap::new(),
        last_tick: 0,
        obs,
        ops,
    }
}

pub(in crate::shard::tests) fn rig_actor(
    index: usize,
    neighbors: Vec<Mailbox<ShardMsg<TState, TStrip>>>,
) -> ShardActor<TWorld, (), TState, TStrip> {
    let (_tick_tx, tick_rx) = broadcast::channel(64);
    let (_self_tx, rx) = channel::<ShardMsg<TState, TStrip>>(16);
    ShardActor::new(
        RoomConfig {
            id: RoomId(13),
            keepalive_hz: 0.0,
            metrics_cadence_hz: 0.0,
            ..Default::default()
        },
        index,
        TWorld::default(),
        Box::new(rig_logic(index)),
        tick_rx,
        rx,
        neighbors,
        1,
        metrics_null(),
        None, // no result sink
    )
}

/// Two bare shard actors (0 and 1, mutual neighbors) wired through
/// channels the TEST controls. `put` seeds strip entities directly —
/// the strip is `|x| <= 1`, and x stays strictly inside the owner's
/// region so no migration path ever fires under these tests.
pub(in crate::shard::tests) struct BorderRig {
    pub(in crate::shard::tests) s0: ShardActor<TWorld, (), TState, TStrip>,
    pub(in crate::shard::tests) s1: ShardActor<TWorld, (), TState, TStrip>,
    /// What s0 exports to s1 lands here (test-held receiving end).
    pub(in crate::shard::tests) tx01: Mailbox<ShardMsg<TState, TStrip>>,
    pub(in crate::shard::tests) rx01: Inbox<ShardMsg<TState, TStrip>>,
    /// What s1 sends back (resync requests) lands here.
    pub(in crate::shard::tests) _tx10: Mailbox<ShardMsg<TState, TStrip>>,
    pub(in crate::shard::tests) rx10: Inbox<ShardMsg<TState, TStrip>>,
}

impl BorderRig {
    /// DELTA-mode rig: the default for the §6.4 protocol locks, whose
    /// assertions are about upsert/exit/resync packaging.
    pub(in crate::shard::tests) fn new() -> Self {
        Self::with_mode(ExchangeMode::Delta)
    }

    /// ALWAYS-FULL rig: Faz C's local-link derivation under test.
    pub(in crate::shard::tests) fn new_always_full() -> Self {
        Self::with_mode(ExchangeMode::AlwaysFull)
    }

    pub(in crate::shard::tests) fn with_mode(mode: ExchangeMode) -> Self {
        let (tx01, rx01) = mpsc::channel(16);
        let (tx10, rx10) = mpsc::channel(16);
        // Slot fillers for the unused self-slots (never targeted:
        // TLogic::neighbors is [1] for index 0 and [0] for index 1).
        let (d0, _d0rx) = channel::<ShardMsg<TState, TStrip>>(1);
        let (d1, _d1rx) = channel::<ShardMsg<TState, TStrip>>(1);
        let mut s0 = rig_actor(0, vec![d0, tx01.clone()]);
        let mut s1 = rig_actor(1, vec![tx10.clone(), d1]);
        // Both directions run the requested packaging (the self-slot
        // entries are never targeted; sizing the override to the
        // links vec keeps indexing trivially safe).
        s0.force_exchange_modes(vec![mode, mode]);
        s1.force_exchange_modes(vec![mode, mode]);
        BorderRig {
            s0,
            s1,
            tx01,
            rx01,
            _tx10: tx10,
            rx10,
        }
    }

    /// Run shard 0's phases at this tick index (its exports land in
    /// `rx01`).
    pub(in crate::shard::tests) fn step0(&mut self, tick: u64) {
        assert!(self.s0.step_phases(&tinfo(tick)), "shard 0 keeps running");
    }

    /// Take everything shard 0 exported (WITHOUT delivering): the
    /// test inspects each message and decides deliver vs drop — the
    /// exact seam a lost exchange needs.
    pub(in crate::shard::tests) fn drain01(&mut self) -> Vec<ShardMsg<TState, TStrip>> {
        let mut out = Vec::new();
        while let Ok(m) = self.rx01.try_recv() {
            out.push(m);
        }
        out
    }

    /// Feed messages into shard 1's CONTROL handler.
    pub(in crate::shard::tests) fn deliver_to_s1(&mut self, msgs: Vec<ShardMsg<TState, TStrip>>) {
        for m in msgs {
            assert!(self.s1.handle_msg(m, 999), "s1 keeps running");
        }
    }

    /// Feed messages into shard 0's CONTROL handler.
    pub(in crate::shard::tests) fn deliver_to_s0(&mut self, msgs: Vec<ShardMsg<TState, TStrip>>) {
        for m in msgs {
            assert!(self.s0.handle_msg(m, 999), "s0 keeps running");
        }
    }
}

/// Seed a boundary entity straight into a shard's world.
pub(in crate::shard::tests) fn put(
    a: &mut ShardActor<TWorld, (), TState, TStrip>,
    wire: u64,
    x: f32,
    y: f32,
) {
    a.world.ents.insert(wire, (x, y, 0));
}

/// Assert the batch is exactly one Border carrying a Full; return its
/// (seq, entities). Generic over the strip payload so every rig
/// (positional and rich) reuses one helper.
pub(in crate::shard::tests) fn expect_full<S: Debug>(
    msgs: &[ShardMsg<TState, S>],
) -> (u64, &[BorderRecord<S>]) {
    assert_eq!(msgs.len(), 1, "exactly one export message: {msgs:?}");
    match &msgs[0] {
        ShardMsg::Border {
            exchange: BorderExchange::Full { seq, entities, .. },
            ..
        } => (*seq, entities.as_slice()),
        other => panic!("expected a Full exchange, got {other:?}"),
    }
}

/// Assert the batch is exactly one Border carrying a Delta; return
/// its (seq, upserts, exits). Generic over the strip payload.
pub(in crate::shard::tests) fn expect_delta<S: Debug>(
    msgs: &[ShardMsg<TState, S>],
) -> (u64, &[BorderRecord<S>], &[u64]) {
    assert_eq!(msgs.len(), 1, "exactly one export message: {msgs:?}");
    match &msgs[0] {
        ShardMsg::Border {
            exchange:
                BorderExchange::Delta {
                    seq,
                    upserts,
                    exits,
                    ..
                },
            ..
        } => (*seq, upserts.as_slice(), exits.as_slice()),
        other => panic!("expected a Delta exchange, got {other:?}"),
    }
}
