//! The room's configuration: rates, capacities, and the derived tick
//! period every phase budget is measured against.

use crate::id::RoomId;
use std::fmt::Debug;
use std::time::Duration;

/// Static configuration for a room.
///
/// `PartialEq` (not just `Debug + Clone`): the control plane's
/// idempotent create compares the requested config with the existing
/// room's config (identical request = no-op success, different request
/// = conflict — see `RegistryMsg::CreateRoom`).
#[derive(Debug, Clone, PartialEq)]
pub struct RoomConfig {
    pub id: RoomId,
    /// Simulation rate in ticks per second. Must divide the global ticker
    /// rate: the room steps on every k-th global tick.
    pub tick_hz: f64,
    /// Capacity of the control channel (join/leave/shutdown).
    pub control_capacity: usize,
    /// Capacity of each connection's action channel.
    pub action_capacity: usize,
    /// Per-connection per-tick pull budget (fairness cut, READ phase): a
    /// single connection's input can contribute at most this many actions
    /// to one tick, no matter how much its bounded channel is holding.
    /// The excess *stays in the channel* — it is pulled on later ticks
    /// (deferred, not dropped by the room); if a connection outpaces the
    /// budget sustainedly, its own `try_send` hits the full channel and
    /// drops *its own* newest input (counted by the connection actor,
    /// attributed to it). This is what stops one flooding connection from
    /// pushing *other* connections' earlier input out of a tick.
    ///
    /// Default 16: 480 actions/s at the default 30 Hz — ~70× the measured
    /// demo input rate (one MOVE_TO per 150 ms ≈ 6.7/s, the load test's
    /// steady state at the 10k-connection wall) — while cutting a
    /// flooder's per-tick contribution from the channel cap (256) to 16.
    pub max_actions_per_conn_per_tick: usize,
    /// Room-level pull budget (READ phase): the total number of actions the
    /// room pulls in one tick. This is a *pull* bound, not a drop bound —
    /// when it is exhausted the room simply pulls no more this tick and the
    /// remainder waits in the senders' bounded channels (the room never
    /// drops an action — which is why there is no room-scope drop counter;
    /// the architecture's only input-loss point is a connection's own full
    /// action channel, counted there as
    /// [`crate::metrics::ConnSample::actions_dropped`]). It bounds
    /// the tick's ingest cost, which is what the 10k-connection load wall
    /// measured (the room's serial step path, not its memory).
    pub max_pending_actions: usize,
    /// Per-room membership cap (capacity, JOIN phase): when the room
    /// already holds this many members, a *new* join is rejected with
    /// [`crate::error::CoreError::RoomFull`] — the room never creates the
    /// entity, and the connection actor replies with a gentle `ERROR`
    /// frame (code 8) instead of a silent close (the connection stays
    /// alive and may join another room). A re-join of a connection that is
    /// already a member is never rejected (it supersedes its own state).
    ///
    /// Default `Some(10_000)`: the measured single-room load wall (load
    /// test C1: the 33.3 ms step budget is breached between 9k and 10k
    /// members; at 10k the room runs at 23.2 Hz with 52 771 dropped
    /// fan-out frames and 2.17 s of late ticks). The cap makes that
    /// degraded regime structurally unreachable; `None` = unlimited.
    pub max_players: Option<usize>,
    /// Cap for catch-up `dt`, in periods: after a long stall the next step
    /// simulates at most this many periods (temporary slow-motion).
    pub max_catchup: u32,
    /// Warn (log) when a group's snapshot payload exceeds this many bytes.
    /// rUDP MTU readiness: an oversized snapshot cannot ride a datagram, so
    /// a sustained warning is the signal to split the group (AOI) or lower
    /// its emission rate.
    pub max_snapshot_bytes: usize,
    /// Keep-alive rate for unchanged groups, in Hz. When a group is
    /// unchanged the room ships nothing for it — but every
    /// `tick_hz / keepalive_hz` steps each group re-sends its last cached
    /// snapshot, so a client that lost its last packet cannot stay stale
    /// forever. `<= 0` disables keep-alive.
    ///
    /// Must be `<= tick_hz`: the room cannot keep alive faster than it
    /// ticks. The registry rejects a room with `keepalive_hz > tick_hz` at
    /// creation ([`crate::error::CoreError::KeepaliveRate`]); direct
    /// construction (library use) warns once and clamps the cadence to
    /// every step, which defeats the silence gain.
    pub keepalive_hz: f64,
    /// Rate, in Hz, at which the room emits a metrics sample over the
    /// (bounded) metrics channel. The room samples at most every
    /// `tick_hz / metrics_cadence_hz` steps; the collector only ever uses
    /// the *latest* sample per report period, so sampling faster than the
    /// report cadence discards samples (A2). Tying this to the collector's
    /// report cadence (default 1 Hz) means each room sends ~1 sample per
    /// report instead of one per step — 30× less traffic on the channel, and
    /// since the counters are cumulative nothing is lost.
    ///
    /// `> tick_hz` clamps to every step (you cannot sample faster than you
    /// step); `<= 0` means "every step" as well.
    pub metrics_cadence_hz: f64,
    /// Per-connection cap on PENDING external (delegated) requests — the
    /// request/RPC pattern (see `crate::rpc`). A request arriving when
    /// the connection already has this many in flight is answered with a
    /// normal rejection in the same tick. Room-local requests (answered
    /// in the same tick) never occupy a pending slot. The arrival *rate*
    /// is separately bounded by the READ phase's per-connection pull
    /// budget (a request is an action), so the cap bounds state, not
    /// rate.
    ///
    /// Default 4: a well-behaved client has at most 1–2 requests in
    /// flight (one per logical action; a two-action burst in one frame
    /// is the realistic maximum) plus 1–2 of retry headroom (a client
    /// that suspects a lost answer re-requests under a FRESH id, which
    /// occupies a new slot while the first is still pending). 4 = 2 + 2:
    /// 2 would reject a burst+retry; larger values only let one
    /// misbehaving connection hoard more of the room's budget — the
    /// fairness knob is the exhaustion threshold (room cap / this cap),
    /// see `max_pending_requests` and §6.1 of
    /// `docs/RPC-CONTROL-PLANE.md`.
    pub max_pending_requests_per_conn: usize,
    /// Room-wide cap on pending external requests (all connections). The
    /// per-connection cap alone would still let N connections × the cap
    /// of pending work accumulate in the room's pending set and its
    /// worker tasks; the room cap makes the room's delegation budget
    /// explicit and bounded independently of its population.
    ///
    /// Sizing rule: the in-flight count is driven by **request rate ×
    /// backend latency** (Little's law: L = λ·T) — NOT by the room's
    /// population. Example: 10 000 players × 1 request/min × 1 s of
    /// backend latency ≈ 167 in flight; ×~12 headroom → the default
    /// 2000. Full-cost at saturation: 2000 worker tasks ≈ 1–4 MB,
    /// 2000 timers, worst-case wake spread ≈ 2–4 ms against the 33 ms
    /// step budget.
    ///
    /// Exhaustion threshold (derived): room cap / per-connection cap =
    /// 2000 / 4 = **500 connections** — the room cap can bind only if
    /// ≥500 connections are simultaneously at their full per-connection
    /// quota (5 % of a 10 000-member room); below it, no subset of
    /// connections can starve the rest of the room of pending budget.
    /// The fairness property lives in that number (see §6.1 of
    /// `docs/RPC-CONTROL-PLANE.md`).
    ///
    /// Boundary: this caps the request COUNT (the room's own state
    /// budget), NOT backend concurrency — 2000 in flight means up to
    /// 2000 concurrent backend calls. Capacity-limited services need
    /// call-site throttling (your own queue/pool): the base cannot know
    /// the service's capacity.
    pub max_pending_requests: usize,
    /// The request timeout (see `crate::rpc`): a pending external request
    /// whose deadline passes is swept on the next tick and answered with
    /// a timeout reply (the client never hangs on an accepted request).
    /// The worker task carries the same deadline internally as a resource
    /// guard (a never-resolving future cannot outlive it).
    ///
    /// Default 5 s: comfortably above the latency of a healthy in-process
    /// or networked service (the hook's job is a service round trip, not
    /// game logic), short enough that a stuck dependency degrades a
    /// client's request within one heartbeat cycle.
    pub request_timeout: Duration,
    /// Whether the registry should REBUILD the room from the same factory +
    /// config when its actor task dies unexpectedly (a panic in the game
    /// logic kills the room's task; without the death watch the registry's
    /// table kept answering `Running` forever and joins disappeared into a
    /// dead mailbox — see `RegistryMsg::RoomDied`).
    ///
    /// Contract of a rebirth: the room comes back **EMPTY**. The old
    /// members were notified (`ConnIn::RoomGone`) and may rejoin; no world
    /// state survives — the world lived inside the dead task, and no
    /// cross-task state is shared by design. Default `false`: a death is
    /// final and the operator decides what to do.
    ///
    /// `PartialEq` participation (the idempotent-create comparison): the
    /// flag is an ordinary field, so it compares like every other field —
    /// a retry carries it identically by construction (a retry IS the same
    /// request), and flipping it between retries is a different spec =
    /// `RoomConflict`, exactly as for any other field change.
    pub restart_on_panic: bool,
    /// The room's CLASS (§8 of `docs/RECONNECT.md`), not another restart
    /// knob: `true` = a persistent world piece (an MMO map) that must not
    /// die with a panic; `false` = an ephemeral match room whose end is
    /// final.
    ///
    /// What the class buys, enforced by the REGISTRY:
    /// - supervision rebuilds a dead persistent room EVEN WHEN
    ///   [`RoomConfig::restart_on_panic`] is `false` ("a continuous room
    ///   does not stay dead from a panic" is a class guarantee, not a
    ///   tunable preference). The rebuild comes back EMPTY — without a
    ///   persistence layer an MMO map opens fresh, which is the documented
    ///   §8 limit of this seam, not its goal;
    /// - `DestroyRoom` RETIRES the id in either class (later joins/resumes
    ///   answer ERROR 12 and no create may resurrect the id in-process);
    ///   for an ephemeral room the same retirement means "the match ended,
    ///   never retry" — the flag changes supervision, not the destroy
    ///   path.
    ///
    /// Default `false`; ordinary `PartialEq` participant like every other
    /// field (a retry carries its class identically).
    pub persistent: bool,
    /// **Input-idle ceiling**, in seconds — `None` (the default) = OFF.
    ///
    /// AFK is a GAME decision, not the base's: standing still in an MMO
    /// town is legitimate play, thirty motionless seconds in a MOBA is a
    /// bot-takeover trigger. The base therefore ships the *signal*
    /// unconditionally ([`crate::room::IdleView`], reachable from every
    /// tick hook through `TickCtx::since_input`) and leaves the policy to
    /// the logic. This field is the other half: a capacity SAFETY VALVE
    /// for operators, not a policy.
    ///
    /// When set, a member whose last action-bearing frame is at least
    /// this old is handed to the ordinary disconnect path — the room
    /// calls [`crate::room::GameLogic::on_disconnect`], exactly as a dead
    /// transport does, and the GAME decides park / AI handover / despawn.
    /// The base despawns nothing on its own: one decision point, and a
    /// MOBA gets bot-takeover-on-AFK for free.
    ///
    /// Off by default because there is no value that is right for every
    /// game, and a ceiling that fires on a legitimately motionless player
    /// is worse than no ceiling at all. A parked (detached) or bot-fed
    /// member is never subject to it — neither has a live input source.
    ///
    /// `Some(0)` is treated as OFF as well: a zero ceiling would expire
    /// every member on its first sweep, which is never what an operator
    /// means by "0".
    pub max_idle_input_secs: Option<u64>,
}

