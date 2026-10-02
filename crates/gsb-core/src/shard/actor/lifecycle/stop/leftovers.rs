//! What a stopping shard still holds beyond its sessions (BACKLOG B68):
//! the messages its inbox and deferred queue hold — connection ops,
//! migrations landing, effects, team and border updates — and the
//! remote effects it holds to send or apply. A child of the shard's stop.

use std::fmt::Debug;
use std::hash::Hash;

use crate::error::CoreError;
use crate::id::{ConnectionId, EntityId};
use crate::room::drop_unread;
use crate::shard::ShardMsg;
use crate::shard::actor::ShardActor;

impl<W, G, St, Sp> ShardActor<W, G, St, Sp>
where
    W: Send + 'static,
    G: Eq + Hash + Clone + Debug + Send + 'static,
    St: Debug + Send + 'static,
    Sp: Debug + Clone + PartialEq + Send + 'static,
{
    /// Close the inbox (a later send fails at its sender: a neighbour's
    /// migration stays with the neighbour as `migrations_failed`; a
    /// connection's refused join is counted by its dispatcher as
    /// `joins_refused_closed` — B75; a refused leave or detach nowhere,
    /// this stop having ended the member — F55) and count every leftover
    /// by kind.
    /// The broadcast ops (`Leave`, `Detach`, `Resume` reach every shard
    /// of the room) count only where they would have acted: the owning
    /// member's shard, the shard holding the parked identity.
    ///
    /// A resume whose identity is parked HERE is answered `RoomGone`
    /// (B75): "counted here" — the dispatcher's fan-out then answers the
    /// client `RoomGone` without the fallback fresh join, which would
    /// otherwise be refused (or queued) and counted a second time. Every
    /// other reply is dropped unanswered, which is how the fan-out learns
    /// this shard is gone (B71); a resume parked on no shard falls
    /// through to the fresh join and is counted there, once.
    pub(crate) fn count_leftovers(&mut self) {
        self.inbox.close();
        let mut left: Vec<ShardMsg<St, Sp>> = self.deferred.drain(..).collect();
        left.extend(self.inbox.drain());
        for m in left {
            match m {
                ShardMsg::Join { .. } => self.m.stop.joins_unprocessed += 1,
                ShardMsg::Resume {
                    identity, reply, ..
                } => {
                    if self
                        .conns
                        .values()
                        .any(|rc| rc.detached && rc.identity == identity)
                    {
                        self.m.stop.resumes_unprocessed += 1;
                        let _ = reply.send(Err(CoreError::RoomGone));
                    }
                }
                ShardMsg::Leave { conn, entity, .. } => {
                    if self.member(conn, entity).is_some() {
                        self.m.stop.leaves_unprocessed += 1;
                    }
                }
                ShardMsg::Detach { conn, entity, .. } | ShardMsg::DetachBy { conn, entity, .. } => {
                    if self.member(conn, entity) == Some(false) {
                        self.m.stop.detaches_unprocessed += 1;
                    }
                }
                ShardMsg::Migrate { player, .. } => {
                    self.m.stop.migrations_in_dropped += 1;
                    // A player rides with its input channel: what it had
                    // not read yet is lost with it, counted as a session
                    // end counts it.
                    if let Some(mut p) = player {
                        self.m.count_unread(drop_unread(&mut p.actions));
                    }
                }
                ShardMsg::RemoteEffect(_) => self.m.stop.effects_unapplied += 1,
                ShardMsg::TeamImport(_) => self.m.stop.team_imports_unapplied += 1,
                ShardMsg::Border { .. } | ShardMsg::ResyncRequest { .. } => {
                    self.m.stop.border_updates_unapplied += 1;
                }
                ShardMsg::Shutdown => {}
            }
        }
        let fx = &self.effects;
        self.m.stop.effects_unapplied += fx.pending.len() as u64;
        self.m.stop.effects_unsent += (fx.retry.len() + fx.out.queue.len()) as u64;
    }

    /// Whether `conn` is bound here to a row owning `entity` — and if so,
    /// whether that row is parked.
    fn member(&self, conn: ConnectionId, entity: EntityId) -> Option<bool> {
        let player = self.binding.get(&conn)?;
        let rc = self.conns.get(player)?;
        (rc.entity == entity).then_some(rc.detached)
    }
}
