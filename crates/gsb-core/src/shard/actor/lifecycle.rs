//! Birth, the tick loop (its only await is the ticker receive), the
//! measured step, and the metrics sample.

use std::collections::{HashMap, VecDeque};
use std::fmt::Debug;
use std::hash::Hash;
use std::time::Instant;

use tokio::sync::{broadcast, mpsc};
use tracing::{debug, info, warn};

use crate::channel::{Inbox, Mailbox};
use crate::metrics::MetricsEvent;
use crate::room::{RoomConfig, RoomCounters};
use crate::ticker::TickInfo;

use crate::shard::actor::ShardActor;
use crate::shard::*;

mod sample;

impl<W, G, St, Sp> ShardActor<W, G, St, Sp>
where
    W: Send + 'static,
    G: Eq + Hash + Clone + Debug + Send + 'static,
    St: Debug + Send + 'static,
    // The strip rides every exchange and view; the bounds mirror what
    // the delta protocol does with it (diff via PartialEq, clone into
    // each neighbor's message, store in the actor's maps).
    Sp: Debug + Clone + PartialEq + Send + 'static,
{
    /// Build a shard actor. `neighbors` is indexed by shard index (the
    /// unused slots may be any closed/unused mailbox — only the
    /// `ShardLogic::neighbors()` slots are sent to); each entry is
    /// wrapped into an in-process `ShardLink` here, so the registry's
    /// wiring shape is unchanged. `result_sink` is the logical room's
    /// match-result sink shared by all its shards (`None` = this shard
    /// reports no result).
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        config: RoomConfig,
        index: usize,
        world: W,
        logic: Box<dyn ShardLogic<W, GroupKey = G, State = St, Strip = Sp>>,
        tick_rx: broadcast::Receiver<TickInfo>,
        shard_rx: Inbox<ShardMsg<St, Sp>>,
        neighbors: Vec<Mailbox<ShardMsg<St, Sp>>>,
        run_every: u64,
        metrics: mpsc::Sender<MetricsEvent>,
        result_sink: Option<Mailbox<crate::registry::MatchResult>>,
    ) -> Self {
        let keepalive_every = if config.keepalive_hz > 0.0 {
            if config.keepalive_hz > config.tick_hz {
                warn!(
                    room = %config.id,
                    shard = index,
                    "keepalive_hz exceeds tick_hz: keep-alive clamps to \
                     every step (the silence gain is lost); set \
                     keepalive_hz <= tick_hz"
                );
            }
            Some(((config.tick_hz / config.keepalive_hz).round() as u64).max(1))
        } else {
            None
        };
        let metrics_every = if config.metrics_cadence_hz > 0.0 {
            ((config.tick_hz / config.metrics_cadence_hz).round() as u64).max(1)
        } else {
            1
        };
        let budget_us = config.period().as_micros() as u64;
        // measurement scaffolding for CROSS-SHARD §7: the border summary
        // window ≈ one second of ticks.
        let border_every = (config.tick_hz.round() as u64).max(1);
        // The completion channel: bounded at the shard-wide pending cap
        // (the room actor's rule — a completion burst cannot exceed the
        // number of in-flight workers, which the cap bounds); drained
        // every tick's 0b phase, so a full channel only parks a worker
        // until the next tick, never the shard.
        let (completions_tx, completions) = mpsc::channel(config.max_pending_requests.max(1));
        let effects = EffectBook::new(index, logic.shard_count());
        Self {
            config,
            index,
            world,
            logic,
            tick_rx,
            inbox: Box::new(InProcLink::inbound(shard_rx)),
            conns: HashMap::new(),
            binding: HashMap::new(),
            conn_epoch: HashMap::new(),
            conn_tombstone: HashMap::new(),
            last_tombstone_sweep: None,
            idle: crate::room::IdleClock::default(),
            idle_ceiling_warns: 0,
            detach_ceiling_warns: 0,
            groups: HashMap::new(),
            links: neighbors
                .into_iter()
                .map(InProcLink::outbound)
                .map(|l| Box::new(l) as Box<dyn ShardLink<St, Sp>>)
                .collect(),
            exchange_override: None,
            border: HashMap::new(),
            lenders: Vec::new(),
            effects,
            export: HashMap::new(),
            pending_out: Vec::new(),
            deferred: VecDeque::new(),
            run_every: run_every.max(1),
            last_at: None,
            steps: 0,
            keepalive_every,
            metrics_every,
            budget_us,
            m: RoomCounters::default(),
            bstats: BorderStats::default(),
            border_every,
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
            teams: TeamImports::default(),
            team_sent: false,
            tstats: TeamStats::default(),
            tstats_logged: TeamStats::default(),
        }
    }

    /// Give this shard the registry mailbox it reports detach-despawns on
    /// (see [`crate::registry::RegistryMsg::DetachDespawned`]). A builder for
    /// the same reason the room actor uses one: the direct-drive rigs
    /// construct shards without a registry, and `new` is already at the
    /// argument limit.
    pub fn with_registry(mut self, registry: Mailbox<crate::registry::RegistryMsg>) -> Self {
        self.registry = Some(registry);
        self
    }

    /// Stamp this shard's remote effects with the room incarnation
    /// `epoch` (the registry's install generation — every shard of one
    /// install gets the same). Effects of another epoch are refused on
    /// arrival. A builder like [`Self::with_registry`]: the direct-drive
    /// rigs keep epoch 0.
    pub fn with_effect_epoch(mut self, epoch: u64) -> Self {
        self.effects.out.epoch = epoch;
        self
    }

    /// Run until the ticker channel closes or a `Shutdown` is processed on
    /// a tick (same lifecycle discipline as the room actor).
    pub async fn run(mut self) {
        debug!(
            room = %self.config.id,
            shard = self.index,
            hz = self.config.tick_hz,
            "shard actor started"
        );
        loop {
            let t = match self.tick_rx.recv().await {
                Ok(t) => t,
                Err(broadcast::error::RecvError::Lagged(missed)) => {
                    self.m.lagged_events += 1;
                    self.m.lagged_ticks += missed;
                    warn!(
                        room = %self.config.id,
                        shard = self.index,
                        missed,
                        "shard lagged behind global ticker; next step \
                         catches up via dt (and its boundary exports / \
                         migrations are one tick late — the accepted \
                         degradation, see the module docs)"
                    );
                    continue;
                }
                Err(broadcast::error::RecvError::Closed) => break,
            };
            if t.tick % self.run_every != 0 {
                continue;
            }
            if !self.step(&t) {
                break;
            }
        }
        self.logic.on_shutdown();
        // The match-result seam (the Faz 3 promotion; see the module docs,
        // "Shard-RPC and match-result"): THIS shard reports ITS final state
        // through the shared sink under the LOGICAL room id. One logical
        // room therefore yields one payload PER SHARD (the platform's
        // adapter concatenates/filters; nothing was added to
        // `MatchResult`). Same best-effort discipline as the room actor:
        // a full or gone sink drops the result and warns/debugs — a slow
        // consumer must not stall the shard's teardown, and the shard's
        // only await stays `tick_rx.recv()`.
        if let Some(result) = self.logic.match_result(&mut self.world)
            && let Some(sink) = &self.result_sink
        {
            match sink.try_send(crate::registry::MatchResult {
                room: self.config.id,
                payload: result,
            }) {
                Ok(()) => debug!(
                    room = %self.config.id,
                    shard = self.index,
                    "shard match result reported"
                ),
                Err(mpsc::error::TrySendError::Full(_)) => warn!(
                    room = %self.config.id,
                    shard = self.index,
                    "match result dropped: sink full"
                ),
                Err(mpsc::error::TrySendError::Closed(_)) => debug!(
                    room = %self.config.id,
                    shard = self.index,
                    "match result dropped: sink gone"
                ),
            }
        }
        debug!(
            room = %self.config.id,
            shard = self.index,
            members = self.conns.len(),
            "shard actor stopped"
        );
    }

    /// One full step: the six phases (see module docs), measured and
    /// flushed to the metrics channel once at the end. Synchronous — the
    /// shard's only await stays `tick_rx.recv()`. Returns `false` when the
    /// actor should stop.
    pub(crate) fn step(&mut self, t: &TickInfo) -> bool {
        self.steps += 1;

        // -- tick latency (same measurement as the room actor).
        // On the stamp's own clock (`ticker::now`): the tick clock.
        let late_us = crate::ticker::now()
            .saturating_duration_since(t.at)
            .as_micros() as u64;
        self.m.observe_late_us(self.steps, late_us);

        let t0 = Instant::now();
        let keep = self.step_phases(t);
        let step_us = t0.elapsed().as_micros() as u64;
        // The same accounting the room actor runs — literally the same
        // code now (`RoomCounters::observe_step_us`) rather than a
        // hand-kept mirror of it: extremes, sum, and both histograms from
        // this one `step_us`.
        self.m.observe_step_us(self.steps, self.budget_us, step_us);

        if self.steps.is_multiple_of(self.metrics_every) {
            let sample = self.sample();
            self.m.note_logic(sample.room, &sample.logic);
            if let Err(mpsc::error::TrySendError::Full(_)) =
                self.metrics.try_send(MetricsEvent::Room(sample))
            {
                self.m.metrics_dropped += 1;
            }
        }

        // measurement scaffolding for CROSS-SHARD §7 — remove or promote
        // after the delta decision: one summary line per ~1 s of steps
        // (WINDOW deltas — the counters reset here), emitted only when
        // this shard actually exchanged something in the window, so a
        // quiet shard logs nothing and the steady state reads as clean
        // per-second rates.
        if self.steps.is_multiple_of(self.border_every) && !self.logic.neighbors().is_empty() {
            let s = std::mem::take(&mut self.bstats);
            if s.exports > 0 || s.imports > 0 {
                info!(
                    room = %self.config.id,
                    shard = self.index,
                    window_ticks = self.border_every,
                    exports = s.exports,
                    export_records = s.export_records,
                    export_bytes = s.export_bytes,
                    export_records_max = s.export_records_max,
                    export_us = s.export_us,
                    export_drops = s.export_drops,
                    imports = s.imports,
                    import_records = s.import_records,
                    import_bytes = s.import_bytes,
                    import_us = s.import_us,
                    // Delta-era additions (§6.4): the exchange mix, the
                    // resync traffic and the full-equivalent byte context.
                    delta_exchanges = s.delta_exchanges,
                    full_exchanges = s.full_exchanges,
                    full_resyncs_served = s.full_resyncs_served,
                    resync_requests_sent = s.resync_requests_sent,
                    delta_drops = s.delta_drops,
                    equiv_full_bytes = s.equiv_full_bytes,
                    "border_exchange_summary"
                );
            }
            self.log_effect_summary();
        }
        // The team exchange's window — on the same ~1 s cadence, only when
        // something moved (the metrics sample carries the cumulative
        // counters, W2; §8b.6 had the line alone).
        let window = self
            .steps
            .is_multiple_of(self.border_every)
            .then(|| self.tstats.since(&self.tstats_logged));
        if let Some(s) = window.filter(TeamStats::any) {
            self.tstats_logged = self.tstats.clone();
            info!(
                room = %self.config.id,
                shard = self.index,
                window_ticks = self.border_every,
                exports = s.exports,
                export_drops = s.export_drops,
                export_records = s.export_records,
                over_cap = s.over_cap,
                over_budget = s.over_budget,
                imports = s.imports,
                import_records = s.import_records,
                expired = s.expired,
                "team_exchange_summary"
            );
        }
        keep
    }
}
