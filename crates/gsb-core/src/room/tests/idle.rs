//! The AFK seam: the input-idle SIGNAL every logic can read, and the
//! optional input-idle CEILING an operator can set.
//!
//! What "action-bearing" means is structural, and these tests lock the
//! structure rather than an opcode list: the clock moves exactly when the
//! room's READ phase pulls an [`Action`] for a member. A HEARTBEAT never
//! becomes one — the connection actor answers it in its own `HEARTBEAT`
//! arm and never calls `forward_to_room` (locked from the connection side
//! by `tests/security.rs::postauth_heartbeat_flood_is_answered_once_…`)
//! — so from the room's point of view a heartbeat-only client is a member
//! whose action channel stays empty while its session stays live. That is
//! exactly the shape the first test drives.

use super::*;

/// The observation channel payload: `(player, since_input)` as the logic
/// saw it in `update` on some step.
type IdleObs = (PlayerId, Option<Duration>);

struct IdleLogic {
    players: Vec<PlayerId>,
    obs: mpsc::Sender<IdleObs>,
    disc: mpsc::Sender<(PlayerId, String)>,
    decision: Detach,
}

impl GameLogic<()> for IdleLogic {
    type GroupKey = ();
    type Strip = ();
    fn snapshot_op(&self) -> u16 {
        0x7500
    }
    fn private_op(&self) -> u16 {
        0x7501
    }
    fn group_of(&self, _w: &(), _p: PlayerId) -> Self::GroupKey {}
    fn snapshot(
        &mut self,
        _w: &mut (),
        _c: &TickCtx,
        _g: &(),
        _borrowed: &[crate::shard::BorderRecord<()>],
        _o: &mut bytes::BytesMut,
    ) -> bool {
        false
    }
    fn on_join(&mut self, _w: &mut (), conn: ConnectionId) -> Admission {
        // Test identity policy: the conn id doubles as the player id.
        let player = PlayerId(conn.0);
        self.players.push(player);
        Admission { player, entity: 1 }
    }
    fn on_leave(&mut self, _w: &mut (), player: PlayerId) {
        self.players.retain(|p| *p != player);
    }
    fn ingest(&mut self, _w: &mut (), _c: &TickCtx, a: &mut Vec<Action>) {
        a.clear();
    }
    /// The AFK policy seam: the signal is read from the tick context,
    /// inside an ordinary hook, with no await and no new method.
    fn update(&mut self, _w: &mut (), ctx: &TickCtx) {
        for p in &self.players {
            let _ = self.obs.try_send((*p, ctx.since_input(*p)));
        }
    }
    fn on_disconnect(&mut self, _w: &mut (), player: PlayerId, identity: &str) -> Detach {
        let _ = self.disc.try_send((player, identity.to_string()));
        self.decision
    }
}

impl RoomLogic<()> for IdleLogic {}

struct Rig {
    actor: RoomActor<(), (), ()>,
    obs: mpsc::Receiver<IdleObs>,
    disc: mpsc::Receiver<(PlayerId, String)>,
    t0: Instant,
    /// Kept alive so the fan-out never sees a closed outbound half.
    _outs: Vec<mpsc::Receiver<FrameBatch>>,
}

impl Rig {
    fn new(cfg: RoomConfig, decision: Detach) -> Self {
        let (obs_tx, obs) = mpsc::channel(4096);
        let (disc_tx, disc) = mpsc::channel(64);
        let (_tick_tx, tick_rx) = broadcast::channel(4);
        let (_control, control_rx) = channel(16);
        let actor = RoomActor::new(
            cfg,
            (),
            Box::new(IdleLogic {
                players: Vec::new(),
                obs: obs_tx,
                disc: disc_tx,
                decision,
            }),
            tick_rx,
            control_rx,
            1,
            null_metrics_tx(),
            None,
        );
        Self {
            actor,
            obs,
            disc,
            t0: Instant::now(),
            _outs: Vec::new(),
        }
    }

    /// Join through the resume arm, which is the path an IDENTIFIED
    /// client takes (§14.3: a ticket-pinned JOIN is the implicit resume
    /// attempt, and an empty ledger makes it a transparent fresh join) —
    /// so the row remembers its resume key.
    fn join(&mut self, conn: ConnectionId, identity: &str) -> (EntityId, Mailbox<Action>) {
        let (out_tx, out_rx) = mpsc::channel::<FrameBatch>(64);
        self._outs.push(out_rx);
        let (rtx, mut rrx) = oneshot::channel();
        self.actor.handle_control(RoomControl::Resume {
            conn,
            epoch: 1,
            identity: identity.to_string(),
            out: out_tx,
            reply: rtx,
        });
        rrx.try_recv()
            .expect("reply sent synchronously")
            .expect("join accepted")
    }

