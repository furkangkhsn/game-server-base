//! Per-link exchange modes: the local link stays always-full and two
//! directions of one seam may run different packagings.

use super::*;

/// Two bare [`RichLogic`] actors wired through channels the TEST
/// controls (the [`BorderRig`] pattern, typed over [`TRich`]).
/// Faz C lock 1 — same-process links run ALWAYS-FULL packaging: a
/// changed strip ships a complete Full every tick (no deltas on the
/// wire, no dirty suppression), because bytes over an mpsc move are
/// free while the diffing CPU was measured at 40-55x the full cost.
#[tokio::test]
async fn local_link_exchanges_are_always_full() {
    let mut r = BorderRig::new_always_full();
    put(&mut r.s0, 100, -1.0, 0.0);
    r.step0(1);
    let msgs = r.drain01();
    let (_seq, entities) = expect_full(&msgs);
    assert_eq!(entities.len(), 1, "bootstrap ships the strip as Full");

    // A move next tick ships ANOTHER Full — wholesale replacement,
    // never a Delta upsert.
    put(&mut r.s0, 100, -1.0, 1.0);
    r.step0(2);
    let msgs = r.drain01();
    let (_seq, entities) = expect_full(&msgs);
    assert_eq!(entities.len(), 1);

    r.deliver_to_s1(msgs);
    assert_eq!(r.s1.border[&0].recs.len(), 1, "view established");
    assert_eq!(
        r.s1.border[&0].recs[&100].state.y, 1,
        "the relocated position arrived"
    );
}

/// Faz C lock 2 — ALWAYS-FULL mode keeps the borrowed view exact
/// across UNCHANGED ticks too: wholesale replacement cannot ghost,
/// duplicate, or drift, locking the evaporation-guard under this
/// mode against future regressions.
#[tokio::test]
async fn always_full_keeps_view_exact_across_unchanged_ticks() {
    let mut r = BorderRig::new_always_full();
    put(&mut r.s0, 100, -1.0, 0.0);
    r.step0(1);
    let msgs = r.drain01();
    r.deliver_to_s1(msgs);

    // Three ticks with NOTHING changed: each still ships a Full of
    // the identical single record, and the receiving view never
    // grows beyond it.
    for tick in 2..=4 {
        r.step0(tick);
        let msgs = r.drain01();
        let (_seq, entities) = expect_full(&msgs);
        assert_eq!(entities.len(), 1, "tick {tick} ships the strip");
        r.deliver_to_s1(msgs);
        assert_eq!(
            r.s1.border[&0].recs.len(),
            1,
            "view stays exactly one record at tick {tick}"
        );
    }
}

/// Faz C lock 3 — OPPOSITE DIRECTIONS may run different packagings
/// (s0->s1 forced ALWAYS-FULL while s1->s0 runs DELTA): each receiver
/// applies its own inbound variant correctly and both views stay
/// exact. This per-link independence is what the mode derivation
/// relies on when future Ipc/Net links mix with local ones.
#[tokio::test]
async fn opposite_directions_run_different_packagings() {
    let mut r = BorderRig::with_mode(ExchangeMode::Delta);
    // Asymmetric forcing: s0's outbound slot runs ALWAYS-FULL while
    // s1's outbound slot runs DELTA.
    r.s0.force_exchange_modes(vec![ExchangeMode::AlwaysFull, ExchangeMode::AlwaysFull]);
    r.s1.force_exchange_modes(vec![ExchangeMode::Delta, ExchangeMode::Delta]);

    // Entities on BOTH sides, so both directions have content.
    put(&mut r.s0, 100, -1.0, 0.0);
    put(&mut r.s1, 200, 1.0, 0.0);

    let tinfo = |tick: u64| TickInfo {
        tick,
        at: Instant::now(),
    };

    // Tick 5: both shards step and bootstrap (needs_full ⇒ Full lead).
    r.step0(5);
    let _ = r.s1.step_phases(&tinfo(5));
    // Deliver each direction's exchanges so views establish BEFORE
    // the assertions: delivery feeds inboxes, processing happens on
    // the NEXT step.
    for m in r.drain01() {
        r.deliver_to_s1(vec![m]);
    }
    for m in {
        let mut v = Vec::new();
        while let Ok(m) = r.rx10.try_recv() {
            v.push(m);
        }
        v
    } {
        r.deliver_to_s0(vec![m]);
    }
    r.step0(6);
    let _ = r.s1.step_phases(&tinfo(6));
    assert_eq!(r.s1.border[&0].recs.len(), 1, "s1 sees s0's entity");
    assert_eq!(r.s0.border[&1].recs.len(), 1, "s0 sees s1's entity");

    // Move each entity; next ticks ship per-mode packaging.
    put(&mut r.s0, 100, -1.0, 2.0);
    put(&mut r.s1, 200, 1.0, 2.0);
    r.step0(7);
    let _ = r.s1.step_phases(&tinfo(7));
    let m01 = r.drain01();
    let mut m10: Vec<_> = Vec::new();
    while let Ok(m) = r.rx10.try_recv() {
        m10.push(m);
    }

    assert!(
        m01.iter().any(|m| matches!(
            m,
            ShardMsg::Border {
                exchange: BorderExchange::Full { .. },
                ..
            }
        )),
        "AlwaysFull direction keeps shipping fulls"
    );
    assert!(
        m10.iter().any(|m| matches!(
            m,
            ShardMsg::Border {
                exchange: BorderExchange::Delta { .. },
                ..
            }
        )),
        "Delta direction ships an upsert"
    );
    for m in m01 {
        r.deliver_to_s1(vec![m]);
    }
    for m in m10 {
        r.deliver_to_s0(vec![m]);
    }

    // Views converge to the moved positions.
    r.step0(8);
    let _ = r.s1.step_phases(&tinfo(8));
    assert_eq!(
        r.s0.border[&1].recs[&200].state.y, 2,
        "s0's borrowed view took s1's update"
    );
    assert_eq!(
        r.s1.border[&0].recs[&100].state.y, 2,
        "s1's borrowed view took s0's update"
    );
}

