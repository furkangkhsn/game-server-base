//! One connection's handshake task, and the queue its finished endpoint
//! takes to the accept loop (`Listener::accept`).

use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use tracing::{debug, warn};

use crate::transport::Endpoint;
use crate::transport::intake::{Intake, Ready, Slot};

impl Intake {
    /// Run one connection's handshake in its own task: ONE awaited
    /// future — the handshake under its deadline, under the door.
    pub(crate) fn spawn<F>(
        self: &Arc<Self>,
        slot: Slot,
        peer: SocketAddr,
        deadline: Duration,
        handshake: F,
    ) where
        F: Future<Output = io::Result<Endpoint>> + Send + 'static,
    {
        let intake = Arc::clone(self);
        intake.live.fetch_add(1, Ordering::AcqRel);
        tokio::spawn(async move {
            let deadlined = async { Ok(tokio::time::timeout(deadline, handshake).await) };
            // Each count is made once the slot has moved on (queued or
            // released), so a reader of the counters never sees it early.
            match intake.door.admit(deadlined).await {
                Ok(Ok(Ok(endpoint))) => {
                    debug!(door = intake.kind, %peer, "handshake completed");
                    intake.hand_over(Ready {
                        endpoint,
                        _slot: slot,
                    });
                    intake.completed.fetch_add(1, Ordering::Relaxed);
                }
                Ok(Ok(Err(e))) => {
                    drop(slot);
                    intake.failed.fetch_add(1, Ordering::Relaxed);
                    warn!(door = intake.kind, %peer, error = %e, "handshake failed; closing");
                }
                Ok(Err(_)) => {
                    drop(slot);
                    intake.timed_out.fetch_add(1, Ordering::Relaxed);
                    warn!(door = intake.kind, %peer, timeout = ?deadline, "handshake timed out; closing");
                }
                Err(_) => {
                    drop(slot);
                    intake.cut.fetch_add(1, Ordering::Relaxed);
                    debug!(door = intake.kind, %peer, "door closed; handshake cut");
                }
            }
            // Last: every count of this task is made (`close::settle`).
            if intake.live.fetch_sub(1, Ordering::AcqRel) == 1 {
                intake.quiet.notify_one();
            }
        });
    }

    /// Queue a finished endpoint for the accept loop. Never full (the
    /// slots bound it); a close that raced the handshake drops it again.
    fn hand_over(&self, ready: Ready) {
        if self.queue_tx.try_send(ready).is_ok() {
            self.ready.notify_one();
        }
        if self.door.is_closed() {
            self.drain();
        }
    }

    /// The next finished endpoint (`Listener::accept`), or the closed
    /// error once the door is closed.
    pub(crate) async fn next(self: Arc<Self>) -> io::Result<Endpoint> {
        let queued = async {
            loop {
                if let Ok(Ready { endpoint, .. }) = self.queue_rx.try_recv() {
                    return Ok(endpoint);
                }
                // `notify_one` stores a wake-up when nobody waits yet,
                // so an endpoint queued between the check and this wait
                // is not missed.
                self.ready.notified().await;
            }
        };
        self.door.admit(queued).await
    }
}