    /// Step the room with an exact synthetic wall clock: `at = t0 + secs`.
    /// The idle clock reads the TICK's own `at`, so a test can age a
    /// member by minutes without sleeping.
    fn step_at(&mut self, tick: u64, secs: u64) {
        self.actor.step_phases(&TickInfo {
            tick,
            at: self.t0 + Duration::from_secs(secs),
        });
    }

    /// The last idle observation the logic made for `player`.
    fn last_idle(&mut self, player: PlayerId) -> Option<Duration> {
        let mut last = None;
        while let Ok((p, d)) = self.obs.try_recv() {
            if p == player {
                last = Some(d);
            }
        }
        last.expect("the logic observed this player at least once")
    }

    fn disconnects(&mut self) -> Vec<(PlayerId, String)> {
        let mut v = Vec::new();
        while let Ok(x) = self.disc.try_recv() {
            v.push(x);
        }
        v
    }
}

fn cfg(id: u64, ceiling: Option<u64>) -> RoomConfig {
    RoomConfig {
        id: RoomId(id),
        max_idle_input_secs: ceiling,
        ..Default::default()
    }
}

// =====================================================================
// 1 — the SIGNAL
// =====================================================================

/// A client that keeps its session alive but never plays is INPUT-IDLE
/// and still a full member: the row is present, not detached, holding its
/// slot. Liveness ("a frame arrived") and input ("an action arrived") are
/// two different clocks, and this is the one the base did not have.
#[test]
fn a_heartbeat_only_client_is_input_idle_while_it_stays_a_member() {
    let mut r = Rig::new(cfg(50, None), Detach::Despawn);
    let (_e, _actions) = r.join(ConnectionId(1), "ana");
    let p = PlayerId(1);
    // Ten seconds of ticks and NOTHING on the action channel — which is
    // all a heartbeat-only client ever produces here.
    for k in 1..=10u64 {
        r.step_at(k, k);
    }
    let idle = r.last_idle(p).expect("the member is on the input clock");
    assert!(
        idle >= Duration::from_secs(9),
        "a heartbeat-only client must read as input-idle, got {idle:?}"
    );
    assert!(
        r.actor.conns.contains_key(&p) && !r.actor.conns[&p].detached,
        "…while staying a live, undetached member (liveness is a \
         different clock)"
    );
}

/// An action-bearing frame — anything the connection actor forwards as an
/// [`Action`] — resets the clock, and the clock starts running again the
/// moment the input stops.
#[test]
fn an_action_resets_the_input_clock() {
    let mut r = Rig::new(cfg(51, None), Detach::Despawn);
    let (_e, actions) = r.join(ConnectionId(1), "ana");
    let p = PlayerId(1);
    for k in 1..=5u64 {
        r.step_at(k, k);
    }
    assert!(
        r.last_idle(p).expect("on the clock") >= Duration::from_secs(4),
        "idle before the action"
    );
    // One game-band action (the payload is opaque to the core).
    actions
        .try_send(Action {
            conn: ConnectionId(1),
            player: PlayerId(0),
            op: 0x3000,
            payload: bytes::Bytes::new(),
        })
        .expect("action channel open");
    r.step_at(6, 6);
    assert_eq!(
        r.last_idle(p),
        Some(Duration::ZERO),
        "the pull of an action-bearing frame resets the clock"
    );
    // …and it ages again from there.
    for k in 7..=9u64 {
        r.step_at(k, k);
    }
    assert_eq!(
        r.last_idle(p),
        Some(Duration::from_secs(3)),
        "the clock restarts from the action, not from the join"
    );
}

// =====================================================================
// 2 — the CEILING
// =====================================================================

/// **The one that matters most.** With `max_idle_input_secs` unset (the
/// default), the feature is INVISIBLE: no amount of idleness makes the
/// base act. AFK is a game decision; the base only offers the signal.
#[test]
fn with_the_ceiling_unset_nothing_ever_happens() {
    let mut r = Rig::new(cfg(52, None), Detach::Despawn);
    let (_e, _actions) = r.join(ConnectionId(1), "ana");
    let p = PlayerId(1);
    // Ten minutes of perfect silence.
    for k in 1..=60u64 {
        r.step_at(k, k * 10);
    }
    assert!(
        r.disconnects().is_empty(),
        "the default config must never run the disconnect policy on an \
         idle member"
    );
    assert!(
        r.actor.conns.contains_key(&p) && !r.actor.conns[&p].detached,
        "the member is untouched: still present, still not parked"
    );
    assert!(
        r.last_idle(p).expect("still on the clock") >= Duration::from_secs(590),
        "the SIGNAL still works with the ceiling off — only the ACTION \
         is opt-in"
    );
}

