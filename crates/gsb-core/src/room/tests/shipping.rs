//! The broadcast phase's counters: what the room ENCODED (`snapshots`,
//! `snap_bytes`, `snap_bytes_max`, `snap_overflows`, `snap_records`) and
//! what it SHIPPED (`shipped_frames`, `shipped_bytes`, `private_frames`),
//! plus the room-side `keepalive_resends`.
//!
//! These are the operator-facing half of the room sample — the oversize
//! signal, the fan-out volume, the overlap multiplier — and none of them
//! had a test that drove the real path and read the counter back. A
//! counter nothing exercises is a number, not a measurement: the
//! reject-bucket round made that the repo's standard, and this module
//! applies it to the broadcast phase.
//!
//! Every test drives a REAL `RoomActor` through `step()` (the measured
//! step: counters, sample flush and all) and asserts the specific field.
//! Synchronous and deterministic — no spawn, no ticker task, no sleep.

use super::*;
use crate::room::actor::RoomActor;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

/// The knobs a test turns between steps. Atomics, not a shared cell
/// behind a guard: the logic lives inside the actor and the architecture
/// is lock-free (`gsb-lint` enforces it), so a counter-free handle is the
/// only shape available — and the whole rig is single-threaded anyway.
#[derive(Clone, Default)]
struct Knobs {
    /// Bytes the next `snapshot` call emits; 0 = the group reports
    /// "unchanged" (encodes nothing).
    snap: Arc<AtomicUsize>,
    /// Bytes each `private` frame carries; 0 = no private frame.
    private: Arc<AtomicUsize>,
    /// What `encoded_records` reports each time the room polls it.
    records: Arc<AtomicU64>,
}

impl Knobs {
    fn set_snap(&self, n: usize) {
        self.snap.store(n, Ordering::Relaxed);
    }
    fn set_private(&self, n: usize) {
        self.private.store(n, Ordering::Relaxed);
    }
    fn set_records(&self, n: u64) {
        self.records.store(n, Ordering::Relaxed);
    }
}

/// A logic whose encoded sizes the test dictates: one group for the whole
/// room, a snapshot of exactly `knobs.snap` bytes, a private frame of
/// exactly `knobs.private` bytes, and `knobs.records` encoded records.
struct ShipLogic {
    k: Knobs,
}

impl GameLogic<()> for ShipLogic {
    type GroupKey = ();
    type Strip = ();

    fn snapshot_op(&self) -> u16 {
        0x7600
    }
    fn private_op(&self) -> u16 {
        0x7601
    }

    fn group_of(&self, _w: &(), _p: PlayerId) -> Self::GroupKey {}

    fn snapshot(
        &mut self,
        _w: &mut (),
        _c: &TickCtx,
        _g: &(),
        _borrowed: &[crate::shard::BorderRecord<()>],
        out: &mut bytes::BytesMut,
    ) -> bool {
        let n = self.k.snap.load(Ordering::Relaxed);
        if n == 0 {
            return false; // "unchanged": the keep-alive arm's precondition
        }
        out.extend_from_slice(&vec![0xAB; n]);
        true
    }

    fn private(
        &mut self,
        _w: &mut (),
        _p: PlayerId,
        _g: &(),
        _replies: &[crate::rpc::RpcReply],
        out: &mut bytes::BytesMut,
    ) -> bool {
        let n = self.k.private.load(Ordering::Relaxed);
        if n == 0 {
            return false;
        }
        out.extend_from_slice(&vec![0xCD; n]);
        true
    }

    fn on_join(&mut self, _w: &mut (), c: ConnectionId) -> Admission {
        // Test identity policy: the conn id doubles as the player id.
        Admission {
            player: PlayerId(c.0),
            entity: c.0,
        }
    }
    fn on_leave(&mut self, _w: &mut (), _p: PlayerId) {}
    fn ingest(&mut self, _w: &mut (), _c: &TickCtx, a: &mut Vec<Action>) {
        a.clear();
    }
    fn update(&mut self, _w: &mut (), _c: &TickCtx) {}

    fn encoded_records(&mut self) -> u64 {
        self.k.records.load(Ordering::Relaxed)
    }
}

