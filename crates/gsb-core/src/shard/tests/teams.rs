//! The TEAMS phase (`docs/CROSS-SHARD.md` §8b.3) on a directly-stepped
//! shard actor: the logic's export reaches the registry's mailbox
//! (capped, cleared once, drops counted), and the imports the CONTROL
//! drain applied reach the logic merged — until the TTL drops them.

use std::collections::VecDeque;

use bytes::Bytes;

use super::*;
use crate::registry::RegistryMsg;

/// What the stub logic saw of the imports: `(team, wire, from, tick)`.
type Seen = Vec<(u64, u64, usize, u64)>;

/// A one-group logic with no world of its own: the TEAMS hook plays
/// back a script of exports (`None` once it runs out) and reports every
/// import view it is handed.
struct TeamStub {
    script: VecDeque<Option<TeamExport>>,
    seen: mpsc::UnboundedSender<Seen>,
}

impl GameLogic<TWorld> for TeamStub {
    type GroupKey = ();
    type Strip = TStrip;

    fn snapshot_op(&self) -> u16 {
        0x7190
    }
    fn private_op(&self) -> u16 {
        0x7191
    }
    fn group_of(&self, _w: &TWorld, _p: PlayerId) -> Self::GroupKey {}
    fn snapshot(
        &mut self,
        _w: &mut TWorld,
        _ctx: &TickCtx,
        _g: &Self::GroupKey,
        _borrowed: &[BorderRecord<TStrip>],
        _out: &mut bytes::BytesMut,
    ) -> bool {
        false
    }
    fn on_join(&mut self, _w: &mut TWorld, conn: ConnectionId) -> Admission {
        Admission {
            player: PlayerId(conn.0),
            entity: conn.0,
        }
    }
    fn on_leave(&mut self, _w: &mut TWorld, _player: PlayerId) {}
    fn ingest(&mut self, _w: &mut TWorld, _c: &TickCtx, actions: &mut Vec<Action>) {
        actions.clear();
    }
    fn update(&mut self, _w: &mut TWorld, _c: &TickCtx) {}
}

impl ShardLogic<TWorld> for TeamStub {
    type State = TState;

    fn index(&self) -> usize {
        2
    }
    fn shard_count(&self) -> usize {
        4
    }
    fn serial_base(&self) -> u64 {
        2 * SHARD_SERIAL_RANGE
    }
    fn serial_range(&self) -> u64 {
        SHARD_SERIAL_RANGE
    }
    fn serial_used(&self) -> u64 {
        0
    }
    fn neighbors(&self) -> &[usize] {
        &[]
    }
    fn collect_migrations(&mut self, _w: &mut TWorld, _nb: usize) -> Vec<Migrating<TState>> {
        Vec::new()
    }
    fn on_migrate_in(&mut self, _w: &mut TWorld, _: u64, _: TState, _: Option<PlayerId>) {}
    fn on_migrate_out(&mut self, _w: &mut TWorld, _wire: u64) {}
    fn collect_border(&self, _w: &TWorld) -> Vec<BorderRecord<TStrip>> {
        Vec::new()
    }
    fn own_wires(&self, _w: &TWorld) -> Vec<u64> {
        Vec::new()
    }

    fn team_exchange(
        &mut self,
        _world: &mut TWorld,
        _ctx: &TickCtx,
        _borrowed: &[BorderRecord<TStrip>],
        imported: &TeamImports,
    ) -> Option<TeamExport> {
        let seen = imported
            .teams()
            .flat_map(|t| {
                imported
                    .team(t)
                    .iter()
                    .map(move |r| (t, r.wire, r.from, r.tick))
            })
            .collect();
        let _ = self.seen.send(seen);
        self.script.pop_front().flatten()
    }
}

/// A team record whose body is its wire id.
fn rec(team: u64, wire: u64) -> TeamRecord {
    TeamRecord {
        team,
        wire,
        bytes: Bytes::from(wire.to_le_bytes().to_vec()),
    }
}

fn export(views: &[u64], records: &[(u64, u64)]) -> TeamExport {
    TeamExport {
        views: views.to_vec(),
        records: records.iter().map(|&(t, w)| rec(t, w)).collect(),
    }
}

struct Rig {
    actor: ShardActor<TWorld, (), TState, TStrip>,
    registry: Inbox<RegistryMsg>,
    seen: mpsc::UnboundedReceiver<Seen>,
    _inbox: Mailbox<ShardMsg<TState, TStrip>>,
}

/// Shard 2 of room 12, install generation 3, over a registry mailbox of
/// `cap` slots (`with_registry: false` = a directly-driven shard).
fn rig(script: Vec<Option<TeamExport>>, cap: usize, with_registry: bool) -> Rig {
    let (_tick_tx, tick_rx) = broadcast::channel(64);
    let (inbox, rx) = channel::<ShardMsg<TState, TStrip>>(16);
    let (reg_tx, registry) = channel::<RegistryMsg>(cap);
    let (seen_tx, seen) = mpsc::unbounded_channel();
    let actor = ShardActor::new(
        RoomConfig {
            id: RoomId(12),
            keepalive_hz: 0.0,
            metrics_cadence_hz: 0.0,
            ..Default::default()
        },
        2,
        TWorld::default(),
        Box::new(TeamStub {
            script: script.into(),
            seen: seen_tx,
        }),
        tick_rx,
        rx,
        Vec::new(),
        1,
        metrics_null(),
        None,
    )
    .with_effect_epoch(3);
    let actor = if with_registry {
        actor.with_registry(reg_tx)
    } else {
        actor
    };
    Rig {
        actor,
        registry,
        seen,
        _inbox: inbox,
    }
}

