//! The per-connection dispatcher's two join paths: the plain spawn,
//! and the resume broadcast that exactly one shard may answer.

use std::fmt::Debug;
use std::hash::Hash;

use tokio::sync::{mpsc, oneshot};

use crate::channel::{FrameBatch, Mailbox};
use crate::error::CoreError;
use crate::id::{ConnectionId, EntityId, RoomId};
use crate::registry::*;
use crate::room::{Action, RoomControl};
use crate::shard::{ResumeReply, ShardMsg};

use crate::registry::actor::Registry;

#[cfg(test)]
mod tests;

impl<W, G, St, Sp> Registry<W, G, St, Sp>
where
    W: Send + 'static,
    G: Eq + Hash + Clone + Debug + Send + 'static,
    St: Debug + Send + 'static,
    // The strip payload's trait bounds (`GameLogic::Strip`) — the
    // registry never inspects payloads, but both actor shapes it spawns
    // require them.
    Sp: Debug + Clone + PartialEq + Send + 'static,
{
    /// The ordinary fresh join round trip (the pre-reconnect shape):
    /// single rooms get a control `Join`; sharded rooms a `ShardMsg::Join`
    /// on the home shard. Returns the outcome class the dispatcher acts on:
    /// a send the room refused is [`OpOutcome::Refused`] (B75), apart
    /// from a reply dropped after the room took the op.
    #[allow(clippy::too_many_arguments)] // the join's routing, its joiner, its channel
    pub(super) async fn dispatch_plain_join(
        conn: ConnectionId,
        _room: RoomId,
        handle: &RoomHandle<St, Sp>,
        shard: Option<usize>,
        epoch: u64,
        identity: String,
        claims: Option<bytes::Bytes>,
        out: mpsc::Sender<FrameBatch>,
    ) -> OpOutcome {
        let (joined_tx, joined_rx) =
            oneshot::channel::<Result<(EntityId, Mailbox<Action>), CoreError>>();
        let sent = match handle {
            RoomHandle::Single(control) => control
                .send(RoomControl::Join {
                    conn,
                    out,
                    reply: joined_tx,
                })
                .await
                .is_ok(),
            RoomHandle::Sharded(mailboxes) => {
                let i = shard.expect("sharded join carries its shard");
                mailboxes[i]
                    .send(ShardMsg::Join {
                        conn,
                        epoch,
                        identity,
                        claims,
                        out,
                        reply: joined_tx,
                    })
                    .await
                    .is_ok()
            }
        };
        Self::settle(sent, joined_rx).await
    }

    /// Fold one join round trip's send and reply: a refused send never
    /// reached the room (`Refused`), a dropped reply was taken and lost
    /// there (`Gone`, the room's stop counts it).
    async fn settle(
        sent: bool,
        reply: oneshot::Receiver<Result<(EntityId, Mailbox<Action>), CoreError>>,
    ) -> OpOutcome {
        if !sent {
            return OpOutcome::Refused;
        }
        match reply.await {
            Ok(Ok((entity, actions))) => OpOutcome::Joined(entity, actions),
            Ok(Err(e)) => OpOutcome::Rejected(e),
            Err(_) => OpOutcome::Gone,
        }
    }

    /// The implicit resume round trip (§14.3 + §6): single rooms get one
    /// `RoomControl::Resume` (the room falls back to a fresh join itself);
    /// sharded rooms broadcast `ShardMsg::Resume` to EVERY shard and the
    /// outcomes are folded here — at most one accept is structurally
    /// possible (the ledger record lives on exactly one shard), an
    /// all-miss folds into a fallback plain join on the home shard, and a
    /// tripped epoch guard propagates as a rejection (a newer session
    /// already owns the identity).
    #[allow(clippy::too_many_arguments)] // the join's routing, its joiner, its channel
    pub(super) async fn dispatch_resume(
        conn: ConnectionId,
        room: RoomId,
        handle: &RoomHandle<St, Sp>,
        shard: Option<usize>,
        epoch: u64,
        identity: String,
        claims: Option<bytes::Bytes>,
        out: mpsc::Sender<FrameBatch>,
    ) -> OpOutcome {
        match handle {
            RoomHandle::Single(control) => {
                let (joined_tx, joined_rx) =
                    oneshot::channel::<Result<(EntityId, Mailbox<Action>), CoreError>>();
                let sent = control
                    .send(RoomControl::Resume {
                        conn,
                        epoch,
                        identity,
                        claims,
                        out,
                        reply: joined_tx,
                    })
                    .await
                    .is_ok();
                Self::settle(sent, joined_rx).await
            }
            RoomHandle::Sharded(mailboxes) => {
                // One oneshot per shard; each shard answers exactly once
                // (its CONTROL phase drains its FIFO inbox), or drops the
                // reply unanswered as it stops or dies (B71): a stopping
                // shard's `finish` drains its inbox dropping each reply,
                // a dead shard's inbox goes with its task, and a closed
                // inbox refused the send here. Gone shards cost nothing.
                let mut answers = Vec::with_capacity(mailboxes.len());
                for tx in mailboxes {
                    let (reply_tx, reply_rx) = oneshot::channel::<ResumeReply>();
                    if tx
                        .send(ShardMsg::Resume {
                            conn,
                            epoch,
                            identity: identity.clone(),
                            out: out.clone(),
                            reply: reply_tx,
                        })
                        .await
                        .is_ok()
                    {
                        answers.push(reply_rx);
                    }
                }
                // Every live shard's answer, however slow (B82) — the
                // plain join waits for its one shard alike. A fan-out that
                // gave up on a live shard would fall back to a fresh join
                // while that shard's resume is still queued; if it holds
                // the park it then rebinds the parked row to this `out`:
                // the connection bound on two shards, the second binding
                // a member the registry does not track. The shards answer
                // in parallel, so the wait is the slowest shard's, once.
                let mut accepted: Option<(EntityId, Mailbox<Action>)> = None;
                let mut stale: Option<CoreError> = None;
                for answer in answers {
                    match answer.await {
                        Ok(Ok(Some(pair))) => accepted = Some(pair),
                        Ok(Err(e)) => stale = Some(e),
                        // "Not here", or a shard gone with the reply.
                        Ok(Ok(None)) | Err(_) => {}
                    }
                }
                if let Some((entity, actions)) = accepted {
                    return OpOutcome::Joined(entity, actions);
                }
                // A stale epoch — or `RoomGone` from a stopping shard that
                // holds the park: it counted the resume as unprocessed
                // (B75), so no fallback join may count it again.
                if let Some(e) = stale {
                    return OpOutcome::Rejected(e);
                }
                // All shards answered "not here" or are gone without the
                // park: transparent fresh join (§5) through the ordinary
                // path — the one that counts a stopped room's refusal.
                Self::dispatch_plain_join(conn, room, handle, shard, epoch, identity, claims, out)
                    .await
            }
        }
    }
}
