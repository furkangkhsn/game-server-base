//! A connection whose transport moved it to a new client address (an
//! rUDP migration, BACKLOG B113): its unauthenticated count follows it
//! to the new source when that source has room; into a source at its cap
//! the count stays where it was, counted — the move frees nothing the
//! new source cannot take, and strands no one.

use super::*;
use gsb_core::metrics::MetricsEvent;

/// A registry with the per-source cap `cap`, and its metric samples.
fn start_counted(
    cap: u64,
) -> (
    Mailbox<RegistryMsg>,
    JoinHandle<()>,
    mpsc::Receiver<MetricsEvent>,
) {
    let (tx, rx) = channel::<RegistryMsg>(256);
    let (ticker, _ticker) = Ticker::spawn(60.0, 64).expect("valid tick rate");
    let (metrics, samples) = mpsc::channel(4096);
    let factory: RoomFactory<(), (), (), ()> =
        Arc::new(|_id, _cfg| unreachable!("no room is created here"));
    let task = tokio::spawn(
        Registry::new(rx, tx.clone(), factory, ticker, metrics, None, None, None)
            .with_unauth_per_source(Some(cap))
            .run(),
    );
    (tx, task, samples)
}

async fn moved(tx: &Mailbox<RegistryMsg>, n: u64, to: &str) {
    tx.send(RegistryMsg::ConnPeerChanged {
        conn: ConnectionId(n),
        source: Source::of(to.parse::<IpAddr>().expect("an address")),
    })
    .await
    .expect("registry alive");
}

/// The registry's latest `unauth_source_moves_kept`.
fn kept(samples: &mut mpsc::Receiver<MetricsEvent>) -> u64 {
    let mut last = 0;
    while let Ok(ev) = samples.try_recv() {
        if let MetricsEvent::Registry(s) = ev {
            last = s.unauth_source_moves_kept;
        }
    }
    last
}

#[tokio::test]
async fn a_moved_session_counts_against_its_new_source() {
    let (tx, task, mut samples) = start_counted(1);
    let mut a = open(&tx, 1, Some("10.0.0.1")).await;
    assert_eq!(verdict(&tx, &mut a).await, None);
    moved(&tx, 1, "10.0.0.2").await;
    let mut a2 = open(&tx, 2, Some("10.0.0.1")).await;
    assert_eq!(verdict(&tx, &mut a2).await, None, "its old source is free");
    let mut b = open(&tx, 3, Some("10.0.0.2")).await;
    assert_eq!(verdict(&tx, &mut b).await, OVER, "its new source holds it");
    // Its close gives the new source's place back.
    tx.send(RegistryMsg::ConnClosed {
        conn: ConnectionId(1),
        verdict: None,
    })
    .await
    .expect("registry alive");
    let mut b2 = open(&tx, 4, Some("10.0.0.2")).await;
    assert_eq!(verdict(&tx, &mut b2).await, None, "closed: given back");
    assert_eq!(kept(&mut samples), 0);
    stop(tx, task).await;
}

#[tokio::test]
async fn a_move_into_a_full_source_keeps_its_count_where_it_was() {
    let (tx, task, mut samples) = start_counted(1);
    let mut a = open(&tx, 1, Some("10.0.0.1")).await;
    let mut b = open(&tx, 2, Some("10.0.0.2")).await;
    assert_eq!(verdict(&tx, &mut a).await, None);
    assert_eq!(verdict(&tx, &mut b).await, None);
    // 1 moves into 10.0.0.2, which is full: its count stays at 10.0.0.1.
    moved(&tx, 1, "10.0.0.2").await;
    let mut a2 = open(&tx, 3, Some("10.0.0.1")).await;
    assert_eq!(
        verdict(&tx, &mut a2).await,
        OVER,
        "the old source still holds it"
    );
    let mut b2 = open(&tx, 4, Some("10.0.0.2")).await;
    assert_eq!(verdict(&tx, &mut b2).await, OVER, "and the new one its own");
    assert_eq!(kept(&mut samples), 1, "the kept move, counted");
    // Once the new source has room, the next move takes the count along.
    tx.send(RegistryMsg::Authed {
        conn: ConnectionId(2),
    })
    .await
    .expect("registry alive");
    moved(&tx, 1, "10.0.0.2").await;
    let mut a3 = open(&tx, 5, Some("10.0.0.1")).await;
    assert_eq!(verdict(&tx, &mut a3).await, None, "moved now");
    assert_eq!(kept(&mut samples), 1);
    stop(tx, task).await;
}

/// An authenticated connection is in no per-source count: it moves into
/// a full source freely, and nothing is kept or counted.
#[tokio::test]
async fn an_authenticated_session_moves_freely() {
    let (tx, task, mut samples) = start_counted(1);
    let mut a = open(&tx, 1, Some("10.0.0.1")).await;
    let mut b = open(&tx, 2, Some("10.0.0.2")).await;
    assert_eq!(verdict(&tx, &mut a).await, None);
    assert_eq!(verdict(&tx, &mut b).await, None);
    tx.send(RegistryMsg::Authed {
        conn: ConnectionId(1),
    })
    .await
    .expect("registry alive");
    moved(&tx, 1, "10.0.0.2").await;
    moved(&tx, 99, "10.0.0.3").await; // no such row: ignored
    let mut a2 = open(&tx, 3, Some("10.0.0.1")).await;
    assert_eq!(verdict(&tx, &mut a2).await, None);
    assert_eq!(kept(&mut samples), 0);
    stop(tx, task).await;
}
