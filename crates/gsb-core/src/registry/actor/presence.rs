//! Engine events and the presence query (`docs/EVENTS.md` §5.4, §20 E12,
//! round e4): an identity's presence DERIVED from the connection table —
//! the same rows event routing reads (`events.rs`), so the two can never
//! disagree, and no second copy of the facts to keep in step.
//!
//! An identity's [`View`] is `None` (offline: no row) or whether one of
//! its rows is a live session and the room its WORLD legs go to. Around
//! every message that can change a row ([`Registry::presence_watch`]) the
//! registry takes the views of the identities the message touches, before
//! and after, and offers the difference to the subscribers as
//! [`EngineEvent`]s ([`Registry::presence_settle`]). No subscriber: no
//! view is taken (one branch per message).

use std::fmt::Debug;
use std::hash::Hash;

use crate::calls::Identity;
use crate::events::{
    EngineEvent, EngineSubscribers, PresenceAnswer, PresenceAsk, PresenceEntry, PresencePage,
    PresenceQuery, PresenceState,
};
use crate::id::RoomId;
use crate::registry::RegistryMsg;
use crate::registry::actor::Registry;

#[cfg(test)]
mod tests;

/// An online identity's presence, as its rows have it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::registry) struct View {
    /// One of its rows is an authenticated session whose transport is up.
    live: bool,
    /// Where its WORLD legs go: its live membership, else its parked one.
    room: Option<RoomId>,
}

/// The identities a message may change, with their views before it.
pub(in crate::registry) struct Watch(Vec<(String, Option<View>)>);

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
    /// Offer engine events to `subscribers` (the composition root's,
    /// `docs/EVENTS.md` §5.4). Without any, the registry derives nothing.
    pub fn with_engine_events(mut self, subscribers: EngineSubscribers) -> Self {
        self.engine = subscribers;
        self
    }

    /// `identity`'s presence now (`None`: offline).
    fn view_of(&self, identity: &str) -> Option<View> {
        let rows = self.ids.get(identity)?;
        let mut infos = rows
            .iter()
            .filter_map(|c| Some((*c, self.conns.get(c)?)))
            .peekable();
        infos.peek()?;
        let mut live = false;
        let mut member: Option<(bool, crate::id::ConnectionId, RoomId)> = None;
        for (c, i) in infos {
            live |= i.authed && !i.detached && i.inbox.is_some();
            // The membership routing picks (`events.rs`, `Locate`): a
            // live one before a parked one, the newest first.
            if let Some(room) = i.room
                && member.is_none_or(|(d, mc, _)| (!i.detached, c) > (d, mc))
            {
                member = Some((!i.detached, c, room));
            }
        }
        Some(View {
            live,
            room: member.map(|m| m.2),
        })
    }

    /// The identities `msg` may change and their views now — `None`
    /// without subscribers, or for a message that touches no row's
    /// identity, room or transport.
    pub(in crate::registry) fn presence_watch(&self, msg: &RegistryMsg) -> Option<Watch> {
        if self.engine.is_empty() {
            return None;
        }
        let of = |c: &crate::id::ConnectionId| self.conns.get(c).map(|i| i.identity.clone());
        let mut ids: Vec<String> = match msg {
            // The row takes an identity (it may replace another).
            RegistryMsg::Authed { conn, identity, .. }
            | RegistryMsg::SpawnPlayer { conn, identity, .. } => {
                of(conn).into_iter().chain([identity.clone()]).collect()
            }
            // One connection's row: its transport, its membership, its
            // park (a park row and a resume's released rows are the same
            // identity's).
            RegistryMsg::ConnClosed { conn, .. }
            | RegistryMsg::SpawnDone { conn, .. }
            | RegistryMsg::LeaveDone { conn, .. }
            | RegistryMsg::DetachDone { conn, .. }
            | RegistryMsg::DetachDespawned { conn, .. }
            | RegistryMsg::DespawnPlayer { conn } => of(conn).into_iter().collect(),
            RegistryMsg::CloseConn(req) => of(&req.conn).into_iter().collect(),
            RegistryMsg::LeaveConn(req) => of(&req.conn).into_iter().collect(),
            // A room's end clears every membership in it.
            RegistryMsg::RoomDied { id, .. } | RegistryMsg::DestroyRoom { id, .. } => self
                .conns
                .values()
                .filter(|i| i.room == Some(*id))
                .map(|i| i.identity.clone())
                .collect(),
            // Every other message leaves the rows' identities, rooms and
            // transports alone (an open's row has no identity yet). No
            // match-all arm: a new message kind must be decided here.
            RegistryMsg::CreateRoom { .. }
            | RegistryMsg::RoomStatus { .. }
            | RegistryMsg::ConnOpened { .. }
            | RegistryMsg::ConnPeerChanged { .. }
            | RegistryMsg::Push(_)
            // A migration moves the member's shard, not its room.
            | RegistryMsg::Relocated { .. }
            | RegistryMsg::Presence(_)
            | RegistryMsg::Shutdown
            | RegistryMsg::SpawnFailed { .. }
            | RegistryMsg::OpsClosed { .. }
            | RegistryMsg::TeamExport { .. } => return None,
        };
        ids.retain(|i| !i.is_empty());
        ids.sort_unstable();
        ids.dedup();
        if ids.is_empty() {
            return None;
        }
        Some(Watch(
            ids.into_iter()
                .map(|i| {
                    let v = self.view_of(&i);
                    (i, v)
                })
                .collect(),
        ))
    }

    /// Offer what the message changed of each watched identity's
    /// presence (see the module docs; the order is
    /// [`crate::events::EngineEvent`]'s).
    pub(in crate::registry) fn presence_settle(&mut self, watch: Option<Watch>) {
        let Some(Watch(ids)) = watch else {
            return;
        };
        for (id, before) in ids {
            let after = self.view_of(&id);
            for e in changes(&Identity::new(&id), before, after) {
                self.engine.emit(&e);
            }
        }
    }

    /// [`RegistryMsg::Presence`]: answered in place (never awaited).
    pub(in crate::registry) fn on_presence(&mut self, ask: PresenceAsk) {
        let answer = match ask.query {
            PresenceQuery::Page { after, limit } => PresenceAnswer::Page(self.page(after, limit)),
            PresenceQuery::Of(who) => {
                PresenceAnswer::Of(who.into_iter().map(|w| self.entry(w)).collect())
            }
        };
        // An asker that gave up: nothing to tell.
        let _ = ask.reply.send(answer);
    }

    fn entry(&self, identity: Identity) -> PresenceEntry {
        let (state, room) = match self.view_of(identity.as_str()) {
            None => (PresenceState::Offline, None),
            Some(v) if v.live => (PresenceState::Live, v.room),
            Some(v) => (PresenceState::Parked, v.room),
        };
        PresenceEntry {
            identity,
            state,
            room,
        }
    }

    /// The online identities after `after`, at most `limit`, in order:
    /// O(identities) to select, O(limit · log limit) to order.
    fn page(&self, after: Option<Identity>, limit: usize) -> PresencePage {
        let mut keys: Vec<&str> = self
            .ids
            .keys()
            .map(String::as_str)
            .filter(|k| after.as_ref().is_none_or(|a| *k > a.as_str()))
            .collect();
        let more = keys.len() > limit;
        if more {
            keys.select_nth_unstable(limit);
            keys.truncate(limit);
        }
        keys.sort_unstable();
        let entries: Vec<PresenceEntry> = keys
            .into_iter()
            .map(|k| self.entry(Identity::new(k)))
            .filter(|e| e.state != PresenceState::Offline)
            .collect();
        let next = match more {
            true => entries.last().map(|e| e.identity.clone()),
            false => None,
        };
        PresencePage { entries, next }
    }
}

