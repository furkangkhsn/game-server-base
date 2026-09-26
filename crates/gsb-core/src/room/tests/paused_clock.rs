//! A room on the live ticker under tokio's PAUSED clock (BACKLOG F10):
//! the game's `dt` is the paused time between two ticks, so an entity
//! walking at a fixed speed covers exactly speed × elapsed — in
//! microseconds of real time. The ticker used to stamp ticks with the
//! wall clock (`std::time::Instant`): under the paused clock the ticks
//! came back to back and `dt` was microseconds, a runner stood still.

use super::*;
use crate::ticker::Ticker;

/// Metres per second of the walker.
const SPEED: f64 = 7.0;

/// One entity walking in +x at [`SPEED`]: `update` integrates the tick's
/// `dt` and reports the position.
struct Walker {
    x: f64,
    report: mpsc::Sender<f64>,
}

impl GameLogic<()> for Walker {
    type GroupKey = ();
    type Strip = ();

    fn snapshot_op(&self) -> u16 {
        0x7020
    }
    fn private_op(&self) -> u16 {
        0x7021
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
    fn ingest(&mut self, _w: &mut (), _c: &TickCtx, actions: &mut Vec<Action>) {
        actions.clear();
    }
    fn update(&mut self, _w: &mut (), ctx: &TickCtx) {
        self.x += SPEED * ctx.dt.as_secs_f64();
        let _ = self.report.try_send(self.x);
    }
}

impl RoomLogic<()> for Walker {}

#[tokio::test(start_paused = true)]
async fn a_walker_covers_its_distance_on_the_paused_clock() {
    let (ticker, _task) = Ticker::spawn(30.0, 64).expect("rate");
    let (report, mut positions) = mpsc::channel::<f64>(256);
    let config = RoomConfig {
        id: RoomId(90),
        tick_hz: 30.0,
        ..Default::default()
    };
    let (_control, control_rx) = channel(config.control_capacity);
    let actor = RoomActor::new(
        config,
        (),
        Box::new(Walker { x: 0.0, report }),
        ticker.subscribe(),
        control_rx,
        1,
        null_metrics_tx(),
        None,
    );
    tokio::spawn(actor.run());

    let real = std::time::Instant::now();
    let paused = tokio::time::Instant::now();
    let mut x = 0.0;
    for _ in 0..60 {
        x = positions.recv().await.expect("the room steps");
    }
    let elapsed = paused.elapsed().as_secs_f64();
    // 60 steps at 30 Hz: two seconds of paused time, ~14 m. The first
    // step's `dt` is one nominal period (no previous stamp), so the walk
    // equals speed × (elapsed time), within one period's worth.
    assert!(
        (1.9..=2.1).contains(&elapsed),
        "the paused clock ran {elapsed} s"
    );
    let expected = SPEED * elapsed;
    assert!(
        (x - expected).abs() <= SPEED / 30.0 + 1e-6,
        "walked {x} m, expected {expected} m"
    );
    assert!(
        real.elapsed() < Duration::from_secs(1),
        "virtual time: two seconds of walking took {:?} of real time",
        real.elapsed()
    );
}
