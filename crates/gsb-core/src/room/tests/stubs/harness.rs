//! The rigs that drive a real room actor off a manual ticker feed,
//! using the metrics channel as a per-step barrier.

use super::*;

pub(in crate::room::tests) struct Harness {
    pub(in crate::room::tests) tick_tx: broadcast::Sender<TickInfo>,
    pub(in crate::room::tests) control: Mailbox<RoomControl>,
    pub(in crate::room::tests) handle: tokio::task::JoinHandle<()>,
    pub(in crate::room::tests) t0: Instant,
    pub(in crate::room::tests) next_tick: u64,
    pub(in crate::room::tests) run_every: u64,
}

impl Harness {
    pub(in crate::room::tests) fn new(run_every: u64, config: RoomConfig, logic: RecLogic) -> Self {
        let (tick_tx, _first) = broadcast::channel(64);
        let tick_rx = tick_tx.subscribe();
        let (control, control_rx) = channel(config.control_capacity);
        let actor = RoomActor::new(
            config,
            (),
            Box::new(logic),
            tick_rx,
            control_rx,
            run_every,
            null_metrics_tx(),
            None,
        );
        Self {
            tick_tx,
            control,
            handle: tokio::spawn(actor.run()),
            t0: Instant::now(),
            next_tick: 0,
            run_every: run_every.max(1),
        }
    }

    /// Send the next global tick with an exact synthetic timestamp:
    /// `at = t0 + n * period`, so dts are deterministic.
    pub(in crate::room::tests) fn tick(&mut self, period: Duration) {
        self.next_tick += 1;
        let at =
            self.t0 + Duration::from_secs_f64(self.next_tick as f64 * period.as_secs_f64());
        self.tick_tx
            .send(TickInfo {
                tick: self.next_tick,
                at,
            })
            .expect("room subscriber alive");
    }

    pub(in crate::room::tests) async fn join(
        &mut self,
        conn: ConnectionId,
        ticks_needed: u64,
    ) -> (EntityId, Mailbox<Action>) {
        let (out_tx, _out_rx) = mpsc::channel::<FrameBatch>(8);
        let (reply_tx, reply_rx) =
            oneshot::channel::<Result<(EntityId, Mailbox<Action>), CoreError>>();
        self.control
            .send(RoomControl::Join {
                conn,
                out: out_tx,
                reply: reply_tx,
            })
            .await
            .expect("control alive");
        // Control is processed on the room's next *step*; feed enough
        // ticks to guarantee one (run_every + slack).
        for _ in 0..ticks_needed {
            self.tick(Duration::from_secs_f64(1.0 / 30.0));
        }
        let (entity, actions) = tokio::time::timeout(Duration::from_secs(2), reply_rx)
            .await
            .expect("timed out waiting for join reply")
            .expect("join reply dropped")
            .expect("join accepted (room not full)");
        (entity, actions)
    }

    pub(in crate::room::tests) async fn shutdown(mut self) {
        self.control
            .send(RoomControl::Shutdown)
            .await
            .expect("control alive");
        // Control is processed on the room's next step: feed enough
        // ticks to guarantee one.
        for _ in 0..self.run_every {
            self.tick(Duration::from_secs_f64(1.0 / 30.0));
        }
        tokio::time::timeout(Duration::from_secs(2), &mut self.handle)
            .await
            .expect("room did not shut down")
            .expect("room task panicked");
    }
}
