//! The rUDP idle sweep against a stalled process (BACKLOG F72), on a
//! demux driven directly: the sweep runs at a chosen `now`, so a stall
//! is a sweep far past the deadlines — exactly what the demux sees when
//! it wakes from one.

use super::super::*;
use gsb_core::conn::ConnIn;
use std::net::SocketAddr;

use gsb_core::channel::Inbox;
use gsb_core::conn::ServerClose;
use gsb_core::metrics::MetricsEvent;
use tokio::sync::mpsc;

use crate::pump::IDLE_STALL_GRACE;

const W: Duration = Duration::from_secs(2);

/// A demux with a 2 s idle window and a metrics channel.
async fn demux() -> (Demux, mpsc::Receiver<MetricsEvent>) {
    let sock = UdpSocket::bind("127.0.0.1:0".parse::<SocketAddr>().unwrap())
        .await
        .expect("bind");
    let (end_tx, _end_rx) = crossbeam_channel::bounded(1);
    let key = CookieKey::generate().expect("OS entropy in test");
    let mut d = Demux::new(Arc::new(sock), end_tx, key, 16, 16, 1200, Some(W));
    let (tx, rx) = mpsc::channel(16);
    d.metrics = Some(tx);
    (d, rx)
}

/// A session for `port`, last heard at `seen` (its deadline armed the
/// way a datagram arms it); its actor's inbox.
fn install(d: &mut Demux, port: u16, seen: Instant) -> (SocketAddr, Inbox<ConnIn>) {
    let peer = SocketAddr::from(([127, 0, 0, 1], port));
    let (in_tx, in_rx) = gsb_core::channel::channel(4);
    let (out_tx, _out_rx) = gsb_core::channel::channel(4);
    let key = d
        .sessions
        .insert(UdpSession::new(peer, None, in_tx, out_tx, seen));
    d.deadlines.insert((seen + W, key));
    (peer, in_rx)
}

/// A datagram from `peer` at `at` (what `inbound` does to the clock).
fn heard(d: &mut Demux, peer: SocketAddr, at: Instant) {
    let key = d.sessions.key_at(&peer).expect("live");
    d.sessions.get_mut(key).expect("live").last_seen = at;
    d.deadlines.insert((at + W, key));
}

/// Whether the session was closed idle (its actor told, its row gone).
fn closed_idle(d: &Demux, peer: SocketAddr, inbox: &mut Inbox<ConnIn>) -> bool {
    let told = matches!(
        inbox.try_recv(),
        Ok(ConnIn::ServerClosed {
            cause: ServerClose::IdleTimeout,
            ..
        })
    );
    told && !d.sessions.contains_key(&peer)
}

/// Restarted windows counted so far.
fn restarts(rx: &mut mpsc::Receiver<MetricsEvent>) -> u64 {
    let mut n = 0;
    while let Ok(ev) = rx.try_recv() {
        if let MetricsEvent::Transport(t) = ev {
            n += t.idle_windows_restarted_late;
        }
    }
    n
}

/// THE PROPERTY: a deadline found a second overdue restarts the window
/// (the session stays, counted); a client still silent is closed when
/// the restarted window ends.
#[tokio::test]
async fn a_stall_restarts_the_window_once_and_counts() {
    let (mut d, mut m) = demux().await;
    let t0 = Instant::now();
    let (peer, mut inbox) = install(&mut d, 40_001, t0);
    let wake = t0 + W + Duration::from_secs(1);
    d.sweep_at(wake);
    assert!(
        d.sessions.contains_key(&peer),
        "the stall is not the client's silence"
    );
    assert!(inbox.is_empty(), "nothing told");
    assert_eq!(restarts(&mut m), 1);
    d.sweep_at(wake + W - Duration::from_millis(1));
    assert!(
        d.sessions.contains_key(&peer),
        "the restarted window is whole"
    );
    d.sweep_at(wake + W);
    assert!(
        closed_idle(&d, peer, &mut inbox),
        "silent through the restart"
    );
    assert_eq!((d.swept_idle, restarts(&mut m)), (1, 0));
}

