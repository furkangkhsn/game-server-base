//! Game services and their explicit stop (BACKLOG F5, DESIGN §9.2).
//!
//! A *service* is a long-lived task a game runs next to its rooms — the
//! demo's economy service is the reference: rooms reach it through a
//! cloned handle over a bounded mailbox and get answers on oneshots. Left
//! alone, such a task ends only when its last sender drops, which makes
//! its end implicit: nothing orders it after the rooms' teardown hooks
//! (`on_shutdown`, `match_result` — the last chance to settle with the
//! service), and nothing stops it from outliving the server's stop.
//!
//! Two building blocks make the end explicit, both synchronous for the
//! caller:
//!
//! - [`Service`]: the service's task plus a stop REQUEST, a plain
//!   `FnOnce` (typically [`crate::channel::post`] of the service's own
//!   stop message — the actor's `Shutdown` idiom). The composition root
//!   calls it only after every room task has ended, then joins the task
//!   under a deadline and aborts it past that deadline. A service that
//!   honours the request serves whatever the rooms queued before it,
//!   then ends.
//! - [`hold`]: a drop barrier — cloneable [`Hold`] tokens and one
//!   [`Released`] waiter that completes when the last token drops. The
//!   registry's death watchers each keep one until their room task ends
//!   (see `Registry::with_rooms_hold`), which is how the composition root
//!   learns "every room has run its teardown" without ever awaiting a
//!   room. A service can use one for its own in-flight work.
//!
//! No select, no lock: the waiter's one await is a receive that can only
//! end (nothing is ever sent — the item type is uninhabited).

use std::convert::Infallible;
use std::fmt;

use tokio::sync::mpsc;
use tokio::task::JoinHandle;

/// A service's task and its stop request (see the module docs).
pub struct Service {
    name: &'static str,
    task: JoinHandle<()>,
    stop: Box<dyn FnOnce() + Send>,
}

impl Service {
    /// A service named `name` (logs and reports), running as `task`,
    /// stopped by calling `stop`. `stop` must not block: it asks, the
    /// caller waits (bounded) for `task`.
    pub fn new(
        name: &'static str,
        task: JoinHandle<()>,
        stop: impl FnOnce() + Send + 'static,
    ) -> Self {
        Self {
            name,
            task,
            stop: Box::new(stop),
        }
    }

    /// The service's name.
    pub fn name(&self) -> &'static str {
        self.name
    }

    /// Ask the service to stop, handing back its task to wait on. Dropping
    /// a `Service` instead neither asks nor waits: the task keeps its
    /// pre-F5 life (it ends when its last sender drops).
    pub fn request_stop(self) -> JoinHandle<()> {
        (self.stop)();
        self.task
    }
}

impl fmt::Debug for Service {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Service")
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

/// A token of the drop barrier (see [`hold`]). Clone it into whatever
/// must end before the waiter proceeds; dropping it is the release.
#[derive(Clone, Debug)]
pub struct Hold(#[allow(dead_code)] mpsc::Sender<Infallible>);

/// The drop barrier's waiter (see [`hold`]).
#[derive(Debug)]
pub struct Released(mpsc::Receiver<Infallible>);

/// A drop barrier: [`Released::wait`] completes once the returned [`Hold`]
/// and every clone of it are dropped.
pub fn hold() -> (Hold, Released) {
    let (tx, rx) = mpsc::channel(1);
    (Hold(tx), Released(rx))
}

impl Released {
    /// Wait until every [`Hold`] is gone (immediately if they already
    /// are). Bound it with a deadline where the holders are not trusted.
    pub async fn wait(mut self) {
        // Nothing can be sent (the item type is uninhabited): the only
        // way this receive ends is every sender dropping.
        while self.0.recv().await.is_some() {}
    }
}

#[cfg(test)]
mod tests;
