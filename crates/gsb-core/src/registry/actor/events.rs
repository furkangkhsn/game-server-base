//! The registry as the event router (`docs/EVENTS.md` §6): the routing
//! table ([`EventRoutes`]) reads this actor's own tables through
//! [`Locate`]; this module keeps the one index routing needs on top of
//! them (identity → its rows) and handles [`RegistryMsg::Push`].
//!
//! An identity has a ROW while the connection table holds a row for it —
//! an authenticated session (live or parked), or a park row. Its numbers
//! (`EventRoutes`) end when its last row goes: a resume keeps them, a new
//! life after being offline starts a new stream.

use std::collections::{HashMap, HashSet};
use std::fmt::Debug;
use std::hash::Hash;
use std::time::{Duration, Instant};

use crate::channel::Mailbox;
use crate::events::EventIn;
use crate::events::route::{Live, Locate, Located, Place};
use crate::id::{ConnectionId, RoomId};
use crate::registry::actor::Registry;
use crate::registry::{ConnInfo, RoomEntry};

/// How often a stream of pushes refreshes the registry's metrics sample
/// at most (a push changes only counters, so it does not emit a sample
/// of its own every time — the burst's last push does, when the mailbox
/// runs dry, or this period after the previous one under a sustained
/// stream).
const PUSH_SAMPLE_EVERY: Duration = Duration::from_millis(250);

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
    /// `conn`'s row now carries `identity` (an authenticated session):
    /// the identity index follows.
    pub(in crate::registry) fn set_identity(&mut self, conn: ConnectionId, identity: &str) {
        let Some(info) = self.conns.get_mut(&conn) else {
            return;
        };
        let old = std::mem::replace(&mut info.identity, identity.to_owned());
        reindex(&mut self.ids, &mut self.routes, conn, &old, identity);
    }

    /// Remove `conn`'s row from the connection table — THE removal path,
    /// so the identity index (and with it the routing numbers) can never
    /// outlive the rows it indexes.
    pub(in crate::registry) fn release_row(&mut self, conn: ConnectionId) -> Option<ConnInfo> {
        let info = self.conns.remove(&conn)?;
        reindex(&mut self.ids, &mut self.routes, conn, &info.identity, "");
        Some(info)
    }

    /// Put `info` under `conn` (a park row), indexed.
    pub(in crate::registry) fn insert_row(&mut self, conn: ConnectionId, info: ConnInfo) {
        let identity = info.identity.clone();
        let old = self
            .conns
            .insert(conn, info)
            .map(|o| o.identity)
            .unwrap_or_default();
        reindex(&mut self.ids, &mut self.routes, conn, &old, &identity);
    }

    /// [`crate::registry::RegistryMsg::Authed`]: the row is authenticated
    /// under `identity` with `features`.
    pub(in crate::registry) fn on_authed(
        &mut self,
        conn: ConnectionId,
        identity: String,
        features: u64,
    ) -> bool {
        let Some(info) = self.conns.get_mut(&conn) else {
            return false;
        };
        info.authed = true;
        info.features = features;
        self.set_identity(conn, &identity);
        true
    }

    /// [`crate::registry::RegistryMsg::Push`]: route every recipient's
    /// every leg — never awaited, every delivery a `try_send`.
    pub(in crate::registry) fn on_push(&mut self, push: crate::events::Push) {
        let at = Tables {
            conns: &self.conns,
            rooms: &self.rooms,
            ids: &self.ids,
        };
        self.routes.route(push, &at);
        // The counters changed: sampled at the end of a burst (the
        // mailbox ran dry), or every `PUSH_SAMPLE_EVERY` under a stream
        // that never lets it.
        let now = Instant::now();
        let due = self
            .push_sampled_at
            .is_none_or(|at| now.duration_since(at) >= PUSH_SAMPLE_EVERY);
        if self.inbox.is_empty() || due {
            self.push_sampled_at = Some(now);
            self.emit_metrics();
        }
    }

    /// [`crate::registry::RegistryMsg::Relocated`]: shard `shard` of
    /// `room` installed `identity`'s migration `mig` — its membership
    /// rows follow (where its WORLD legs go), and the shard they were
    /// routed to before is sealed (`docs/EVENTS.md` §7.3). Stale (and
    /// counted) for another incarnation of the room, an identity with no
    /// membership there, or a count not above the one known.
    pub(in crate::registry) fn on_relocated(
        &mut self,
        room: RoomId,
        generation: u64,
        identity: &str,
        shard: usize,
        mig: u64,
    ) {
        let Some(entry) = self.rooms.get(&room).filter(|e| e.generation == generation) else {
            self.routes.relocation_stale();
            return;
        };
        let rows: Vec<ConnectionId> = self
            .ids
            .get(identity)
            .into_iter()
            .flatten()
            .copied()
            .filter(|c| self.conns.get(c).is_some_and(|i| i.room == Some(room)))
            .collect();
        // The membership routing reads (`Tables::locate`): a live row
        // before a parked one.
        let top = rows
            .iter()
            .filter_map(|c| Some((*c, self.conns.get(c)?)))
            .max_by_key(|(c, i)| (!i.detached, *c))
            .map(|(_, i)| (i.shard, i.mig));
        let Some((old, _)) = top.filter(|(_, known)| mig > *known) else {
            self.routes.relocation_stale();
            return;
        };
        for c in &rows {
            if let Some(info) = self.conns.get_mut(c) {
                info.shard = Some(shard);
                info.mig = mig;
            }
        }
        let door = old
            .filter(|&o| o != shard)
            .and_then(|o| entry.events.get(o));
        self.routes.relocated(door, identity, mig);
    }

    /// `room` ended: its room-wide numbers end (a rebuilt room starts
    /// them from 1).
    pub(in crate::registry) fn forget_room_events(&mut self, room: RoomId) {
        self.routes.room_gone(room);
    }
}

