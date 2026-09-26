//! The hub's relay rules on real bounded mailboxes: fan-out only to the
//! OTHER shards viewing a record's team, isolation between teams, the
//! one clearing import, a refused relay retried, the TTL sweep.

use bytes::Bytes;

use super::*;
use crate::channel::{Inbox, channel};

type Msg = ShardMsg<(), ()>;

const ROOM: RoomId = RoomId(9);

/// `n` shard mailboxes of capacity `cap` and their receivers.
fn shards(n: usize, cap: usize) -> (Vec<Mailbox<Msg>>, Vec<Inbox<Msg>>) {
    (0..n).map(|_| channel::<Msg>(cap)).unzip()
}

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
        over_budget: 0,
    }
}

/// Imports as `(from, tick, [(team, wire)])`.
type Got = Vec<(usize, u64, Vec<(u64, u64)>)>;

/// Every import queued for one shard.
fn drained(rx: &mut Inbox<Msg>) -> Got {
    let mut out = Vec::new();
    while let Ok(msg) = rx.try_recv() {
        match msg {
            ShardMsg::TeamImport(i) => out.push((
                i.from,
                i.tick,
                i.records.iter().map(|r| (r.team, r.wire)).collect(),
            )),
            other => panic!("the hub sends only imports: {other:?}"),
        }
    }
    out
}

/// Four shards: 0 views team 1, 1 views team 2, 2 views teams 1 and 2,
/// 3 views nothing (a ward-only shard). Shard 0's export of team 1 and
/// team 2 records reaches exactly the shards viewing each team — never
/// shard 0 itself, never shard 3, never a record of a team the target
/// does not view.
#[test]
fn an_export_reaches_only_the_other_shards_viewing_its_teams() {
    let (tx, mut rx) = shards(4, 16);
    let mut hub = TeamHub::default();
    hub.on_export(ROOM, 1, 1, export(&[2], &[]), &tx);
    hub.on_export(ROOM, 2, 1, export(&[2, 1, 2], &[]), &tx);
    hub.on_export(ROOM, 3, 1, export(&[], &[(3, 30)]), &tx);
    // Shard 0 has exported before (it views team 1 itself).
    hub.on_export(ROOM, 0, 1, export(&[1], &[]), &tx);
    assert_eq!(hub.views(2), Some(&[1, 2][..]), "sorted, deduplicated");
    for r in rx.iter_mut() {
        let _ = drained(r);
    }

    hub.on_export(ROOM, 0, 2, export(&[1], &[(1, 10), (2, 11), (1, 12)]), &tx);
    assert!(drained(&mut rx[0]).is_empty(), "never back to the source");
    assert_eq!(drained(&mut rx[1]), [(0, 2, vec![(2, 11)])], "team 2 only");
    assert_eq!(
        drained(&mut rx[2]),
        [(0, 2, vec![(1, 10), (2, 11), (1, 12)])]
    );
    assert!(drained(&mut rx[3]).is_empty(), "no viewer, no relay");
    assert_eq!(hub.stats.relays, 2);
    assert_eq!(hub.stats.relay_records, 4);
}

/// A shard that has never exported has no subscription: nothing is
/// relayed to it until its first export lists a team.
#[test]
fn a_shard_that_never_exported_gets_nothing() {
    let (tx, mut rx) = shards(2, 16);
    let mut hub = TeamHub::default();
    hub.on_export(ROOM, 0, 1, export(&[1], &[(1, 10)]), &tx);
    assert!(drained(&mut rx[1]).is_empty());
    hub.on_export(ROOM, 1, 1, export(&[1], &[]), &tx);
    hub.on_export(ROOM, 0, 2, export(&[1], &[(1, 10)]), &tx);
    assert_eq!(drained(&mut rx[1]), [(0, 2, vec![(1, 10)])]);
}