impl Default for RoomConfig {
    fn default() -> Self {
        Self {
            id: RoomId(0),
            tick_hz: 30.0,
            control_capacity: 128,
            action_capacity: 256,
            max_actions_per_conn_per_tick: 16,
            max_pending_actions: 65536,
            max_players: Some(10_000),
            max_catchup: 4,
            max_snapshot_bytes: 1400,
            keepalive_hz: 1.0,
            metrics_cadence_hz: 1.0,
            max_pending_requests_per_conn: 4,
            max_pending_requests: 2000,
            request_timeout: Duration::from_secs(5),
            restart_on_panic: false,
            persistent: false,
            // OFF: the feature is invisible until an operator asks for it.
            max_idle_input_secs: None,
        }
    }
}

/// The fallback tick period [`RoomConfig::period`] reports for a config
/// whose `tick_hz` has no usable period (`<= 0`, NaN/±inf, or so high the
/// reciprocal truncates to zero). One second: slow enough that every
/// derived quantity stays finite and panic-free (the µs budget for the
/// step histogram, `period * max_catchup` as the dt cap — even ×u32::MAX
/// fits `Duration`), fast enough that a misconfigured room still steps
/// (and its metrics show a 1 Hz rate instead of a frozen counter).
pub(in crate::room) const FALLBACK_TICK_PERIOD: Duration = Duration::from_secs(1);

