//! F53: what is queued behind the registry's `Shutdown` is read — the
//! inbox closed, drained and counted by kind — and the counts reach the
//! collector in the registry's final sample, ahead of its final report.
//! Every message is queued before the registry runs, so the order is
//! exact: `Shutdown` first, everything else behind it.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{mpsc, oneshot};

use crate::channel::{Inbox, Mailbox, channel};
use crate::conn::{ConnIn, ServerClose};
use crate::error::CoreError;
use crate::id::{ConnectionId, RoomId};
use crate::metrics::{MetricReport, MetricSink, MetricsCollector, MetricsEvent};
use crate::registry::actor::Registry;
use crate::registry::{
    CloseRequest, LeaveRequest, RegistryMsg, RoomEntry, RoomFactory, Seat, ShardGroup, TeamHub,
};
use crate::room::RoomConfig;
use crate::shard::{ShardMsg, TeamExport};
use crate::ticker::Ticker;

type Reg = Registry<(), (), (), ()>;
type Joined = oneshot::Receiver<Result<Seat, CoreError>>;
/// The shards' inboxes, kept so the stop's `Shutdown`s fit in place.
type Shards = Vec<Inbox<ShardMsg<(), ()>>>;

const WAIT: Duration = Duration::from_secs(5);

/// A registry holding one sharded room (id 5, generation 0) over two
/// shard mailboxes (their inboxes returned, so the stop's `Shutdown`s fit
/// in place), sampling on `metrics`. Not running yet: the test queues
/// first.
fn registry(
    metrics: mpsc::Sender<MetricsEvent>,
    ticker: Ticker,
) -> (Reg, Mailbox<RegistryMsg>, Shards) {
    let factory: RoomFactory<(), (), (), ()> =
        Arc::new(|_id, _cfg| unreachable!("no room is created here"));
    let (tx, rx) = channel::<RegistryMsg>(32);
    let (mailboxes, shards): (Vec<_>, Vec<_>) =
        (0..2).map(|_| channel::<ShardMsg<(), ()>>(8)).unzip();
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
    (reg, tx, shards)
}

/// Shard 0's team export for `room`, from incarnation `generation`.
fn export(room: u64, generation: u64) -> RegistryMsg {
    RegistryMsg::TeamExport {
        room: RoomId(room),
        generation,
        from: 0,
        tick: 1,
        export: TeamExport {
            views: vec![1],
            records: Vec::new(),
            over_budget: 0,
        },
    }
}

/// A join of `conn` into room 5, and the connection's end of its reply.
fn join(conn: u64) -> (RegistryMsg, Joined) {
    let (reply, joined) = oneshot::channel();
    let (out, _) = channel(1);
    let msg = RegistryMsg::SpawnPlayer {
        conn: ConnectionId(conn),
        room: RoomId(5),
        out,
        identity: String::new(),
        reply,
    };
    (msg, joined)
}

fn queue(tx: &Mailbox<RegistryMsg>, msg: RegistryMsg) {
    tx.try_send(msg).expect("room in the registry's mailbox");
}

