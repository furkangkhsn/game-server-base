//! The logic's own counters (F9) at the room actor: the sample carries
//! what `GameLogic::logic_counters` puts, read after the step; a logic
//! that declares nothing sends an empty set; a logic over the bound has
//! its extra names dropped, counted and warned about once.

use super::*;
use crate::metrics::{LOGIC_COUNTERS_MAX, LogicCounter, LogicCounters};
use crate::room::actor::RoomActor;

const UPDATES: LogicCounter = LogicCounter::sum("updates", "Update calls, cumulative.");

/// A logic that counts its own `update` calls in a plain field, and
/// reports `extra` more names on top (to overflow the bound).
struct Counting {
    updates: u64,
    extra: usize,
}

impl GameLogic<()> for Counting {
    type GroupKey = ();
    type Strip = ();
    fn snapshot_op(&self) -> u16 {
        0x7400
    }
    fn private_op(&self) -> u16 {
        0x7401
    }
    fn group_of(&self, _w: &(), _p: PlayerId) -> Self::GroupKey {}
    fn snapshot(
        &mut self,
        _w: &mut (),
        _c: &TickCtx,
        _g: &Self::GroupKey,
        _borrowed: &[crate::shard::BorderRecord<()>],
        _o: &mut bytes::BytesMut,
    ) -> bool {
        false
    }
    fn on_join(&mut self, _w: &mut (), c: ConnectionId) -> Admission {
        Admission {
            player: PlayerId(c.0),
            entity: 1,
        }
    }
    fn on_leave(&mut self, _w: &mut (), _p: PlayerId) {}
    fn ingest(&mut self, _w: &mut (), _c: &TickCtx, a: &mut Vec<Action>) {
        a.clear();
    }
    fn update(&mut self, _w: &mut (), _c: &TickCtx) {
        self.updates += 1;
    }
    fn logic_counters(&self, _w: &(), out: &mut LogicCounters) {
        out.put(&UPDATES, self.updates);
        for i in 0..self.extra {
            out.put(&LogicCounter::sum(&format!("extra_{i}"), ""), 1);
        }
    }
}

impl RoomLogic<()> for Counting {}

/// A room sampling every step into a channel the test reads.
fn rig(logic: Box<dyn RoomLogic<(), GroupKey = (), Strip = ()>>) -> Rig {
    let (_tick_tx, tick_rx) = broadcast::channel(16);
    let (control, control_rx) = channel(16);
    let (metrics_tx, metrics) = mpsc::channel(64);
    let actor = RoomActor::new(
        RoomConfig {
            id: RoomId(41),
            tick_hz: 30.0,
            keepalive_hz: 0.0,
            metrics_cadence_hz: 30.0,
            ..Default::default()
        },
        (),
        logic,
        tick_rx,
        control_rx,
        1,
        metrics_tx,
        None,
    );
    Rig {
        actor,
        metrics,
        _control: control,
    }
}

struct Rig {
    actor: RoomActor<(), (), ()>,
    metrics: mpsc::Receiver<MetricsEvent>,
    _control: Mailbox<RoomControl>,
}

impl Rig {
    /// Step `n` times; the last sample sent.
    fn run(&mut self, n: u64) -> crate::metrics::RoomSample {
        for t in 1..=n {
            assert!(self.actor.step(&TickInfo {
                tick: t,
                at: Instant::now(),
            }));
        }
        let mut last = None;
        while let Ok(ev) = self.metrics.try_recv() {
            if let MetricsEvent::Room(s) = ev {
                last = Some(s);
            }
        }
        last.expect("the room sampled")
    }
}

#[tokio::test]
async fn the_sample_carries_the_logics_counters_after_the_step() {
    let mut r = rig(Box::new(Counting {
        updates: 0,
        extra: 0,
    }));
    let s = r.run(3);
    assert_eq!(
        s.logic.get("updates"),
        Some(3),
        "read after the third update"
    );
    assert_eq!(s.logic.slots().len(), 1);
    assert_eq!(
        s.logic.slots()[0].counter.help(),
        "Update calls, cumulative."
    );
}

#[tokio::test]
async fn a_logic_that_declares_nothing_sends_an_empty_set() {
    let (dts, _d) = mpsc::channel(64);
    let (ops, _o) = mpsc::channel(64);
    let mut r = rig(Box::new(RecLogic { dts, ops }));
    assert_eq!(r.run(2).logic, LogicCounters::new());
}

#[tokio::test]
async fn names_over_the_bound_are_dropped_counted_and_warned_once() {
    let (warn_tx, mut warns) = mpsc::channel::<String>(64);
    let mut r = rig(Box::new(Counting {
        updates: 0,
        extra: LOGIC_COUNTERS_MAX,
    }));
    let s = tracing::subscriber::with_default(WarnCapture { tx: warn_tx }, || r.run(3));
    assert_eq!(s.logic.slots().len(), LOGIC_COUNTERS_MAX);
    assert_eq!(s.logic.dropped(), 1, "one name over, in this sample");
    assert_eq!(s.logic.get("updates"), Some(3), "the first names are kept");
    let w = warns.try_recv().expect("a warning");
    assert!(w.contains("more counters than a sample holds"), "{w}");
    assert!(warns.try_recv().is_err(), "once, not every sample");
}
