//! The kernel's drops on the door's one socket (BACKLOG B85).
//!
//! ONE socket carries every session, and a datagram that arrives while
//! its receive queue is full is dropped by the kernel before the demux
//! sees it — so no counter of the demux can see it. Linux counts it per
//! socket (`sk_drops`) and shows it in the `drops` column of
//! `/proc/net/udp` (`/proc/net/udp6` for an IPv6 socket), on the line
//! whose `inode` is the socket's. Until B85 the only view was the
//! system-wide `Udp: RcvbufErrors` of `/proc/net/snmp`, which mixes every
//! UDP socket of the host (a load generator's clients included).
//!
//! **Decision: poll the socket's `/proc` line, off the demux.** At bind
//! the door finds its socket's inode (`/proc/self/fd/<fd>` reads
//! `socket:[<inode>]`) and, when the door reports metrics, spawns one
//! small task that reads its line every [`KERNEL_POLL`] and hands the
//! growth to the collector as `udp_datagrams_dropped_kernel` (the
//! transport scope's path, `crate::metrics::Flusher`). One awaited
//! source (the sleep); the read is a few kilobytes of procfs per second,
//! outside the demux's per-datagram path. It stops when its line is gone
//! (the socket closed) or when the listener closes (it is aborted with
//! the demux, and its `Drop` reads the line one last time). The column
//! is a `u32` that wraps; deltas are taken in wrapping arithmetic.
//!
//! **Linux only, documented:** elsewhere (and where `/proc` cannot be
//! read — a warning at bind) no task is spawned and the counter stays 0;
//! everything else compiles and runs the same.
//!
//! **Rejected — `SO_RXQ_OVFL`** (the kernel attaches the socket's drop
//! count to every datagram as a control message): per-datagram and exact,
//! but setting the option needs a raw `setsockopt` that neither tokio nor
//! `socket2` 0.6 exposes (and `unsafe` is forbidden; `nix` is not in the
//! lock), and reading it turns the demux's `recv_from` into a `recvmsg`
//! with a control buffer — a change to the hottest path of the transport
//! for a counter a 1 s poll serves as well. **Rejected — the system-wide
//! `RcvbufErrors`:** it is what B85 replaces. **Rejected — reading the
//! line in the demux:** the procfs read would sit on the one task every
//! session shares, and grows with the host's number of UDP sockets.

use std::net::SocketAddr;
use std::time::Duration;

use gsb_core::metrics::TransportCounters;
use tokio::net::UdpSocket;
use tracing::{info, warn};

/// How often the kernel's count is read.
pub(super) const KERNEL_POLL: Duration = Duration::from_secs(1);

/// Where one socket's kernel drop count is read: its `/proc/net` table
/// and its inode.
#[derive(Debug, Clone)]
pub(super) struct ProcLine {
    table: &'static str,
    inode: u64,
}

impl ProcLine {
    /// The line of `sock` (Linux; an error elsewhere or without `/proc`).
    pub(super) fn of(sock: &UdpSocket) -> std::io::Result<Self> {
        Self::locate(sock, sock.local_addr()?)
    }

    #[cfg(target_os = "linux")]
    fn locate(sock: &UdpSocket, local: SocketAddr) -> std::io::Result<Self> {
        use std::os::fd::AsRawFd;
        let link = std::fs::read_link(format!("/proc/self/fd/{}", sock.as_raw_fd()))?;
        let inode = link
            .to_str()
            .and_then(socket_inode)
            .ok_or_else(|| std::io::Error::other(format!("not a socket link: {link:?}")))?;
        let table = match local {
            SocketAddr::V4(_) => "/proc/net/udp",
            SocketAddr::V6(_) => "/proc/net/udp6",
        };
        Ok(Self { table, inode })
    }

    #[cfg(not(target_os = "linux"))]
    fn locate(_sock: &UdpSocket, _local: SocketAddr) -> std::io::Result<Self> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "per-socket kernel drops are read from Linux's /proc/net/udp",
        ))
    }

    /// The socket's current `drops` column; `None` once its line is gone.
    pub(super) fn read(&self) -> std::io::Result<Option<u32>> {
        Ok(drops_of(&std::fs::read_to_string(self.table)?, self.inode))
    }
}

/// The inode of a `/proc/self/fd` socket link (`socket:[12345]`).
pub(super) fn socket_inode(link: &str) -> Option<u64> {
    link.strip_prefix("socket:[")?
        .strip_suffix(']')?
        .parse()
        .ok()
}

/// The `drops` column of the line whose `inode` column is `inode`, in a
/// `/proc/net/udp{,6}` table (the header line has no numeric inode).
pub(super) fn drops_of(table: &str, inode: u64) -> Option<u32> {
    table.lines().find_map(|line| {
        let mut cols = line.split_whitespace();
        // sl, local, remote, st, tx:rx, tr:when, retrnsmt, uid, timeout,
        // inode, ref, pointer, drops.
        if cols.nth(9)?.parse::<u64>().ok()? != inode {
            return None;
        }
        cols.nth(2)?.parse().ok()
    })
}

/// The poll task's state: the line, the last value read and the
/// cumulative total (the column wraps at `u32`).
pub(super) struct Watcher {
    line: ProcLine,
    seen: u32,
    total: u64,
    flusher: crate::metrics::Flusher,
}

impl Watcher {
    pub(super) fn new(line: ProcLine, metrics: crate::TransportMetrics) -> Self {
        Self {
            line,
            seen: 0,
            total: 0,
            flusher: crate::metrics::Flusher::new(metrics),
        }
    }

    /// Read the line once; `false` once the socket is gone (or `/proc`
    /// stopped answering).
    pub(super) fn poll(&mut self) -> bool {
        match self.line.read() {
            Ok(Some(now)) => {
                self.advance(now);
                true
            }
            Ok(None) | Err(_) => false,
        }
    }

    /// Take a new reading of the column (it wraps at `u32`).
    pub(super) fn advance(&mut self, now: u32) {
        self.total += u64::from(now.wrapping_sub(self.seen));
        self.seen = now;
    }

    /// Hand the growth to the collector (`last`: past a full channel).
    pub(super) fn flush(&mut self, last: bool) {
        let totals = TransportCounters {
            udp_datagrams_dropped_kernel: self.total,
            ..Default::default()
        };
        self.flusher.flush(totals, last);
    }

    /// The task: one awaited source, the sleep.
    pub(super) async fn run(mut self) {
        loop {
            tokio::time::sleep(KERNEL_POLL).await;
            let alive = self.poll();
            self.flush(false);
            if !alive {
                break;
            }
        }
    }
}

/// The last read, and the last word to the collector — also when the
/// listener's `close` aborts the task.
impl Drop for Watcher {
    fn drop(&mut self) {
        self.poll();
        self.flush(true);
        if self.total > 0 {
            info!(
                dropped = self.total,
                "rUDP: datagrams the kernel dropped on the door's full receive queue"
            );
        }
    }
}

/// Spawn the watcher of `sock` when the door reports metrics; `None`
/// (with a warning when `/proc` is the reason) otherwise.
pub(super) fn spawn(
    sock: &UdpSocket,
    metrics: &crate::TransportMetrics,
) -> Option<tokio::task::JoinHandle<()>> {
    metrics.as_ref()?;
    match ProcLine::of(sock) {
        Ok(line) => Some(tokio::spawn(Watcher::new(line, metrics.clone()).run())),
        Err(e) => {
            if e.kind() != std::io::ErrorKind::Unsupported {
                warn!(%e, "rUDP: the kernel's drops on the door's socket cannot be counted");
            }
            None
        }
    }
}

#[cfg(test)]
mod tests;