struct RichRig {
    s0: ShardActor<TWorld, (), TState, TRich>,
    s1: ShardActor<TWorld, (), TState, TRich>,
    /// What s0 exports to s1 lands here (test-held receiving end).
    rx01: Inbox<ShardMsg<TState, TRich>>,
    /// What s1 sends back lands here (never drained yet: no rich
    /// lock exercises the resync round trip; held so the channel
    /// stays open).
    #[allow(dead_code)]
    _rx10: Inbox<ShardMsg<TState, TRich>>,
}

impl RichRig {
    fn new() -> Self {
        let (tx01, rx01) = mpsc::channel(16);
        let (tx10, _rx10) = mpsc::channel(16);
        let build = |index: usize,
                     tx: Mailbox<ShardMsg<TState, TRich>>,
                     other: Mailbox<ShardMsg<TState, TRich>>| {
            let (_tick_tx, tick_rx) = broadcast::channel(64);
            let (_self_tx, rx) = channel::<ShardMsg<TState, TRich>>(16);
            ShardActor::new(
                RoomConfig {
                    id: RoomId(15),
                    keepalive_hz: 0.0,
                    metrics_cadence_hz: 0.0,
                    ..Default::default()
                },
                index,
                TWorld::default(),
                Box::new(RichLogic { index }),
                tick_rx,
                rx,
                vec![tx, other],
                1,
                metrics_null(),
                None, // no result sink
            )
        };
        let (d0, _d0rx) = mpsc::channel(1);
        let (d1, _d1rx) = mpsc::channel(1);
        let mut s0 = build(0, d0, tx01.clone());
        let mut s1 = build(1, tx10, d1);
        // The rich locks exercise DELTA packaging (upsert/exit
        // bookkeeping over the custom Strip fields) — force Delta on
        // the live directions (Faz C made local links default to
        // AlwaysFull).
        s0.force_exchange_modes(vec![ExchangeMode::AlwaysFull, ExchangeMode::Delta]);
        s1.force_exchange_modes(vec![ExchangeMode::Delta, ExchangeMode::AlwaysFull]);
        RichRig {
            s0,
            s1,
            rx01,
            _rx10,
        }
    }

    /// Run shard 0's phases at this tick index (its exports land in
    /// `rx01`).
    fn step0(&mut self, tick: u64) {
        assert!(self.s0.step_phases(&tinfo(tick)), "shard 0 keeps running");
    }

    /// Take everything shard 0 exported (WITHOUT delivering).
    fn drain01(&mut self) -> Vec<ShardMsg<TState, TRich>> {
        let mut out = Vec::new();
        while let Ok(m) = self.rx01.try_recv() {
            out.push(m);
        }
        out
    }

    /// Feed messages into shard 1's CONTROL handler.
    fn deliver_to_s1(&mut self, msgs: Vec<ShardMsg<TState, TRich>>) {
        for m in msgs {
            assert!(self.s1.handle_msg(m, 999), "s1 keeps running");
        }
    }
}