impl RoomLogic<()> for ShipLogic {}

/// A bare room the test steps directly, with the peers that must outlive
/// it: closing an out channel or the control mailbox would change what a
/// step does (a dead outbound half is a different fan-out path).
struct Rig {
    actor: RoomActor<(), (), ()>,
    k: Knobs,
    t0: Instant,
    tick: u64,
    _outs: Vec<mpsc::Receiver<FrameBatch>>,
    _control: Mailbox<RoomControl>,
}

impl Rig {
    fn new(cfg: RoomConfig) -> Self {
        let k = Knobs::default();
        let (_tick_tx, tick_rx) = broadcast::channel(8);
        let (control, control_rx) = channel(16);
        let actor = RoomActor::new(
            cfg,
            (),
            Box::new(ShipLogic { k: k.clone() }),
            tick_rx,
            control_rx,
            1,
            null_metrics_tx(),
            None,
        );
        Self {
            actor,
            k,
            t0: Instant::now(),
            tick: 0,
            _outs: Vec::new(),
            _control: control,
        }
    }

    /// Join one connection, keeping its outbound half open and UNREAD
    /// (capacity 64 — far more than any test here ships, so nothing is
    /// dropped and `shipped_*` counts every frame the room handed over).
    fn join(&mut self, conn: ConnectionId) {
        let (out_tx, out_rx) = mpsc::channel::<FrameBatch>(64);
        self._outs.push(out_rx);
        let (rtx, mut rrx) = oneshot::channel();
        self.actor.handle_control(RoomControl::Join {
            conn,
            out: out_tx,
            reply: rtx,
        });
        rrx.try_recv()
            .expect("reply sent synchronously")
            .expect("join accepted (room not full)");
    }

    /// One measured step, stamped on a synthetic 30 Hz clock.
    fn step(&mut self) {
        self.tick += 1;
        let at = self.t0 + Duration::from_secs_f64(self.tick as f64 / 30.0);
        assert!(
            self.actor.step(&TickInfo {
                tick: self.tick,
                at
            }),
            "the room keeps running"
        );
    }

    fn sample(&self) -> crate::metrics::RoomSample {
        self.actor.sample()
    }
}

fn cfg(id: u64) -> RoomConfig {
    RoomConfig {
        id: RoomId(id),
        tick_hz: 30.0,
        keepalive_hz: 0.0, // off unless a test asks for it
        metrics_cadence_hz: 0.0,
        ..Default::default()
    }
}

// =====================================================================
// Encoded: bytes, peak, overflow, records
// =====================================================================

/// `snap_bytes` is the SUM of every encoded payload and `snap_bytes_max`
/// the largest single one — a peak, not "the last one" and not "the
/// first one".
///
/// The third step is the load-bearing one: it encodes a SMALLER payload
/// than the peak already seen, so a `snap_bytes_max` that merely tracked
/// the latest encode (or was assigned unconditionally) reports 20 here
/// instead of 30. `snap_bytes_max` is the MTU-readiness signal an
/// operator sizes `max_snapshot_bytes` against, so "the last payload" is
/// not a harmless approximation of it: it under-reports exactly when a
/// burst of large snapshots is followed by a quiet one.
#[test]
fn snapshot_bytes_sum_and_peak_track_the_encoded_payloads() {
    let mut r = Rig::new(cfg(21));
    r.join(ConnectionId(1));

    r.k.set_snap(10);
    r.step();
    let s = r.sample();
    assert_eq!(s.snapshots, 1, "one group encoded one snapshot");
    assert_eq!(s.snap_bytes, 10, "the encoded payload's bytes");
    assert_eq!(s.snap_bytes_max, 10, "the first payload seeds the peak");

    r.k.set_snap(30);
    r.step();
    let s = r.sample();
    assert_eq!(s.snapshots, 2);
    assert_eq!(s.snap_bytes, 40, "10 + 30: the sum is cumulative");
    assert_eq!(s.snap_bytes_max, 30, "a larger payload raises the peak");

    // The peak must SURVIVE a smaller payload.
    r.k.set_snap(20);
    r.step();
    let s = r.sample();
    assert_eq!(s.snapshots, 3);
    assert_eq!(s.snap_bytes, 60, "10 + 30 + 20");
    assert_eq!(
        s.snap_bytes_max, 30,
        "a smaller payload must not lower the peak: snap_bytes_max is the \
         largest snapshot ever encoded, not the latest"
    );
}