/// With the ceiling set, expiry runs the DISCONNECT POLICY — the same
/// hook a dead transport reaches, with the row's real resume key — and
/// the base despawns nothing on its own: here the policy answers
/// `Hold { AiHandover }` and the entity is parked, which is MOBA
/// bot-takeover for free.
#[test]
fn the_ceiling_runs_the_disconnect_policy_with_the_rows_identity() {
    let mut r = Rig::new(
        cfg(53, Some(5)),
        Detach::Hold {
            grace: Some(Duration::from_secs(60)),
            to: ExpireTo::AiHandover,
        },
    );
    let (_e, _actions) = r.join(ConnectionId(1), "ana");
    let p = PlayerId(1);
    for k in 1..=3u64 {
        r.step_at(k, k);
    }
    assert!(
        r.disconnects().is_empty(),
        "below the ceiling nothing happens"
    );
    r.step_at(4, 6); // 6 s idle ≥ the 5 s ceiling
    assert_eq!(
        r.disconnects(),
        vec![(p, "ana".to_string())],
        "expiry runs on_disconnect ONCE, with the resume key the row \
         remembers (an empty key would make the member unparkable)"
    );
    assert!(
        r.actor.conns.contains_key(&p) && r.actor.conns[&p].detached,
        "the base parked nothing itself: the POLICY's Hold answer did, \
         and the entity is still alive"
    );
    // The parked row is off the clock, so the ceiling cannot fire on it
    // again — no matter how long the hold lasts.
    for k in 5..=30u64 {
        r.step_at(k, 10 + k);
    }
    assert!(
        r.disconnects().is_empty(),
        "a parked (detached) member is never double-counted by the idle \
         ceiling"
    );
}

/// The forced-ceiling warning is a standing property of the room, so it
/// is emitted ONCE — not once per expired member, and certainly not once
/// per tick. (Asserted on the room's own warn counter rather than on a
/// captured tracing stream: `tracing` caches callsite interest per
/// process, so a scoped subscriber cannot reliably capture a callsite
/// another test in the same binary already evaluated.)
#[test]
fn the_ceiling_warns_once_not_per_tick() {
    let mut r = Rig::new(cfg(54, Some(5)), Detach::Despawn);
    for c in 1..=3u64 {
        let (_e, _a) = r.join(ConnectionId(c), "ana");
    }
    // Age everybody past the ceiling and keep stepping well beyond the
    // first expiry.
    for k in 1..=20u64 {
        r.step_at(k, 10 + k);
    }
    assert_eq!(
        r.disconnects().len(),
        3,
        "every idle member reached the policy"
    );
    assert_eq!(
        r.actor.idle_ceiling_warns, 1,
        "one warning for the room — not one per member, not one per tick"
    );
}

/// A member the ceiling expired toward `Despawn` leaves through the one
/// despawn funnel, and the registry report the detach path owes is queued
/// exactly as a transport death's would be.
#[test]
fn a_despawning_policy_takes_the_ordinary_leave_funnel() {
    let mut r = Rig::new(cfg(55, Some(5)), Detach::Despawn);
    let (_e, _actions) = r.join(ConnectionId(1), "ana");
    let p = PlayerId(1);
    r.step_at(1, 30);
    assert_eq!(r.disconnects(), vec![(p, "ana".to_string())]);
    assert!(!r.actor.conns.contains_key(&p), "the row is gone");
    assert!(r.actor.binding.is_empty(), "so is its binding");
    assert!(r.actor.roster.is_empty(), "and its roster slot");
    assert_eq!(
        r.actor.idle.len(),
        0,
        "and its input clock (no stale slot survives the despawn)"
    );
}

/// A player parked by a REAL transport death must not be double-counted
/// by the idle clock: the detach takes the row off it, so however long
/// the hold lasts the ceiling never asks the policy a second time. (Without
/// that, the ceiling would fire on top of every park whose grace outlives
/// it — two mechanisms racing for one entity.)
#[test]
fn a_transport_death_park_is_never_re_expired_by_the_ceiling() {
    let mut r = Rig::new(
        cfg(56, Some(5)),
        Detach::Hold {
            grace: None, // combat-held: the hold has no deadline at all
            to: ExpireTo::Despawn,
        },
    );
    let (entity, _actions) = r.join(ConnectionId(1), "ana");
    let p = PlayerId(1);
    // The transport dies; the policy parks the entity.
    r.actor.handle_control(RoomControl::Detach {
        conn: ConnectionId(1),
        entity,
        identity: "ana".into(),
    });
    assert_eq!(
        r.disconnects(),
        vec![(p, "ana".to_string())],
        "the transport death ran the policy once"
    );
    assert!(r.actor.conns[&p].detached, "parked");
    // Far past the ceiling, for a long time. `may_release` defaults to
    // `true`, so phase 0c ends this combat-held park on the first step —
    // the assertion that matters is that the CEILING never spoke.
    for k in 1..=40u64 {
        r.step_at(k, 100 + k);
    }
    assert!(
        r.disconnects().is_empty(),
        "the input-idle ceiling must not re-run the disconnect policy on \
         a row the detach path already owns"
    );
    assert_eq!(
        r.actor.idle_ceiling_warns, 0,
        "…and must not even warn about it"
    );
}

mod afk;
mod leave;
// The ceiling's verdicts the server's stop kept from the registry (F56).
mod stop;
