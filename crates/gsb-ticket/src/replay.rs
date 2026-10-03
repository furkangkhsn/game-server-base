//! The single-use guard (opt-in): a ticket id seen once is refused the
//! second time (`replayed`) until its ticket has expired.
//!
//! **Where the seen-set lives.** The validator runs in a spawned worker
//! per validating connection, so the set is shared — and the workspace
//! has no locks: it is owned by ONE small actor. A validation sends
//! `(jti, keep_until, now)` and awaits the verdict on a oneshot; the
//! actor's loop awaits only its inbox. One check is a hash lookup and an
//! insert, so the single actor is not a bottleneck next to the Ed25519
//! verification each worker already did.
//!
//! **Bounded memory.** At most `capacity` ids; each is kept until its
//! ticket's expiry plus the skew allowance (after that the ticket is
//! refused as `expired` anyway) and pruned on the next check. A full set
//! — more live single-use tickets than the bound — refuses
//! (`replay_unavailable`), as does a full inbox or a guard that is gone:
//! the guard fails CLOSED, and every such refusal is counted.
//!
//! **Scope.** One process. Two servers of the same audience do not share
//! the set: a ticket meant for one server is single-use per server. A
//! deployment that needs more pins the room (the ticket's `room` already
//! names one server's room) or keeps the default reusable policy.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};

use gsb_core::auth::TicketReason;
use tokio::sync::{mpsc, oneshot};

/// The guard's inbox depth: validations waiting for the actor. A burst
/// beyond it is refused (`replay_unavailable`), never queued unbounded.
pub const INBOX: usize = 1024;

/// One check: is `jti` fresh? Kept until `keep_until` (Unix seconds).
struct Check {
    jti: String,
    keep_until: i64,
    now: i64,
    reply: oneshot::Sender<Result<(), TicketReason>>,
}

/// A handle to the single-use guard's actor (cheap to clone).
#[derive(Debug, Clone)]
pub struct ReplayGuard {
    tx: mpsc::Sender<Check>,
}

impl ReplayGuard {
    /// Spawn the guard's actor, remembering at most `capacity` ticket ids.
    /// Must be called inside a tokio runtime. The actor ends when the
    /// last handle is dropped.
    pub fn spawn(capacity: usize) -> Self {
        let (tx, rx) = mpsc::channel(INBOX);
        tokio::spawn(run(rx, capacity));
        Self { tx }
    }

    /// Admit `jti` once: `Ok` the first time, [`TicketReason::Replayed`]
    /// after, [`TicketReason::ReplayUnavailable`] when the guard cannot
    /// answer (its set or inbox is full, or it is gone).
    pub async fn admit(&self, jti: &str, keep_until: i64, now: i64) -> Result<(), TicketReason> {
        let (reply, verdict) = oneshot::channel();
        let check = Check {
            jti: jti.to_owned(),
            keep_until,
            now,
            reply,
        };
        self.tx
            .try_send(check)
            .map_err(|_| TicketReason::ReplayUnavailable)?;
        verdict
            .await
            .unwrap_or(Err(TicketReason::ReplayUnavailable))
    }
}

/// The actor: the seen-set and its expiry order.
async fn run(mut rx: mpsc::Receiver<Check>, capacity: usize) {
    let mut seen: HashMap<String, i64> = HashMap::new();
    let mut order: BinaryHeap<Reverse<(i64, String)>> = BinaryHeap::new();
    while let Some(c) = rx.recv().await {
        // Prune every id whose ticket can no longer be accepted.
        while let Some(Reverse((until, _))) = order.peek() {
            if *until >= c.now {
                break;
            }
            if let Some(Reverse((until, jti))) = order.pop()
                && seen.get(&jti) == Some(&until)
            {
                seen.remove(&jti);
            }
        }
        let verdict = if seen.contains_key(&c.jti) {
            Err(TicketReason::Replayed)
        } else if seen.len() >= capacity {
            Err(TicketReason::ReplayUnavailable)
        } else {
            seen.insert(c.jti.clone(), c.keep_until);
            order.push(Reverse((c.keep_until, c.jti)));
            Ok(())
        };
        // The validation may have timed out meanwhile: nobody to tell.
        let _ = c.reply.send(verdict);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn an_id_is_admitted_once_until_its_ticket_expires() {
        let g = ReplayGuard::spawn(8);
        assert_eq!(g.admit("a", 100, 10).await, Ok(()));
        assert_eq!(g.admit("a", 100, 50).await, Err(TicketReason::Replayed));
        assert_eq!(g.admit("b", 100, 50).await, Ok(()));
        // Past `keep_until` the id is forgotten (its ticket is refused as
        // expired before it gets here).
        assert_eq!(g.admit("a", 300, 101).await, Ok(()));
    }

    #[tokio::test]
    async fn a_full_set_fails_closed_and_frees_by_expiry() {
        let g = ReplayGuard::spawn(2);
        assert_eq!(g.admit("a", 100, 0).await, Ok(()));
        assert_eq!(g.admit("b", 200, 0).await, Ok(()));
        let full = Err(TicketReason::ReplayUnavailable);
        assert_eq!(g.admit("c", 300, 0).await, full);
        assert_eq!(g.admit("c", 300, 101).await, Ok(()), "a's slot freed");
        assert_eq!(g.admit("b", 300, 101).await, Err(TicketReason::Replayed));
    }
}