/// `snap_overflows` counts EVERY oversized emit, not once per group.
///
/// The distinction is the whole point of the counter: the room warns once
/// per group (a standing property does not need a log line per tick), so
/// the only thing that can answer "how often is this room over the wire
/// budget" is the counter. A once-per-group counter would read 1 for a
/// room that overflows on every single tick — the AOI/MTU decision input
/// the load test reports would be off by the run length.
#[test]
fn every_oversized_snapshot_is_counted_not_just_the_first() {
    let mut r = Rig::new(RoomConfig {
        max_snapshot_bytes: 64,
        ..cfg(22)
    });
    r.join(ConnectionId(1));

    r.k.set_snap(100); // over the 64-byte budget
    for _ in 0..3 {
        r.step();
    }
    let s = r.sample();
    assert_eq!(
        s.snap_overflows, 3,
        "three oversized emits must count three times (the warn fires once \
         per group; the counter is what measures the rate)"
    );
    assert_eq!(s.snapshots, 3, "all three were still encoded and shipped");
    assert_eq!(s.snap_bytes_max, 100);

    // Back under the budget: the counter must stop moving.
    r.k.set_snap(10);
    r.step();
    let s = r.sample();
    assert_eq!(
        s.snap_overflows, 3,
        "a payload within the budget is not an overflow"
    );
    assert_eq!(s.snapshots, 4);
}

/// A payload exactly AT `max_snapshot_bytes` is within budget; one byte
/// over is not. The boundary is the operator's threshold (`>`), so it is
/// pinned rather than left to a reading of the source.
#[test]
fn the_overflow_boundary_is_strictly_above_the_budget() {
    let mut r = Rig::new(RoomConfig {
        max_snapshot_bytes: 64,
        ..cfg(23)
    });
    r.join(ConnectionId(1));

    r.k.set_snap(64); // exactly the budget
    r.step();
    assert_eq!(
        r.sample().snap_overflows,
        0,
        "a payload exactly at max_snapshot_bytes is within budget"
    );

    r.k.set_snap(65); // one byte over
    r.step();
    assert_eq!(
        r.sample().snap_overflows,
        1,
        "one byte over the budget is an overflow"
    );
}

/// `snap_records` accumulates what the LOGIC reports it encoded (the
/// overlap-multiplier numerator), polled once per step.
///
/// "Once per step" is the half worth locking: the payload is opaque to
/// the core, so this number can only come from `encoded_records`, and
/// polling it per GROUP instead of per step would silently multiply the
/// overlap metric by the group count.
#[test]
fn snap_records_accumulates_the_logics_encoded_record_count() {
    let mut r = Rig::new(cfg(24));
    r.join(ConnectionId(1));
    r.join(ConnectionId(2));
    r.k.set_snap(8);
    r.k.set_records(7);

    r.step();
    assert_eq!(
        r.sample().snap_records,
        7,
        "one poll per step, whatever the logic reported"
    );

    r.step();
    r.step();
    assert_eq!(
        r.sample().snap_records,
        21,
        "3 steps x 7 records: cumulative, one poll per step"
    );
}

// =====================================================================
// Shipped: frames, bytes, private
// =====================================================================