/// Seed a boundary entity straight into a shard's world (the third
/// tuple slot is the FACING source for [`RichLogic`]'s strip).
fn put_rich(a: &mut ShardActor<TWorld, (), TState, TRich>, wire: u64, x: f32, facing: i8) {
    a.world.ents.insert(wire, (x, 0.0, facing));
}

/// Rich lock 1 — a strip record whose payload has a field beyond
/// position arrives INTACT through both paths: the Full bootstrap on
/// first contact, and the Delta upsert after ONLY the custom field
/// changed. The receiving view (typed over the SAME logic-defined
/// payload) holds the exact values the sender's logic assembled.
#[tokio::test]
async fn rich_strip_record_survives_full_and_delta_paths() {
    let mut r = RichRig::new();

    // Bootstrap: first contact ships the whole strip as a Full, with
    // the custom field intact.
    put_rich(&mut r.s0, 100, -1.0, 7);
    r.step0(1);
    let msgs = r.drain01();
    let (seq, entities) = expect_full(&msgs);
    assert_eq!(
        entities,
        [BorderRecord {
            wire: 100,
            state: TRich {
                x: -1,
                y: 0,
                facing: 7
            }
        }],
        "the Full bootstrap carries the RICH record whole"
    );
    r.deliver_to_s1(msgs);
    assert_eq!(
        r.s1.border[&0].recs[&100].state,
        TRich {
            x: -1,
            y: 0,
            facing: 7
        },
        "the FULL path preserved every payload field"
    );
    assert_eq!(
        r.s1.border[&0].expected_seq,
        seq + 1,
        "receiver sequence re-baselined by the Full"
    );

    // Change ONLY the custom field (position untouched): the next
    // exchange is a delta whose upsert carries the new value intact.
    r.s0.world.ents.get_mut(&100).unwrap().2 = 9;
    r.step0(2);
    let msgs = r.drain01();
    let (_seq2, upserts, exits) = expect_delta(&msgs);
    assert_eq!(
        upserts,
        [BorderRecord {
            wire: 100,
            state: TRich {
                x: -1,
                y: 0,
                facing: 9
            }
        }],
        "the DELTA upsert carries the custom field"
    );
    assert!(exits.is_empty());
    r.deliver_to_s1(msgs);
    assert_eq!(
        r.s1.border[&0].recs[&100].state.facing, 9,
        "the DELTA path preserved the custom field end to end"
    );
    assert_eq!(
        (
            r.s1.border[&0].recs[&100].state.x,
            r.s1.border[&0].recs[&100].state.y
        ),
        (-1, 0),
        "position unchanged alongside it"
    );
}

/// Rich lock 2 — the delta diff keys off WHOLE-payload equality: a
/// change confined to the custom field fires an upsert, a tick with
/// no change of any field ships NOTHING (the silent-tick skip that
/// is the delta's entire point). A position-only diff would miss the
/// first half; an always-ship design would waste the second.
#[tokio::test]
async fn delta_diff_fires_on_custom_field_change() {
    let mut r = RichRig::new();

    // Bootstrap (Full) and settle the ledger.
    put_rich(&mut r.s0, 100, -1.0, 3);
    r.step0(1);
    {
        let msgs = r.drain01();
        r.deliver_to_s1(msgs);
    }

    // Quiet tick: no field changed ⇒ NOTHING ships.
    r.step0(2);
    assert!(r.drain01().is_empty(), "an unchanged strip ships nothing");

    // Change ONLY the custom field: the next tick ships exactly one
    // upsert, carrying the new facing at the unchanged position.
    r.s0.world.ents.get_mut(&100).unwrap().2 = 4;
    r.step0(3);
    let msgs = r.drain01();
    let (_seq, upserts, exits) = expect_delta(&msgs);
    assert_eq!(
        upserts,
        [BorderRecord {
            wire: 100,
            state: TRich {
                x: -1,
                y: 0,
                facing: 4
            }
        }],
        "a custom-field-only change fires the delta"
    );
    assert!(exits.is_empty(), "no exit: the entity never left");
    r.deliver_to_s1(msgs);
    assert_eq!(
        r.s1.border[&0].recs[&100].state.facing, 4,
        "the receiving view took the custom-field update"
    );

    // And quietness resumes once the change was accepted.
    r.step0(4);
    assert!(r.drain01().is_empty(), "no further change ⇒ silent again");
}
