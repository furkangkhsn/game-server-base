//! B72: a relay the hub could not queue is counted into the registry's
//! own cumulative totals, split by cause, and reaches the collector at
//! once — the relay changes no table, so no other sample would carry it.

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use tokio::sync::mpsc;

use crate::channel::{Mailbox, channel};
use crate::id::RoomId;
use crate::metrics::{MetricsEvent, RegistrySample};
use crate::registry::actor::Registry;
use crate::registry::{RegistryMsg, RoomEntry, RoomFactory, ShardGroup, TeamHub};
use crate::room::RoomConfig;
use crate::shard::{ShardMsg, TeamExport, TeamRecord};
use crate::ticker::Ticker;

type Reg = Registry<(), (), (), ()>;

const WAIT: Duration = Duration::from_secs(5);

fn export(views: &[u64], teams: &[u64]) -> TeamExport {
    TeamExport {
        views: views.to_vec(),
        records: teams
            .iter()
            .map(|&team| TeamRecord {
                team,
                wire: team,
                bytes: Bytes::from_static(b"r"),
            })
            .collect(),
        over_budget: 0,
    }
}

/// A registry holding one sharded room (id 5, generation 0) over the
/// given shard mailboxes — no shard tasks: the test plays the shards —
/// running on its own mailbox loop.
fn registry(
    mailboxes: Vec<Mailbox<ShardMsg<(), ()>>>,
) -> (Mailbox<RegistryMsg>, mpsc::Receiver<MetricsEvent>) {
    let factory: RoomFactory<(), (), (), ()> =
        Arc::new(|_id, _cfg| unreachable!("no room is created here"));
    let (tx, rx) = channel::<RegistryMsg>(8);
    let (ticker, _task) = Ticker::spawn(60.0, 64).expect("valid tick rate");
    let (metrics, metrics_rx) = mpsc::channel(64);
    let mut reg: Reg = Registry::new(rx, tx.clone(), factory, ticker, metrics, None, None, None);
    reg.rooms.insert(
        RoomId(5),
        RoomEntry {
            control: None,
            shards: Some(ShardGroup {
                mailboxes,
                home: Arc::new(|_conn, _identity: &str| 0),
                cap: None,
                members: 0,
                pending: 0,
                teams: TeamHub::default(),
            }),
            config: RoomConfig::default(),
            generation: 0,
        },
    );
    tokio::spawn(reg.run());
    (tx, metrics_rx)
}

/// Shard `from`'s export of `tick`, from incarnation `generation`.
async fn send(tx: &Mailbox<RegistryMsg>, generation: u64, from: usize, tick: u64, e: TeamExport) {
    tx.send(RegistryMsg::TeamExport {
        room: RoomId(5),
        generation,
        from,
        tick,
        export: e,
    })
    .await
    .expect("the registry runs");
}

/// The next registry sample (within the wait).
async fn next_sample(rx: &mut mpsc::Receiver<MetricsEvent>) -> RegistrySample {
    loop {
        match tokio::time::timeout(WAIT, rx.recv()).await {
            Ok(Some(MetricsEvent::Registry(s))) => return s,
            Ok(Some(_)) => {}
            other => panic!("no registry sample: {other:?}"),
        }
    }
}

#[tokio::test]
async fn a_full_target_and_a_closed_one_are_counted_apart_and_sampled() {
    let (mailboxes, mut inboxes): (Vec<_>, Vec<_>) =
        (0..3).map(|_| channel::<ShardMsg<(), ()>>(1)).unzip();
    mailboxes[1]
        .try_send(ShardMsg::Shutdown)
        .expect("room for the filler");
    let (tx, mut metrics) = registry(mailboxes);
    // Shards 1 and 2 view team 1; then shard 2 stops (its inbox closes).
    send(&tx, 0, 1, 1, export(&[1], &[])).await;
    send(&tx, 0, 2, 1, export(&[1], &[])).await;
    drop(inboxes.pop());

    send(&tx, 0, 0, 1, export(&[], &[1])).await;
    let s = next_sample(&mut metrics).await;
    assert_eq!(
        (s.team_relays_dropped_full, s.team_relays_dropped_closed),
        (1, 1),
        "the first refusals, sampled at once"
    );

    // Another incarnation's export touches nothing; the counts are
    // cumulative.
    send(&tx, 1, 0, 2, export(&[], &[1])).await;
    send(&tx, 0, 0, 3, export(&[], &[1])).await;
    let s = next_sample(&mut metrics).await;
    assert_eq!(
        (s.team_relays_dropped_full, s.team_relays_dropped_closed),
        (2, 2)
    );
    let more = tokio::time::timeout(Duration::from_millis(100), metrics.recv()).await;
    assert!(more.is_err(), "one refusing export, one sample: {more:?}");
}