/// Once per silence: the restarted window firing late again closes.
#[tokio::test]
async fn a_second_late_fire_closes() {
    let (mut d, mut m) = demux().await;
    let t0 = Instant::now();
    let (peer, mut inbox) = install(&mut d, 40_002, t0);
    let wake = t0 + W + Duration::from_secs(1);
    d.sweep_at(wake);
    d.sweep_at(wake + W + Duration::from_secs(3));
    assert!(closed_idle(&d, peer, &mut inbox));
    assert_eq!(restarts(&mut m), 1);
}

/// A datagram re-arms the restart: the next stall restarts again, and
/// the restart's own entry is then stale.
#[tokio::test]
async fn a_datagram_rearms_the_restart() {
    let (mut d, mut m) = demux().await;
    let t0 = Instant::now();
    let (peer, inbox) = install(&mut d, 40_003, t0);
    let wake = t0 + W + Duration::from_secs(1);
    d.sweep_at(wake);
    let t1 = wake + Duration::from_millis(500);
    heard(&mut d, peer, t1);
    d.sweep_at(t1 + W + Duration::from_secs(1));
    assert!(d.sessions.contains_key(&peer), "restarted again");
    assert!(inbox.is_empty());
    assert_eq!(restarts(&mut m), 2);
    assert_eq!(d.deadlines.len(), 1, "only the second restart is armed");
}

/// On time — or late by no more than the grace — closes as before.
#[tokio::test]
async fn an_on_time_deadline_closes_and_the_grace_is_the_boundary() {
    let (mut d, mut m) = demux().await;
    let t0 = Instant::now();
    let (a, mut a_in) = install(&mut d, 40_004, t0);
    let (b, mut b_in) = install(&mut d, 40_005, t0 + Duration::from_secs(1));
    let (c, _c_in) = install(&mut d, 40_006, t0 + Duration::from_secs(2));
    d.sweep_at(t0 + W);
    assert!(closed_idle(&d, a, &mut a_in), "on time");
    d.sweep_at(t0 + Duration::from_secs(1) + W + IDLE_STALL_GRACE);
    assert!(closed_idle(&d, b, &mut b_in), "late by the grace exactly");
    assert_eq!(restarts(&mut m), 0);
    let ms = Duration::from_millis(1);
    d.sweep_at(t0 + Duration::from_secs(2) + W + IDLE_STALL_GRACE + ms);
    assert!(d.sessions.contains_key(&c), "a millisecond past it");
    assert_eq!(restarts(&mut m), 1);
}

/// One sweep, one sample: every window it restarts, together.
#[tokio::test]
async fn one_sweep_counts_every_restart() {
    let (mut d, mut m) = demux().await;
    let t0 = Instant::now();
    for port in 40_010..40_013 {
        install(&mut d, port, t0);
    }
    d.sweep_at(t0 + W * 3);
    assert_eq!(d.sessions.len(), 3);
    let Ok(MetricsEvent::Transport(t)) = m.try_recv() else {
        panic!("one sample");
    };
    assert_eq!(t.idle_windows_restarted_late, 3);
    assert!(m.try_recv().is_err(), "only one");
    d.sweep_at(t0 + W * 4);
    assert!(d.sessions.is_empty(), "all silent through the restart");
}

/// A restart's entry never closes a NEW session under the same address
/// (the old one removed, the peer back with a fresh handshake).
#[tokio::test]
async fn a_restart_entry_does_not_outlive_its_session() {
    let (mut d, _m) = demux().await;
    let t0 = Instant::now();
    let (peer, _inbox) = install(&mut d, 40_020, t0);
    let wake = t0 + W + Duration::from_secs(1);
    d.sweep_at(wake);
    d.remove_session(d.sessions.key_at(&peer).expect("live"));
    let (_, fresh) = install(&mut d, 40_020, wake + Duration::from_millis(10));
    d.sweep_at(wake + W);
    assert!(
        d.sessions.contains_key(&peer),
        "the fresh session is not due"
    );
    assert!(fresh.is_empty());
}
