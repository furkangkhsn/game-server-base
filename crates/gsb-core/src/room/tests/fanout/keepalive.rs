//! Keep-alive: an unchanged group stays silent until its keep-alive
//! full, a keep-alive above the tick rate warns and clamps, and a
//! group that never emits warns exactly once naming itself.

use super::*;
use crate::room::tests::stubs::WarnCapture;

/// Contract-violating logic: `snapshot` returns `false` on *every*
/// tick, including a fresh group's first tick.
struct SilentLogic;

impl GameLogic<()> for SilentLogic {
    type GroupKey = PlayerId;
    type Strip = ();

    fn snapshot_op(&self) -> u16 {
        0x7030
    }
    fn private_op(&self) -> u16 {
        0x7031
    }

    fn group_of(&self, _w: &(), player: PlayerId) -> Self::GroupKey {
        player
    }

    fn snapshot(
        &mut self,
        _w: &mut (),
        _c: &TickCtx,
        _g: &Self::GroupKey,
        _borrowed: &[crate::shard::BorderRecord<()>],
        _o: &mut bytes::BytesMut,
    ) -> bool {
        false // even the first tick: a contract violation
    }

    fn on_join(&mut self, _w: &mut (), conn: ConnectionId) -> Admission {
        Admission {
            player: PlayerId(conn.0),
            entity: 1,
        }
    }
    fn on_leave(&mut self, _w: &mut (), _player: PlayerId) {}
    fn ingest(&mut self, _w: &mut (), _c: &TickCtx, actions: &mut Vec<Action>) {
        actions.clear();
    }
    fn update(&mut self, _w: &mut (), _c: &TickCtx) {}
}

// Faz 1 trait split: shared contract on `GameLogic`; no room-exclusive
// hook used (empty `RoomLogic` impl).
impl RoomLogic<()> for SilentLogic {}





#[tokio::test]
async fn never_emitted_group_warns_once_naming_the_group() {
    // Only this test sets the process-global default; the other tests
    // in this binary neither set it nor assert on logging.
    let (warn_tx, mut warns) = mpsc::channel::<String>(64);
    tracing::subscriber::set_global_default(WarnCapture { tx: warn_tx })
        .expect("only this test sets the global default");

    let (tick_tx, tick_rx) = broadcast::channel(64);
    let (control, control_rx) = channel(16);
    let actor = RoomActor::new(
        RoomConfig {
            id: RoomId(9),
            ..Default::default()
        },
        (),
        Box::new(SilentLogic),
        tick_rx,
        control_rx,
        1,
        null_metrics_tx(),
            None,
    );
    let handle = tokio::spawn(actor.run());
    let t0 = Instant::now();

    // Tick 1: the join is processed, the group is created (Vacant —
    // no check yet) and its first `snapshot()` returns `false`.
    let (out_tx, _out_rx) = mpsc::channel::<FrameBatch>(8);
    let (reply_tx, reply_rx) =
        oneshot::channel::<Result<(EntityId, Mailbox<Action>), CoreError>>();
    control
        .send(RoomControl::Join {
            conn: ConnectionId(42),
            out: out_tx,
            reply: reply_tx,
        })
        .await
        .expect("control alive");
    tick_tx
        .send(TickInfo {
            tick: 1,
            at: t0 + Duration::from_secs_f64(1.0 / 30.0),
        })
        .expect("room subscriber alive");
    let _ = reply_rx
        .await
        .expect("join reply dropped")
        .expect("join accepted (room not full)");
    tokio::time::sleep(Duration::from_millis(20)).await;
    // No diagnostic for THIS group on its own first tick (the check
    // only sees a group that existed on the previous tick). Other
    // tests in this binary share the global default and may emit
    // their own warns (e.g. RecLogic, which never emits) — only lines
    // naming our group are ours.
    while let Ok(line) = warns.try_recv() {
        assert!(
            !line.contains("PlayerId(42)"),
            "no diagnostic on the group's own first tick: {line}"
        );
    }

    // Ticks 2..=4: the group is Occupied with `last = None` — the
    // diagnostic must fire on tick 2 and (flag) not repeat.
    for n in 2u64..=4 {
        tick_tx
            .send(TickInfo {
                tick: n,
                at: t0 + Duration::from_secs_f64(n as f64 / 30.0),
            })
            .expect("room subscriber alive");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    drop(tick_tx); // ticker closed → the room exits cleanly
    tokio::time::timeout(Duration::from_secs(2), handle)
        .await
        .expect("room did not exit on closed ticker")
        .expect("room task panicked");

    let mut mine = Vec::new();
    while let Ok(line) = warns.try_recv() {
        if line.contains("PlayerId(42)") {
            mine.push(line);
        }
    }
    assert_eq!(mine.len(), 1, "warn fires exactly once: {mine:?}");
    assert!(
        mine[0].contains("group_key=PlayerId(42)"),
        "warn must name the group: {}",
        mine[0]
    );
    assert!(
        mine[0].contains("members=1"),
        "warn must report the member count: {}",
        mine[0]
    );
}
