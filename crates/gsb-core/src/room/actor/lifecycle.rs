//! Birth, the tick loop (whose only await is the ticker receive), the
//! measured step, and the metrics sample.

use crate::channel::{Inbox, Mailbox};
use crate::metrics::MetricsEvent;
use crate::room::*;
use crate::ticker::TickInfo;
use std::collections::HashMap;
use std::fmt::Debug;
use std::hash::Hash;
use std::time::Instant;
use tokio::sync::{broadcast, mpsc};
use tracing::{debug, warn};

use crate::room::actor::RoomActor;

mod sample;

impl<W, G, Sp> RoomActor<W, G, Sp>
where
    G: Eq + Hash + Clone + Debug,
    // The strip payload's trait bounds (`GameLogic::Strip`) — restated so
    // calls through the logic object type-check; the room itself never
    // touches the payloads.
    Sp: Debug + Clone + PartialEq + Send + 'static,
{
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        config: RoomConfig,
        world: W,
        logic: Box<dyn RoomLogic<W, GroupKey = G, Strip = Sp>>,
        tick_rx: broadcast::Receiver<TickInfo>,
        control_rx: Inbox<RoomControl>,
        run_every: u64,
        // Outbound metrics path (see `crate::metrics`): a bounded channel;
        // the room sends with the synchronous `try_send` (no await). A
        // dropped receiver makes the send fail (ignored) — the room does not
        // observe it.
        metrics: mpsc::Sender<MetricsEvent>,
        // The room's match-result sink (the control plane's result seam —
        // see `RoomLogic::match_result`): a bounded mailbox; `None` = the
        // room reports no result.
        result_sink: Option<Mailbox<crate::registry::MatchResult>>,
    ) -> Self {
        // The completion channel: bounded at the room-wide pending cap (a
        // completion burst cannot exceed the number of in-flight workers,
        // which is the cap) — the usual backpressure rule; the room drains
        // it every tick's CONTROL phase, so a full channel only parks a
        // worker until the next tick, never the room.
        let (completions_tx, completions) = mpsc::channel(config.max_pending_requests.max(1));
        let keepalive_every = if config.keepalive_hz > 0.0 {
            if config.keepalive_hz > config.tick_hz {
                // The registry rejects this relationship at room creation;
                // this warn covers direct construction (library use) that
                // bypasses it, so the misconfiguration can never be silent:
                // the cadence clamps to every step, and the "silence when
                // unchanged" gain for this room is gone.
                warn!(
                    room = %config.id,
                    keepalive_hz = config.keepalive_hz,
                    tick_hz = config.tick_hz,
                    "keepalive_hz exceeds tick_hz: keep-alive clamps to every \
                     step (unchanged groups re-send on every step and the \
                     silence gain is lost); set keepalive_hz <= tick_hz"
                );
            }
            Some(((config.tick_hz / config.keepalive_hz).round() as u64).max(1))
        } else {
            None
        };
        // A2: sample at most every `metrics_every` steps, so the send cadence
        // tracks the collector's report cadence. `> tick_hz` (or `<= 0`)
        // clamps to every step.
        let metrics_every = if config.metrics_cadence_hz > 0.0 {
            ((config.tick_hz / config.metrics_cadence_hz).round() as u64).max(1)
        } else {
            1
        };
        let budget_us = config.period().as_micros() as u64;
        Self {
            config,
            world,
            logic,
            tick_rx,
            control_rx,
            conns: HashMap::new(),
            binding: HashMap::new(),
            roster: Vec::new(),
            roster_pos: HashMap::new(),
            read_cursor: 0,
            idle: IdleClock::default(),
            idle_ceiling_warns: 0,
            detach_ceiling_warns: 0,
            groups: HashMap::new(),
            run_every: run_every.max(1),
            last_at: None,
            steps: 0,
            keepalive_every,
            metrics_every,
            budget_us,
            m: RoomCounters::default(),
            metrics,
            pending: HashMap::new(),
            pending_total: 0,
            queued: HashMap::new(),
            replies_buf: Vec::new(),
            completions,
            completions_tx,
            result_sink,
            registry: None,
            despawn_reports: Vec::new(),
        }
    }

    /// Give the room the registry mailbox it reports detach-despawns on
    /// (see [`crate::registry::RegistryMsg::DetachDespawned`]). A builder
    /// instead of another `new` parameter: every direct-drive harness
    /// constructs rooms without a registry, and `new` is already at the
    /// argument limit.
    pub fn with_registry(mut self, registry: Mailbox<crate::registry::RegistryMsg>) -> Self {
        self.registry = Some(registry);
        self
    }

    /// Run until the ticker channel closes or a control `Shutdown` is
    /// processed on a tick.
    pub async fn run(mut self) {
        debug!(
            room = %self.config.id,
            hz = self.config.tick_hz,
            run_every = self.run_every,
            "room actor started"
        );
        loop {
            let t = match self.tick_rx.recv().await {
                Ok(t) => t,
                // We fell behind by more than the broadcast buffer: skip
                // these; the wall-clock dt of the next step covers the gap
                // (bounded by the catch-up cap). Counted for metrics:
                // `lagged_*` is the room's "missed ticks" signal.
                Err(broadcast::error::RecvError::Lagged(missed)) => {
                    self.m.lagged_events += 1;
                    self.m.lagged_ticks += missed;
                    warn!(
                        room = %self.config.id,
                        missed,
                        "lagged behind global ticker; next step catches up via dt"
                    );
                    continue;
                }
                // Ticker aborted: global stop signal.
                Err(broadcast::error::RecvError::Closed) => break,
            };
            if t.tick % self.run_every != 0 {
                continue; // this room runs slower: every k-th global tick
            }
            if !self.step(&t) {
                break;
            }
        }
        self.logic.on_shutdown();
        // The match-result seam (control plane): the logic computes the
        // final result from the world (still alive — it is dropped only
        // when `self` drops, below) and the room reports it to the sink
        // with the synchronous `try_send` (no await: the room's only
        // await stayed `tick_rx.recv()`). Best effort — a full or gone
        // sink drops the result and warns (a slow result consumer must
        // not stall the room's teardown).
        if let Some(result) = self.logic.match_result(&mut self.world)
            && let Some(sink) = &self.result_sink
        {
            match sink.try_send(crate::registry::MatchResult {
                room: self.config.id,
                payload: result,
            }) {
                Ok(()) => debug!(room = %self.config.id, "match result reported"),
                Err(mpsc::error::TrySendError::Full(_)) => {
                    warn!(room = %self.config.id, "match result dropped: sink full");
                }
                Err(mpsc::error::TrySendError::Closed(_)) => {
                    debug!(room = %self.config.id, "match result dropped: sink gone");
                }
            }
        }
        debug!(
            room = %self.config.id,
            dropped_frames = self.m.dropped_frames,
            "room actor stopped"
        );
    }

    /// One full step: control → read → convert → systems → broadcast,
    /// measured (tick latency + step body duration) and flushed to the
    /// metrics channel once at the end. Synchronous: the send is a
    /// bounded-channel `try_send` (drops counted, never parks), so the
    /// room's only await stays `tick_rx.recv()`. Returns `false` when the
    /// actor should stop.
    pub(in crate::room) fn step(&mut self, t: &TickInfo) -> bool {
        self.steps += 1;

        // -- tick latency: how late this room processes the tick (step
        //    start minus the ticker's timestamp; covers broadcast delivery
        //    + the room's queue behind the ticker).
        // On the stamp's own clock (`ticker::now`): the tick clock.
        let late_us = crate::ticker::now()
            .saturating_duration_since(t.at)
            .as_micros() as u64;
        self.m.observe_late_us(self.steps, late_us);

        let t0 = Instant::now();
        let keep = self.step_phases(t);
        let step_us = t0.elapsed().as_micros() as u64;
        // Extremes, sum and both histograms in one place, shared with the
        // shard actor (see `RoomCounters::observe_step_us`): integer only,
        // no allocation, no await — the tick body stays synchronous.
        self.m.observe_step_us(self.steps, self.budget_us, step_us);

        // A2: emit a sample at most every `metrics_every` steps (the send
        // cadence tracks the collector's report cadence; the counters are
        // cumulative, so skipping in between loses nothing). A3: bounded
        // channel + synchronous `try_send` — a full channel drops this
        // sample (harmless: the next sample carries everything) and counts
        // it; a closed channel just fails silently. No await either way, so
        // the tick body stays synchronous.
        if self.steps.is_multiple_of(self.metrics_every)
            && let Err(mpsc::error::TrySendError::Full(_)) =
                self.metrics.try_send(MetricsEvent::Room(self.sample()))
        {
            self.m.metrics_dropped += 1;
        }
        keep
    }
}