/// Move `conn` in the identity index from `old` to `new` (either may be
/// empty: anonymous, no entry). An identity whose last row leaves the
/// index ends its event numbers (`EventRoutes::forget`). Free-standing
/// over the two fields so a caller holding another table's borrow can
/// use it.
pub(in crate::registry) fn reindex(
    ids: &mut HashMap<String, Vec<ConnectionId>>,
    routes: &mut crate::events::route::EventRoutes,
    conn: ConnectionId,
    old: &str,
    new: &str,
) {
    if old != new
        && !old.is_empty()
        && let Some(rows) = ids.get_mut(old)
    {
        rows.retain(|c| *c != conn);
        if rows.is_empty() {
            ids.remove(old);
            routes.forget(old);
        }
    }
    if !new.is_empty() {
        let rows = ids.entry(new.to_owned()).or_default();
        if !rows.contains(&conn) {
            rows.push(conn);
        }
    }
}

/// The registry's tables, as routing reads them.
struct Tables<'a, St, Sp> {
    conns: &'a HashMap<ConnectionId, ConnInfo>,
    rooms: &'a HashMap<RoomId, RoomEntry<St, Sp>>,
    ids: &'a HashMap<String, Vec<ConnectionId>>,
}

impl<St, Sp> Tables<'_, St, Sp> {
    /// The mailbox of the actor holding `info`'s membership.
    fn door(&self, info: &ConnInfo) -> Option<&Mailbox<EventIn>> {
        let entry = self.rooms.get(&info.room?)?;
        match (entry.shards.is_some(), info.shard) {
            (false, _) => entry.events.first(),
            (true, Some(i)) => entry.events.get(i),
            // A sharded membership whose shard the join did not report.
            (true, None) => None,
        }
    }
}

impl<St, Sp> Locate for Tables<'_, St, Sp> {
    fn locate(&self, identity: &str) -> Option<Located<'_>> {
        let rows = self.ids.get(identity)?;
        // The CLIENT leg: the newest live authenticated session (a newer
        // session of an identity supersedes an older one, §13).
        let live = rows
            .iter()
            .filter_map(|c| Some((*c, self.conns.get(c)?)))
            .filter(|(_, i)| i.authed && !i.detached)
            .filter_map(|(c, i)| Some((c, i.inbox.as_ref()?, i.features)))
            .max_by_key(|(c, _, _)| *c)
            .map(|(_, inbox, features)| Live { inbox, features });
        // The WORLD leg: the membership — a live one before a parked one
        // (a resume's new row before the park row it is replacing).
        let member = rows
            .iter()
            .filter_map(|c| Some((*c, self.conns.get(c)?)))
            .filter(|(_, i)| i.room.is_some())
            .max_by_key(|(c, i)| (!i.detached, *c))
            .map(|(_, i)| i);
        let place = match member {
            Some(info) => Place::In(self.door(info)),
            None => Place::Nowhere,
        };
        Some(Located { live, place })
    }

    fn members(&self, room: RoomId) -> Vec<String> {
        let mut seen = HashSet::new();
        self.conns
            .values()
            .filter(|i| i.room == Some(room) && !i.identity.is_empty())
            .filter(|i| seen.insert(i.identity.as_str()))
            .map(|i| i.identity.clone())
            .collect()
    }

    fn identities(&self) -> Vec<String> {
        self.ids.keys().cloned().collect()
    }

    fn doors(&self, room: RoomId) -> Option<Vec<&Mailbox<EventIn>>> {
        self.rooms.get(&room).map(|e| e.events.iter().collect())
    }

    fn rooms(&self) -> Vec<RoomId> {
        self.rooms.keys().copied().collect()
    }
}