impl RoomConfig {
    /// The tick period (`1 / tick_hz`).
    ///
    /// Total by construction: it never panics, mirroring the totality
    /// guard of [`crate::ticker::Ticker::spawn`] (finite, `> 0`,
    /// representable non-zero duration — otherwise the fallback above).
    ///
    /// Why the guard exists even though the registry path rejects bad
    /// rates before any actor exists (the CreateRoom handler refuses a
    /// `tick_hz` whose step divisor rounds below 1 or misses the global
    /// rate): `RoomConfig` is public API and direct hand-built configs
    /// bypass that validation entirely — tests, embedders, factories.
    /// This method previously reached `Duration::from_secs_f64`, which
    /// panics on exactly those inputs, killing an actor task at
    /// construction (and, under `restart_on_panic`, respawning straight
    /// into the same panic forever). A degraded-but-alive room beats a
    /// panic loop: the fallback keeps every derived quantity well-defined.
    /// The input-idle ceiling as a duration, or `None` when it is off
    /// (unset, or an explicit `0` — see the field docs).
    pub(crate) fn max_idle_input(&self) -> Option<Duration> {
        self.max_idle_input_secs
            .filter(|s| *s > 0)
            .map(Duration::from_secs)
    }

    pub fn period(&self) -> Duration {
        let hz = self.tick_hz;
        if hz.is_finite() && hz > 0.0 {
            Duration::try_from_secs_f64(1.0 / hz)
                .ok()
                .filter(|period| !period.is_zero())
                .unwrap_or(FALLBACK_TICK_PERIOD)
        } else {
            FALLBACK_TICK_PERIOD
        }
    }
}