/// What changed between two views of `who`, in the documented order:
/// `Online`, `Left`, `Joined`, `Parked`/`Resumed` — and `Left` before
/// `Offline`.
pub(in crate::registry) fn changes(
    who: &Identity,
    before: Option<View>,
    after: Option<View>,
) -> Vec<EngineEvent> {
    let mut out = Vec::new();
    let identity = || who.clone();
    match (before, after) {
        (None, None) => {}
        (Some(b), None) => {
            if let Some(room) = b.room {
                out.push(EngineEvent::Left {
                    identity: identity(),
                    room,
                });
            }
            out.push(EngineEvent::Offline {
                identity: identity(),
            });
        }
        (b, Some(a)) => {
            // Coming online is from "offline, in no room, live" — a first
            // row that is not live (never seen) still reports its park.
            if b.is_none() {
                out.push(EngineEvent::Online {
                    identity: identity(),
                });
            }
            let b = b.unwrap_or(View {
                live: true,
                room: None,
            });
            if b.room != a.room {
                if let Some(room) = b.room {
                    out.push(EngineEvent::Left {
                        identity: identity(),
                        room,
                    });
                }
                if let Some(room) = a.room {
                    out.push(EngineEvent::Joined {
                        identity: identity(),
                        room,
                    });
                }
            }
            match (b.live, a.live) {
                (true, false) => out.push(EngineEvent::Parked {
                    identity: identity(),
                    room: a.room,
                }),
                (false, true) => out.push(EngineEvent::Resumed {
                    identity: identity(),
                    room: a.room,
                }),
                _ => {}
            }
        }
    }
    out
}