/// `shipped_frames` / `shipped_bytes` / `private_frames` count the
/// per-connection fan-out COPIES, so they scale with members — that is
/// what makes them the room's server-side bytes-out rather than a
/// restatement of `snap_bytes` (which counts each group's encode once).
///
/// Two members, one group, a snapshot frame and a private frame each:
/// four shipped frames, two of them private, and bytes counted per copy.
#[test]
fn shipped_counters_count_every_fanout_copy_private_frames_included() {
    let mut r = Rig::new(cfg(25));
    r.join(ConnectionId(1));
    r.join(ConnectionId(2));
    r.k.set_snap(10);
    r.k.set_private(4);

    r.step();
    let s = r.sample();
    assert_eq!(
        s.snapshots, 1,
        "ONE group encode is shared by both members (the encode is not \
         per connection)"
    );
    assert_eq!(s.snap_bytes, 10, "encoded once");
    assert_eq!(
        s.shipped_frames, 4,
        "2 members x (1 snapshot frame + 1 private frame)"
    );
    assert_eq!(
        s.private_frames, 2,
        "one private frame per member; the snapshot frames are not private"
    );
    assert_eq!(
        s.shipped_bytes,
        2 * (10 + 4),
        "shipped bytes are per COPY: both frames, both members"
    );

    // A step with no private frame moves only the snapshot half.
    r.k.set_private(0);
    r.step();
    let s = r.sample();
    assert_eq!(s.shipped_frames, 6, "4 + 2 snapshot frames");
    assert_eq!(
        s.private_frames, 2,
        "no private frame was encoded on this step"
    );
    assert_eq!(s.shipped_bytes, 28 + 2 * 10);
}

/// A step that ships NOTHING moves none of the shipping counters: the
/// group reported "unchanged" and no private frame was due, so there is
/// no batch at all. (The negative half of the test above — a counter
/// incremented per connection visited rather than per frame pushed would
/// pass that one and fail this one.)
#[test]
fn a_silent_step_ships_nothing() {
    let mut r = Rig::new(cfg(26));
    r.join(ConnectionId(1));
    r.join(ConnectionId(2));
    r.k.set_snap(0);
    r.k.set_private(0);

    r.step();
    r.step();
    let s = r.sample();
    assert_eq!(s.snapshots, 0, "an unchanged group encodes nothing");
    assert_eq!(s.snap_bytes, 0);
    assert_eq!(
        s.shipped_frames, 0,
        "two members were VISITED but nothing was shipped to them"
    );
    assert_eq!(s.private_frames, 0);
    assert_eq!(s.shipped_bytes, 0);
}

// =====================================================================
// Keep-alive (room side)
// =====================================================================

/// `keepalive_resends` counts the re-send of an UNCHANGED group's cached
/// snapshot — and only that. A group that emitted on the keep-alive tick
/// shipped a fresh payload, not a re-send, and must not be counted.
///
/// The shard actor's copy of this counter was locked by
/// `shard::tests::keepalive`; the room's was not, although the two are
/// separate code paths in separate files.
#[test]
fn keepalive_resend_counts_the_unchanged_group_only() {
    // keepalive every 3 steps (tick_hz / keepalive_hz = 30 / 10).
    let mut r = Rig::new(RoomConfig {
        keepalive_hz: 10.0,
        ..cfg(27)
    });
    r.join(ConnectionId(1));

    // Step 1 emits, seeding the group's cache; steps 2 and 3 are
    // unchanged, and step 3 is the keep-alive tick.
    r.k.set_snap(12);
    r.step();
    r.k.set_snap(0);
    r.step();
    r.step();

    let s = r.sample();
    assert_eq!(s.snapshots, 1, "only step 1 encoded anything");
    assert_eq!(
        s.keepalive_resends, 1,
        "step 3 is the keep-alive tick and the group is unchanged: exactly \
         one re-send"
    );
    assert_eq!(
        s.shipped_frames, 2,
        "the emitted snapshot (step 1) and the keep-alive re-send (step 3)"
    );
    assert_eq!(
        s.shipped_bytes, 24,
        "the re-send ships the cached payload again: 12 + 12 bytes"
    );

    // Step 6 is the next keep-alive tick — but the group EMITS on it, so
    // it is not a re-send.
    r.step(); // 4
    r.step(); // 5
    r.k.set_snap(12);
    r.step(); // 6: keep-alive tick AND an emit
    let s = r.sample();
    assert_eq!(s.snapshots, 2, "step 6 encoded a fresh payload");
    assert_eq!(
        s.keepalive_resends, 1,
        "a group that EMITTED on the keep-alive tick shipped a fresh \
         snapshot, not a re-send"
    );
}