/// Behind the `Shutdown`: the live incarnation's export counts, a stale
/// or unknown room's does not (the hub drops those anyway); every join
/// counts and its reply drops; a connection opened behind the stop is
/// told to stop; a room's verdicts — a close, a leave, a despawn report —
/// are lost verdicts (F56), sent as one `VerdictsLost`; the rest — a
/// transport death, a client's leave, a dispatcher's echo — counts
/// nowhere (the stop's own teardown carries them out). The final sample
/// carries the counts and is the registry's last word: its sender is
/// dropped right after, so the channel closes behind it.
#[tokio::test]
async fn what_waits_behind_the_shutdown_is_counted_by_kind() {
    let (ticker, _task) = Ticker::spawn(60.0, 64).expect("valid tick rate");
    let (metrics, mut samples) = mpsc::channel(64);
    let (reg, tx, _shards) = registry(metrics, ticker);
    queue(&tx, RegistryMsg::Shutdown);
    queue(&tx, export(5, 0));
    queue(&tx, export(5, 1));
    queue(&tx, export(9, 0));
    let (j3, joined3) = join(3);
    let (j4, joined4) = join(4);
    queue(&tx, j3);
    queue(&tx, j4);
    let (late, mut late_inbox) = channel(4);
    queue(
        &tx,
        RegistryMsg::ConnOpened {
            conn: ConnectionId(9),
            inbox: late,
        },
    );
    let c1 = ConnectionId(1);
    queue(
        &tx,
        RegistryMsg::ConnClosed {
            conn: c1,
            verdict: None,
        },
    );
    queue(&tx, RegistryMsg::DespawnPlayer { conn: c1 });
    queue(
        &tx,
        RegistryMsg::SpawnDone {
            conn: c1,
            room: RoomId(5),
            entity: 1,
            generation: 0,
        },
    );
    queue(
        &tx,
        RegistryMsg::CloseConn(CloseRequest {
            conn: c1,
            room: RoomId(5),
            entity: 1,
            parked: false,
            cause: ServerClose::IdleInput,
            reason: "idle".into(),
        }),
    );
    queue(
        &tx,
        RegistryMsg::DetachDespawned {
            conn: c1,
            room: RoomId(5),
        },
    );
    queue(
        &tx,
        RegistryMsg::LeaveConn(LeaveRequest {
            conn: c1,
            room: RoomId(5),
            entity: 1,
            park: None,
        }),
    );
    tokio::time::timeout(WAIT, reg.run())
        .await
        .expect("the registry stops on its Shutdown");

    let mut last = None;
    let mut lost = Vec::new();
    while let Some(ev) = tokio::time::timeout(WAIT, samples.recv())
        .await
        .expect("the channel closes once the registry is gone")
    {
        match ev {
            MetricsEvent::Registry(s) => last = Some(s),
            MetricsEvent::VerdictsLost(v) => lost.push(v),
            _ => {}
        }
    }
    let [v] = lost[..] else {
        panic!("one VerdictsLost: {lost:?}");
    };
    assert_eq!(v.closes.get(ServerClose::IdleInput), 1, "the close verdict");
    assert_eq!(v.closes.total(), 1);
    assert_eq!((v.leaves, v.detach_despawns), (1, 1));
    let s = last.expect("the registry's final sample");
    assert_eq!(
        (s.joins_unread, s.team_exports_unread),
        (2, 1),
        "two joins, one live export"
    );
    assert!(
        joined3.await.is_err() && joined4.await.is_err(),
        "the replies drop"
    );
    let told = tokio::time::timeout(WAIT, late_inbox.recv()).await;
    assert!(
        matches!(told, Ok(Some(ConnIn::Shutdown))),
        "the late connection is told to stop: {told:?}"
    );
}

/// The final sample goes out past a FULL metrics channel (the
/// stop-message idiom) and is folded into the collector's final report,
/// which waits for the registry's sender — the last one — to drop. A
/// plain `try_send` loses it here: the report has no registry slice.
#[tokio::test(start_paused = true)]
async fn the_final_sample_is_in_the_final_report_past_a_full_channel() {
    let (ticker, ticker_task) = Ticker::spawn(60.0, 64).expect("valid tick rate");
    // The server's stop aborts the ticker; the broadcast then closes when
    // the registry drops its `Ticker`. No tick is ever sent, so the
    // collector reads nothing before the registry is done.
    ticker_task.abort();
    let (metrics, events) = mpsc::channel(1);
    metrics
        .try_send(MetricsEvent::JoinRefusedClosed)
        .expect("fills the one slot");
    let (sink, mut reports) = mpsc::unbounded_channel::<MetricReport>();
    let collector = tokio::spawn(
        MetricsCollector::new(
            ticker.subscribe(),
            events,
            MetricSink::Channel(sink),
            Duration::from_secs(1),
        )
        .run(),
    );
    let (reg, tx, _shards) = registry(metrics, ticker);
    queue(&tx, RegistryMsg::Shutdown);
    queue(&tx, export(5, 0));
    let (j, _joined) = join(3);
    queue(&tx, j);
    tokio::spawn(reg.run());

    let complete = tokio::time::timeout(WAIT, collector)
        .await
        .expect("the collector ends")
        .expect("the collector task");
    assert!(complete, "every producer ended before the final report");
    let last = std::iter::from_fn(|| reports.try_recv().ok())
        .last()
        .expect("a final report");
    let r = last.registry.expect("the registry's final sample is in it");
    assert_eq!((r.joins_unread, r.team_exports_unread), (1, 1));
    assert_eq!(r.joins_refused_closed, 1, "the filler, folded too");
}
