//! The door's close (BACKLOG B16, B31) and its count (B74): the close
//! cuts every handshake in flight and drops what is queued. The
//! handshakes it cut count themselves in their own tasks, after the
//! close; the queue it dropped is counted by whoever drained it. The
//! intake task's LAST sample must come after both, so it waits until no
//! slot is held and no handshake task runs — bounded, since a close ends
//! every one of them at once.

use std::sync::atomic::Ordering;
use std::time::Duration;

use tracing::warn;

use crate::transport::intake::Intake;

/// How long the intake task waits for its closed door to settle. Every
/// handshake in flight is cut by the close itself, so this is a bound on
/// scheduling, not on any peer.
const SETTLE: Duration = Duration::from_secs(1);

impl Intake {
    /// Close the door: the raw accept, every handshake in flight and the
    /// pending accept end, and what is queued is dropped.
    pub(crate) fn close(&self) {
        self.door.close();
        self.drain();
    }

    /// Drop what is queued, counting each (B74) BEFORE its slot goes, so
    /// a settled door (no slot held) has counted everything it dropped.
    pub(super) fn drain(&self) {
        while let Ok(ready) = self.queue_rx.try_recv() {
            self.unaccepted.fetch_add(1, Ordering::Relaxed);
            drop(ready);
        }
    }

    /// Wait (bounded) until the closed door holds no slot and runs no
    /// handshake task: every count of its close is then made. Called by
    /// the intake task before its last flush, only once the door is
    /// closed (an endpoint closed under an open door leaves its queue to
    /// the accept loop).
    pub(crate) async fn settle(&self) {
        if !self.door.is_closed() {
            return;
        }
        let quiet = async {
            // `notify_one` keeps a wake-up for a waiter not yet parked, so
            // a release between the check and the wait is not missed.
            while self.held.load(Ordering::Acquire) > 0 || self.live.load(Ordering::Acquire) > 0 {
                self.quiet.notified().await;
            }
        };
        if tokio::time::timeout(SETTLE, quiet).await.is_err() {
            warn!(
                door = self.kind,
                "handshakes still running after the close; its last count may miss them"
            );
        }
    }
}
