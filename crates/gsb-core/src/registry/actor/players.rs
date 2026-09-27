//! Join and its completion: dispatching a spawn against the cap, and
//! settling the reservation when the room answers.

use std::fmt::Debug;
use std::hash::Hash;

use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, warn};

use crate::channel::{FrameBatch, Mailbox};
use crate::conn::{ConnIn, ServerClose};
use crate::error::CoreError;
use crate::id::{ConnectionId, RoomId};
use crate::registry::*;

use crate::registry::actor::Registry;

mod done;
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
    pub(super) async fn on_spawn_player(
        &mut self,
        conn: ConnectionId,
        room: RoomId,
        out: mpsc::Sender<FrameBatch>,
        identity: String,
        reply: oneshot::Sender<Result<Seat, CoreError>>,
    ) {
        // Double-session supersedence (§5: "en son kazanan" —
        // latest wins): a LIVE (still-connected) session with
        // the same identity in the same room is evicted here,
        // BEFORE the new join dispatches. The old socket —
        // still open by definition — is closed with ERROR 9
        // (`ConnIn::ServerClosed`), its affiliation released
        // through the ordinary leave path. A DETACHED session
        // with the identity needs no eviction: its park IS the
        // resume target (the re-affiliation cleanup in
        // `SpawnDone` releases it).
        if !identity.is_empty() {
            let mut evicted: Vec<(ConnectionId, Option<Mailbox<ConnIn>>)> = Vec::new();
            for (&other, info) in &self.conns {
                if other != conn
                    && !info.detached
                    && info.room == Some(room)
                    && info.identity == identity
                {
                    evicted.push((other, info.inbox.clone()));
                }
            }
            for (old_conn, inbox) in evicted {
                warn!(
                    %old_conn,
                    %conn,
                    room = %room,
                    %identity,
                    "double session: a newer session supersedes the \
                     live one (ERROR 9 to the old socket)"
                );
                if let Some(inbox) = inbox {
                    let reason = "a newer session for this player superseded this \
                         connection"
                        .to_string();
                    tokio::spawn(async move {
                        let _ = inbox
                            .send(ConnIn::ServerClosed {
                                cause: ServerClose::Superseded,
                                reason,
                            })
                            .await;
                    });
                }
                self.direct_leave(old_conn);
            }
        }
        let Some(entry) = self.rooms.get(&room) else {
            let code = if self.retired.contains_key(&room) {
                CoreError::RoomRetired(room.0)
            } else {
                CoreError::RoomNotFound(room.0)
            };
            let _ = reply.send(Err(code));
            return;
        };
        // Record the resume key on THIS connection's entry (the
        // resume re-affiliation matches on it). The entry may
        // not exist yet (ConnOpened races nothing: the accept
        // loop sends ConnOpened before any client frame), so
        // create-on-demand like the cap check does.
        self.conns.entry(conn).or_default().identity = identity.clone();
        let sharded_room = entry.shards.is_some();
        // The room's input rate limit rides the join to its Seat (one
        // stamp for both room shapes and both join paths).
        let input_rate = entry.config.input_rate;
        // The incarnation this join is dispatched against: the
        // settlement reports (SpawnDone/SpawnFailed) echo it so
        // a late settle of a since-died room cannot touch the
        // table (supervision).
        let generation = entry.generation;
        // Sharded room: the registry is the only actor that
        // sees every join, so it enforces the room cap here
        // (a shard cannot count the room without shared state)
        // and routes the join to the home shard (the factory's
        // pure `home_shard` router, given the authenticated
        // identity — never awaited; K4).
        //
        // All the reads from `entry` happen BEFORE the borrow
        // ends (the `pending += 1` below re-borrows mutably).
        let sharded_pick = match &entry.shards {
            Some(group) => {
                // A rejoin does not take a new slot — and neither does
                // the resume of a park this identity holds here (B40): the
                // park's row already counts, and the resume hands that
                // count to this connection (the SpawnDone cleanup nets it
                // out). Without it a full grid refused a player the
                // return to its own parked entity.
                let rejoin = self.conns.get(&conn).and_then(|i| i.room) == Some(room)
                    || self.holds_park(room, &identity);
                let at_cap = match group.cap {
                    Some(cap) => !rejoin && group.members + group.pending >= cap,
                    None => false,
                };
                Some((
                    at_cap,
                    (group.home)(conn, &identity),
                    group.mailboxes.clone(),
                ))
            }
            None => None,
        };
        let (handle, shard_idx) = match sharded_pick {
            Some((at_cap, home_idx, mailboxes)) => {
                if at_cap {
                    // The cap makes the sharded room's degraded
                    // regime structurally unreachable (the same
                    // guardrail as a single room's max_players:
                    // gentle ERROR 8, the connection stays alive).
                    let _ = reply.send(Err(CoreError::RoomFull(room.0)));
                    return;
                }
                // Reserve against the cap until the join settles
                // (SpawnDone / SpawnFailed release it).
                if let Some(e) = self.rooms.get_mut(&room).and_then(|e| e.shards.as_mut()) {
                    e.pending += 1;
                }
                // Clamp a router bug (an out-of-range pick)
                // instead of panicking at join dispatch; the
                // entity's first boundary crossing self-heals
                // the mis-routing (see BuiltRoom::Sharded).
                let idx = home_idx % mailboxes.len();
                (RoomHandle::Sharded(mailboxes), Some(idx))
            }
            None => (
                RoomHandle::Single(
                    self.rooms
                        .get(&room)
                        .and_then(|e| e.control.clone())
                        .expect("single room always has control"),
                ),
                None,
            ),
        };
        let op_tx = self
            .conn_ops
            .entry(conn)
            .or_insert_with(|| Self::spawn_conn_ops(conn, self.self_mailbox.clone(), None));
        // The guard epoch is minted HERE, in the single-threaded
        // registry, globally across all connections (see the
        // `RoomOp::Join::epoch` doc for why per-connection
        // minting broke first-attempt resumes).
        self.next_join_epoch = self.next_join_epoch.wrapping_add(1);
        let join = RoomOp::Join {
            room,
            handle,
            shard: shard_idx,
            generation,
            epoch: self.next_join_epoch,
            out,
            identity,
            input_rate,
            reply,
        };
        let refused = match op_tx.try_send(join) {
            Ok(()) => false,
            // The dispatcher is gone (B63): its dead sender would refuse
            // every later join of this connection. A fresh one takes its
            // slot and the op, once (see `respawn_conn_ops`).
            Err(TrySendError::Closed(join)) => {
                warn!(%conn, room = %room, "join op dispatcher gone; replacing it");
                self.respawn_conn_ops(conn).try_send(join).is_err()
            }
            Err(TrySendError::Full(_)) => true,
        };
        if refused {
            // Op queue full (pathological), or even the fresh
            // dispatcher gone (the runtime shutting down): the
            // op (and its reply) is dropped; the connection actor observes
            // the dropped reply and sends an ERROR frame. The
            // cap reservation never settles (the dispatcher
            // never saw the op), so release it here.
            if sharded_room
                && let Some(e) = self.rooms.get_mut(&room).and_then(|e| e.shards.as_mut())
            {
                e.pending = e.pending.saturating_sub(1);
            }
            // Counted (B57): the client's ERROR says "registry
            // unavailable", and nothing else would record it.
            self.reg_join_ops_dropped += 1;
            self.emit_metrics();
            warn!(%conn, room = %room, "join op not queued; join failed");
        } else {
            debug!(%conn, room = %room, "join dispatched");
        }
    }

    /// Whether a detached row of `identity` holds a park in `room` (the
    /// target of this identity's implicit resume). O(connections), on the
    /// sharded join path only — a control-plane event, like the
    /// supersedence scan above it.
    fn holds_park(&self, room: RoomId, identity: &str) -> bool {
        !identity.is_empty()
            && self
                .conns
                .values()
                .any(|i| i.detached && i.room == Some(room) && i.identity == identity)
    }
}