/// When a source has nothing left for a target that holds its records,
/// ONE empty import clears the target's slot; after that the hub stays
/// silent toward it.
#[test]
fn an_emptied_set_is_cleared_once_then_silent() {
    let (tx, mut rx) = shards(2, 16);
    let mut hub = TeamHub::default();
    hub.on_export(ROOM, 1, 1, export(&[1], &[]), &tx);
    hub.on_export(ROOM, 0, 1, export(&[], &[(1, 10)]), &tx);
    assert_eq!(drained(&mut rx[1]), [(0, 1, vec![(1, 10)])]);
    hub.on_export(ROOM, 0, 2, export(&[], &[(2, 20)]), &tx);
    assert_eq!(drained(&mut rx[1]), [(0, 2, vec![])], "the clearing import");
    hub.on_export(ROOM, 0, 3, export(&[], &[(2, 20)]), &tx);
    assert!(drained(&mut rx[1]).is_empty(), "nothing held: silence");
}

/// A relay a full mailbox refuses is counted, and the target's claim
/// on the source stays: the source's next export tries again — the
/// clearing import included.
#[test]
fn a_refused_relay_is_counted_and_retried_by_the_next_export() {
    let (tx, mut rx) = shards(2, 1);
    let mut hub = TeamHub::default();
    hub.on_export(ROOM, 1, 1, export(&[1], &[]), &tx);
    hub.on_export(ROOM, 0, 1, export(&[], &[(1, 10)]), &tx);
    hub.on_export(ROOM, 0, 2, export(&[], &[(1, 11)]), &tx);
    assert_eq!(hub.stats.relay_drops, 1, "the mailbox held one");
    assert_eq!(drained(&mut rx[1]), [(0, 1, vec![(1, 10)])]);

    // Now empty: the clear is refused once (mailbox full again), then
    // lands on the next export.
    tx[1]
        .try_send(ShardMsg::Shutdown)
        .expect("room for the filler");
    hub.on_export(ROOM, 0, 3, export(&[], &[]), &tx);
    assert_eq!(hub.stats.relay_drops, 2);
    let _filler = rx[1].try_recv();
    hub.on_export(ROOM, 0, 4, export(&[], &[]), &tx);
    assert_eq!(drained(&mut rx[1]), [(0, 4, vec![])]);
}

/// A shard silent for the TTL loses its subscription at the next sweep
/// (it stops receiving relays until it exports again).
#[test]
fn a_silent_shards_subscription_expires() {
    let (tx, mut rx) = shards(2, 64);
    let mut hub = TeamHub::default();
    hub.on_export(ROOM, 1, 1, export(&[1], &[]), &tx);
    hub.on_export(ROOM, 0, 1, export(&[], &[(1, 10)]), &tx);
    assert_eq!(drained(&mut rx[1]).len(), 1);

    let later = 1 + TEAM_EXPORT_TTL_TICKS.max(TEAM_HUB_SWEEP_EVERY_TICKS);
    hub.on_export(ROOM, 0, later, export(&[], &[(1, 10)]), &tx);
    assert_eq!(hub.views(1), None, "shard 1 went silent: swept");
    assert_eq!(hub.stats.expired, 1);
    assert!(drained(&mut rx[1]).is_empty());

    hub.on_export(ROOM, 1, later + 1, export(&[1], &[]), &tx);
    hub.on_export(ROOM, 0, later + 1, export(&[], &[(1, 10)]), &tx);
    assert_eq!(drained(&mut rx[1]), [(0, later + 1, vec![(1, 10)])]);
}

/// A bogus source index (not a shard of the room) changes nothing.
#[test]
fn an_export_from_an_unknown_index_is_ignored() {
    let (tx, mut rx) = shards(2, 16);
    let mut hub = TeamHub::default();
    hub.on_export(ROOM, 1, 1, export(&[1], &[]), &tx);
    hub.on_export(ROOM, 5, 1, export(&[1], &[(1, 10)]), &tx);
    assert!(drained(&mut rx[1]).is_empty());
    assert_eq!(hub.stats.exports, 1);
}