/// The exports queued on the registry's mailbox:
/// `(room, generation, from, tick, export)`.
fn exports(rx: &mut Inbox<RegistryMsg>) -> Vec<(RoomId, u64, usize, u64, TeamExport)> {
    let mut out = Vec::new();
    while let Ok(msg) = rx.try_recv() {
        match msg {
            RegistryMsg::TeamExport {
                room,
                generation,
                from,
                tick,
                export,
            } => out.push((room, generation, from, tick, export)),
            other => panic!("only team exports expected: {other:?}"),
        }
    }
    out
}

/// The logic's export goes to the registry stamped with the room, the
/// install generation, the shard and the tick; a logic that takes no
/// part (`None`) sends nothing, and neither does a shard without a
/// registry.
#[test]
fn the_teams_phase_sends_the_logics_export_to_the_hub() {
    let mut r = rig(vec![Some(export(&[1], &[(1, 10)])), None], 8, true);
    assert!(r.actor.step(&tinfo(5)));
    assert!(r.actor.step(&tinfo(6)));
    assert_eq!(
        exports(&mut r.registry),
        [(RoomId(12), 3, 2, 5, export(&[1], &[(1, 10)]))]
    );
    assert_eq!(r.actor.tstats.exports, 1);
    assert_eq!(r.actor.tstats.export_records, 1);

    let mut bare = rig(vec![Some(export(&[1], &[(1, 10)]))], 8, false);
    assert!(bare.actor.step(&tinfo(5)));
    assert!(exports(&mut bare.registry).is_empty(), "no hub to tell");
}

/// Nothing to export after something: ONE empty export (the receivers
/// clear this shard's slot), then silence.
#[test]
fn an_emptied_export_is_sent_once_then_silence() {
    let script = vec![
        Some(export(&[1], &[(1, 10)])),
        Some(TeamExport::default()),
        Some(TeamExport::default()),
    ];
    let mut r = rig(script, 8, true);
    for t in 1..=3 {
        assert!(r.actor.step(&tinfo(t)));
    }
    let got: Vec<(u64, TeamExport)> = exports(&mut r.registry)
        .into_iter()
        .map(|(_, _, _, tick, e)| (tick, e))
        .collect();
    assert_eq!(
        got,
        [(1, export(&[1], &[(1, 10)])), (2, TeamExport::default())]
    );
}

/// A refused export is counted, and the clear it would have carried is
/// retried on the next tick.
#[test]
fn a_refused_export_is_counted_and_the_clear_retried() {
    let script = vec![
        Some(export(&[], &[(1, 10)])),
        Some(TeamExport::default()),
        Some(TeamExport::default()),
    ];
    let mut r = rig(script, 1, true);
    assert!(r.actor.step(&tinfo(1)));
    assert!(r.actor.step(&tinfo(2)), "the mailbox is full: dropped");
    assert_eq!(r.actor.tstats.export_drops, 1);
    assert_eq!(exports(&mut r.registry).len(), 1);
    assert!(r.actor.step(&tinfo(3)));
    let got: Vec<u64> = exports(&mut r.registry)
        .into_iter()
        .map(|(_, _, _, tick, e)| {
            assert!(e.is_empty());
            tick
        })
        .collect();
    assert_eq!(got, [3], "the clear, retried");
}

/// The hard caps hold whatever the logic returns; the cut is counted.
#[test]
fn the_core_caps_an_oversized_export() {
    let records: Vec<(u64, u64)> = (0..TEAM_EXPORT_MAX_RECORDS as u64 + 3)
        .map(|w| (0, w))
        .collect();
    let views: Vec<u64> = (0..TEAM_EXPORT_MAX_VIEWS as u64 + 2).collect();
    let mut r = rig(vec![Some(export(&views, &records))], 8, true);
    assert!(r.actor.step(&tinfo(1)));
    let got = exports(&mut r.registry);
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].4.records.len(), TEAM_EXPORT_MAX_RECORDS);
    assert_eq!(got[0].4.views.len(), TEAM_EXPORT_MAX_VIEWS);
    assert_eq!(r.actor.tstats.over_cap, 5);
}

/// An import the CONTROL drain applied reaches the logic in the same
/// tick's TEAMS phase, merged; a source silent for the TTL is gone.
#[test]
fn imports_reach_the_logic_until_the_ttl_drops_them() {
    let mut r = rig(Vec::new(), 8, true);
    let import = TeamImport {
        from: 1,
        tick: 10,
        records: vec![rec(4, 70), rec(4, 71)],
    };
    assert!(r.actor.handle_msg(ShardMsg::TeamImport(import), 10));
    assert!(r.actor.step(&tinfo(10)));
    assert_eq!(
        r.seen.try_recv().expect("the hook ran"),
        [(4, 70, 1, 10), (4, 71, 1, 10)]
    );
    assert_eq!(r.actor.tstats.imports, 1);
    assert_eq!(r.actor.tstats.import_records, 2);

    assert!(r.actor.step(&tinfo(10 + TEAM_EXPORT_TTL_TICKS - 1)));
    assert_eq!(r.seen.try_recv().expect("hook").len(), 2, "not yet");
    assert!(r.actor.step(&tinfo(10 + TEAM_EXPORT_TTL_TICKS)));
    assert!(r.seen.try_recv().expect("hook").is_empty(), "expired");
    assert_eq!(r.actor.tstats.expired, 1);
}
