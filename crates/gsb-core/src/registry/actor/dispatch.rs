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
use crate::shard::ShardMsg;

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
    pub(super) async fn dispatch_plain_join(
        conn: ConnectionId,
        _room: RoomId,
        handle: &RoomHandle<St, Sp>,
        shard: Option<usize>,
        epoch: u64,
        identity: String,
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
    pub(super) async fn dispatch_resume(
        conn: ConnectionId,
        room: RoomId,
        handle: &RoomHandle<St, Sp>,
        shard: Option<usize>,
        epoch: u64,
        identity: String,
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
                        out,
                        reply: joined_tx,
                    })
                    .await
                    .is_ok();
                Self::settle(sent, joined_rx).await
            }
            RoomHandle::Sharded(mailboxes) => {
                // One oneshot per shard; each shard answers exactly once
                // (its CONTROL phase drains its FIFO inbox). Awaiting them
                // all is bounded by the shard count (≤ 16 by the grid
                // topology).
                let n = mailboxes.len();
                let (agg_tx, mut agg_rx) = mpsc::channel::<
                    Result<Option<(EntityId, Mailbox<Action>)>, CoreError>,
                >(n.max(1));
                for tx in mailboxes {
                    let (reply_tx, reply_rx) = oneshot::channel();
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
                        // Forward this shard's eventual answer into the
                        // aggregate; a dropped shard dies with its oneshot
                        // and simply contributes nothing.
                        let value = agg_tx.clone();
                        tokio::spawn(async move {
                            if let Ok(answer) = reply_rx.await {
                                let _ = value.send(answer).await;
                            }
                        });
                    }
                }
                // Only the forwarders hold the aggregate's sender now (B71):
                // once every shard has answered or is known gone — a
                // stopping shard's `finish` drains its inbox dropping each
                // reply, a dead shard's inbox goes with its task, a closed
                // inbox refused the send above — `recv` sees the channel
                // close. A copy held here kept it open, so every missing
                // answer cost the whole per-answer timeout: a resume into
                // a stopping room waited 5 s per shard for its `RoomGone`.
                drop(agg_tx);
                let mut accepted: Option<(EntityId, Mailbox<Action>)> = None;
                let mut stale: Option<CoreError> = None;
                for _ in 0..n {
                    match tokio::time::timeout(std::time::Duration::from_secs(5), agg_rx.recv())
                        .await
                    {
                        Ok(Some(Ok(Some(pair)))) => accepted = Some(pair),
                        Ok(Some(Err(e))) => stale = Some(e),
                        // Every shard answered or is gone.
                        Ok(None) => break,
                        // "Not here", or a live shard slower than the bound.
                        Ok(Some(Ok(None))) | Err(_) => {}
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
                Self::dispatch_plain_join(conn, room, handle, shard, epoch, identity, out).await
            }
        }
    }
}
