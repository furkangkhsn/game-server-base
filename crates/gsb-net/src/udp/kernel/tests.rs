//! The kernel's drops on the door's socket (BACKLOG B85): the `/proc`
//! line is found and parsed, a socket nobody reads reports exactly the
//! datagrams it lost, and the door's watcher hands them to the
//! collector. The socket tests are Linux's (elsewhere the counter is 0
//! and only the parser runs).

use super::*;

/// A `/proc/net/udp` excerpt: the header, two sockets.
const TABLE: &str = "   sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode ref pointer drops
 3547: 00000000:82DE 00000000:0000 07 00000000:00000000 00:00000000 00000000     0        0 5988 2 000000001ec94cdd 0
13166: 0100007F:A871 00000000:0000 07 00000000:00002400 00:00000000 00000000  1000        0 7480703 2 00000000a6090ef3 4294967295
";

#[test]
fn the_drops_column_of_the_sockets_own_line() {
    assert_eq!(drops_of(TABLE, 5988), Some(0));
    assert_eq!(drops_of(TABLE, 7480703), Some(u32::MAX));
    assert_eq!(
        drops_of(TABLE, 12),
        None,
        "no such socket: its line is gone"
    );
    assert_eq!(drops_of("", 5988), None);
    assert_eq!(
        drops_of("1: 2 3 4 5 6 7 8 9 5988", 5988),
        None,
        "a short line"
    );
}

#[test]
fn the_inode_of_a_socket_link() {
    assert_eq!(socket_inode("socket:[7480703]"), Some(7480703));
    assert_eq!(socket_inode("pipe:[7480703]"), None);
    assert_eq!(socket_inode("socket:[x]"), None);
}

/// The column is a `u32` that wraps: the total keeps counting through it.
#[test]
fn the_total_counts_through_the_columns_wrap() {
    let line = ProcLine {
        table: "/nonexistent",
        inode: 1,
    };
    let mut w = Watcher::new(line, None);
    w.advance(u32::MAX - 1);
    w.advance(3);
    assert_eq!(w.total, u64::from(u32::MAX) + 4);
    assert!(!w.poll(), "an unreadable table ends the watcher");
}

/// Flood `to` with `n` datagrams of 1 KB, synchronously (no await: in a
/// current-thread runtime nothing reads meanwhile).
#[cfg(target_os = "linux")]
fn flood(to: SocketAddr, n: usize) {
    let from = std::net::UdpSocket::bind("127.0.0.1:0").expect("bind");
    for _ in 0..n {
        from.send_to(&[0xEE; 1000], to).expect("loopback send");
    }
}

/// A socket nobody reads, its queue one page: what it could not queue
/// is exactly its `drops` column (loopback loses nothing else).
#[cfg(target_os = "linux")]
#[tokio::test]
async fn a_full_queue_shows_exactly_its_drops() {
    let small = crate::listen::UdpBuffers {
        recv: Some(4096),
        send: None,
    };
    let any: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let sock =
        UdpSocket::from_std(crate::listen::bind_udp(any, small).expect("bind")).expect("tokio");
    let line = ProcLine::of(&sock).expect("its /proc line");
    assert_eq!(
        line.read().expect("read"),
        Some(0),
        "a new socket lost nothing"
    );
    flood(sock.local_addr().unwrap(), 64);
    // Drained on the std socket (non-blocking, as tokio left it): no
    // readiness bookkeeping between the queue and the count.
    let sock = sock.into_std().expect("std");
    let mut buf = [0u8; 2048];
    let mut queued = 0;
    while sock.recv_from(&mut buf).is_ok() {
        queued += 1;
    }
    assert!(queued < 64, "a one-page queue cannot hold 64 KB");
    assert_eq!(line.read().expect("read"), Some(64 - queued));
    drop(sock);
    assert_eq!(
        line.read().expect("read"),
        None,
        "a closed socket's line is gone"
    );
}

