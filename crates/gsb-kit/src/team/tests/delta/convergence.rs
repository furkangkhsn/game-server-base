//! The delta stream against the full-only oracle, over a long seeded
//! random run (three teams, joins, leaves, walks, teleports, runtime
//! team changes, neutrals): two rooms — full mode and delta mode — play
//! the same script over twin worlds; a stand-in for the core's broadcast
//! phase groups the players, emits one frame per team (keep-alive on
//! the cadence) and hands each player its batch (group frame, then
//! private frame) in order; every player's frames go through the kit's
//! reference client, `ClientView`.
//!
//! - A lossless delta client holds, after EVERY tick, exactly its team's
//!   true content.
//! - At every keep-alive tick every delta client — a lossy one too
//!   (every fourth player; a third of its frames lost between keep-alive
//!   ticks) — holds exactly the view the full-only client of the same
//!   player holds, which is the team's true content (the convergence
//!   guarantee).
//!
//! (Between keep-alive ticks the full-only client is not always exact:
//! a player that switches into a team that already saw its unit finds
//! that team's content unchanged — no frame — and keeps its old team's
//! view until the team changes or the keep-alive re-send. The delta mode
//! ships that player a one-shot full instead.)

use std::collections::{BTreeMap, HashMap};

use bytes::Bytes;

use super::run::{Op, Rng, Script, Twin};
use super::*;
use crate::client::{ClientDecoder, ClientError, ClientView};
use crate::testing::Record;

/// Keep-alive cadence, steps.
const KEEP: u64 = 30;
/// Steps of the run (30 s at 30 Hz).
const TICKS: u64 = 900;

/// The fixture's decode seam: a record → its wire position; no cells.
struct Dec;

impl ClientDecoder for Dec {
    type Record = (i32, i32);
    type Cell = ();

    fn record(&self, body: &[u8]) -> Result<(u64, (i32, i32)), ClientError> {
        let r = Record::decode(body)?;
        Ok((r.entity, (r.x, r.y)))
    }

    fn cell_of(&self, _: &(i32, i32)) {}

    fn cell_exit(&self, _: &[u8]) -> Result<(), ClientError> {
        Err(ClientError::Malformed("the team room sends no cell exits"))
    }
}

type View = BTreeMap<u64, (i32, i32)>;

fn view_of(v: &ClientView<Dec>) -> View {
    v.iter().map(|(id, &at)| (id, at)).collect()
}

/// One player's batch this step: the group frame, the private frame.
type Batch = (PlayerId, Option<Bytes>, Option<Bytes>);

/// The core's broadcast phase for one twin (see `RoomActor::
/// broadcast_phase`): the group table, one frame per group (a full-mode
/// room's keep-alive re-sends the cached frame), then the batches.
fn broadcast(twin: &mut Twin, cache: &mut HashMap<Team, Bytes>, tick: u64) -> Vec<Batch> {
    let (world, room) = (&mut twin.world, &mut twin.room);
    room.update(world, &ctx(tick));
    let groups: Vec<(PlayerId, Team)> = twin
        .players
        .iter()
        .map(|&p| (p, room.group_of(world, p)))
        .collect();
    let teams: BTreeSet<u8> = groups.iter().map(|&(_, t)| t.0).collect();
    cache.retain(|t, _| teams.contains(&t.0));
    let mut sent = HashMap::new();
    for team in teams.into_iter().map(Team) {
        let mut out = bytes::BytesMut::new();
        if room.snapshot(world, &ctx(tick), &team, &[], &mut out) {
            let frame = out.split().freeze();
            cache.insert(team, frame.clone());
            sent.insert(team, frame);
        }
        if tick.is_multiple_of(KEEP) && cache.contains_key(&team) {
            if room.keepalive(world, &ctx(tick), &team, None, &mut out) {
                cache.insert(team, out.split().freeze());
            }
            sent.insert(team, cache[&team].clone());
        }
    }
    groups
        .into_iter()
        .map(|(p, team)| {
            let mut out = bytes::BytesMut::new();
            let private = room
                .private(world, p, &team, &[], &mut out)
                .then(|| out.freeze());
            (p, sent.get(&team).cloned(), private)
        })
        .collect()
}

/// Deliver a batch to a player's view (`keep`: whether each frame
/// arrives).
fn deliver(view: &mut ClientView<Dec>, batch: &Batch, mut keep: impl FnMut() -> bool) {
    if let Some(frame) = &batch.1
        && keep()
    {
        view.apply_snapshot(frame)
            .expect("a well-formed group frame");
    }
    if let Some(frame) = &batch.2
        && keep()
    {
        view.apply_private(frame)
            .expect("a well-formed private frame");
    }
}