/// A door with a one-page queue that reports to `tx`, flooded while its
/// demux cannot run (this runtime has one thread and the flood never
/// yields): the listener, and the drops its socket's line shows.
#[cfg(target_os = "linux")]
async fn flooded_door(
    tx: tokio::sync::mpsc::Sender<gsb_core::metrics::MetricsEvent>,
) -> (std::sync::Arc<dyn crate::transport::Listener>, u64) {
    use crate::transport::Transport;
    let config = crate::udp::UdpTransportConfig {
        buffers: crate::listen::UdpBuffers {
            recv: Some(4096),
            send: None,
        },
        metrics: Some(tx),
        ..Default::default()
    };
    let transport = std::sync::Arc::new(crate::udp::UdpTransport { config });
    let listener = transport
        .bind("127.0.0.1:0".parse().unwrap())
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    flood(addr, 64);
    let table = std::fs::read_to_string("/proc/net/udp").expect("/proc/net/udp");
    let port = format!(":{:04X} ", addr.port());
    let dropped: u64 = table
        .lines()
        .find(|l| {
            l.split_whitespace()
                .nth(1)
                .is_some_and(|c| c.ends_with(port.trim_end()))
        })
        .and_then(|l| l.split_whitespace().nth(12)?.parse().ok())
        .expect("the door's line");
    assert!(dropped > 0, "the one-page queue overflowed");
    (listener, dropped)
}

/// The drops `rx` reports, summed until they reach `want` (a condition
/// wait, 10 s at most).
#[cfg(target_os = "linux")]
async fn reported(
    rx: &mut tokio::sync::mpsc::Receiver<gsb_core::metrics::MetricsEvent>,
    want: u64,
) -> u64 {
    use gsb_core::metrics::MetricsEvent;
    let mut reported = 0;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while reported < want {
        match tokio::time::timeout_at(deadline, rx.recv()).await {
            Ok(Some(MetricsEvent::Transport(t))) => reported += t.udp_datagrams_dropped_kernel,
            Ok(Some(_)) => {}
            Ok(None) | Err(_) => break,
        }
    }
    reported
}

/// The door's watcher, end to end: the collector receives exactly the
/// drops the door's socket line shows, from the periodic read.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn the_door_reports_its_sockets_kernel_drops() {
    let (tx, mut rx) = tokio::sync::mpsc::channel(256);
    let (listener, dropped) = flooded_door(tx).await;
    assert_eq!(
        reported(&mut rx, dropped).await,
        dropped,
        "every kernel drop, counted once"
    );
    listener.close();
}

/// A door closed before the watcher's first read still reports them: the
/// aborted watcher reads the line one last time as it goes.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn a_closing_door_reports_its_last_kernel_drops() {
    let (tx, mut rx) = tokio::sync::mpsc::channel(256);
    let (listener, dropped) = flooded_door(tx).await;
    listener.close();
    assert_eq!(reported(&mut rx, dropped).await, dropped);
}

/// The listener's close stops the watcher (BACKLOG B96): once the door
/// is closed — its listener still held, so its socket and line live on
/// — the metrics channel's last sender goes with the aborted watcher,
/// and the channel closes, its last drops reported. A watcher the close
/// left running would read the line every second for as long as the
/// handle lives, and the channel would never close.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn a_closed_door_stops_its_watcher() {
    use gsb_core::metrics::MetricsEvent;
    let (tx, mut rx) = tokio::sync::mpsc::channel(256);
    let (listener, dropped) = flooded_door(tx).await;
    listener.close();
    let mut reported = 0;
    let closed = tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(ev) = rx.recv().await {
            if let MetricsEvent::Transport(t) = ev {
                reported += t.udp_datagrams_dropped_kernel;
            }
        }
    })
    .await;
    assert!(closed.is_ok(), "the watcher outlived the close");
    assert_eq!(reported, dropped);
    drop(listener);
}