/// The team's true content, as a client stores it.
fn truth(room: &TeamRoom, team: Team) -> View {
    room.contents[usize::from(team.0)]
        .iter()
        .map(|(&id, w)| (id, (w.x, w.y)))
        .collect()
}

#[test]
fn delta_clients_converge_to_the_full_only_oracle() {
    let mut script = Script::new(0x7EA3_DE17A);
    let mut loss = Rng::new(7);
    let mut full = Twin::new(TeamRoom::new(25.0));
    let mut delta = Twin::new(TeamRoom::new(25.0).with_delta());
    let (mut full_cache, mut delta_cache) = (HashMap::new(), HashMap::new());
    let mut oracle: HashMap<PlayerId, ClientView<Dec>> = HashMap::new();
    let mut clients: HashMap<PlayerId, ClientView<Dec>> = HashMap::new();
    let lossy = |p: PlayerId| p.0 % 4 == 3;
    let (mut joins, mut switches) = (0u64, 0u64);
    let (mut removals, mut fresh_fulls, mut full_bytes, mut delta_bytes) = (0, 0, 0, 0);

    for tick in 1..=TICKS {
        for op in script.tick() {
            if let Op::Leave(i) = op {
                oracle.remove(&full.players[i]);
                clients.remove(&delta.players[i]);
            }
            joins += u64::from(matches!(op, Op::Join(_)));
            switches += u64::from(matches!(op, Op::Switch(..)));
            let admitted = full.apply(op);
            assert_eq!(admitted, delta.apply(op), "the twins mint alike");
            if let Some(p) = admitted {
                oracle.insert(p, ClientView::new(Dec));
                clients.insert(p, ClientView::new(Dec));
            }
        }
        let keep_tick = tick.is_multiple_of(KEEP);
        let mut seen_groups = BTreeSet::new();
        for batch in broadcast(&mut full, &mut full_cache, tick) {
            let team = full.room.group_of(&full.world, batch.0);
            if let Some(frame) = &batch.1
                && seen_groups.insert(team.0)
            {
                full_bytes += frame.len();
            }
            deliver(oracle.get_mut(&batch.0).expect("a client"), &batch, || true);
        }
        let mut seen_groups = BTreeSet::new();
        for batch in broadcast(&mut delta, &mut delta_cache, tick) {
            let team = delta.room.group_of(&delta.world, batch.0);
            if let Some(frame) = &batch.1
                && seen_groups.insert(team.0)
            {
                delta_bytes += frame.len();
                let s = WorldSnapshot::decode(&frame[..]).expect("a frame");
                removals += s.removed.len();
                fresh_fulls += usize::from(!s.delta && !keep_tick);
            }
            let view = clients.get_mut(&batch.0).expect("a client");
            let drops = lossy(batch.0) && !keep_tick;
            deliver(view, &batch, || !(drops && loss.chance(0.3)));
        }

        for &p in &delta.players {
            let team = delta.room.group_of(&delta.world, p);
            let want = truth(&delta.room, team);
            let got = view_of(&clients[&p]);
            if keep_tick {
                assert_eq!(view_of(&oracle[&p]), want, "tick {tick}: the oracle");
            }
            if !lossy(p) || keep_tick {
                assert_eq!(got, want, "tick {tick}: {p:?} (team {})", team.0);
            }
        }
    }

    let sum = |f: fn(&crate::client::Counters) -> u64| -> u64 {
        clients.values().map(|v| f(v.counters())).sum()
    };
    assert_eq!(sum(|c| c.errors), 0);
    assert_eq!(sum(|c| c.stale), 0);
    let lossless_gaps: u64 = clients
        .iter()
        .filter(|(p, _)| !lossy(**p))
        .map(|(_, v)| v.counters().gap_drops)
        .sum();
    assert!(
        lossless_gaps <= joins + switches,
        "a lossless client drops a delta only on a baseline change: {lossless_gaps}"
    );
    // The run exercised every path.
    assert!(sum(|c| c.deltas) > 1_000 && sum(|c| c.private_fulls) > 5);
    assert!(removals > 20 && fresh_fulls > 0, "{removals} {fresh_fulls}");
    assert!(
        delta_bytes * 2 < full_bytes,
        "half the units stand still each tick: {delta_bytes} vs {full_bytes}"
    );
}
