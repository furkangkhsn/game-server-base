//! The demo's friends service (`docs/EVENTS.md` §8.3, round e4): the
//! engine's presence events in, `FriendOnline` / `FriendOffline` out — a
//! `#[gsb_core::service]` trait with one `#[gsb(engine_events)]` method.
//!
//! The engine defines no friends (decision 3: no chat, no presence lists,
//! no guilds in the engine); this is the game's service, over three
//! engine pieces:
//!
//! - its presence SOURCE: the engine events the registry derives
//!   (`Online`, `Offline`, `Parked`, `Resumed`), subscribed with
//!   `Setup::engine_events(Interest::PRESENCE)` and run one at a time by
//!   the host ([`Friends::on_engine`]);
//! - the TRUTH it reads when it needs who is online: the registry's
//!   presence query ([`Presence::of`], §20 E12) — the service keeps no
//!   presence state of its own, so a restart loses nothing;
//! - its VOICE: a [`Publisher`] (`push`, `push_many`) for the two
//!   `to: CLIENT` events of `game.proto`.
//!
//! The friend graph is a fixed, symmetric list handed to the constructor
//! (a real game reads its database). A friend who comes online (or
//! resumes) is announced to their online friends, and learns which of
//! them are online; one who goes offline (or parks — "away" is the
//! game's reading of it, here: offline) is announced to their online
//! friends (a park that ends offline announces it twice — the same
//! fact, harmless to a client). A refused presence query (the registry's mailbox full) falls
//! back to pushing to every friend: the offline ones are counted by the
//! router (`events_offline`), nothing is lost silently.
//!
//! ```ignore
//! let friends = FriendGraph::new(&[("alice", "bob")], parts.presence(), parts.publisher("friends"));
//! parts.service(Friends::serve(friends), |s| s.engine_events(Interest::PRESENCE))?;
//! ```

use std::collections::HashMap;

use gsb_core::calls::{CallCx, Identity};
use gsb_core::events::{EngineEvent, Presence, PresenceState, Publisher};

use crate::demo::game::{FriendOffline, FriendOnline};

/// The friends service's contract.
#[gsb_core::service]
pub trait Friends {
    /// The caller's friends who are online now (live or parked), by
    /// identity, in the graph's order.
    async fn online(&self, cx: &CallCx) -> Result<Vec<String>, String>;

    /// The engine's presence events (`Setup::engine_events`).
    #[gsb(engine_events)]
    async fn on_engine(&self, e: EngineEvent);
}

/// The demo's friends: a fixed symmetric graph, the registry's presence
/// query, a publishing handle.
pub struct FriendGraph {
    friends: HashMap<Identity, Vec<Identity>>,
    presence: Presence,
    events: Publisher,
}

impl std::fmt::Debug for FriendGraph {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FriendGraph")
            .field("identities", &self.friends.len())
            .finish_non_exhaustive()
    }
}

impl FriendGraph {
    /// A graph of the `pairs` (each pair befriends both ways; a pair
    /// given twice counts once).
    pub fn new(pairs: &[(&str, &str)], presence: Presence, events: Publisher) -> Self {
        let mut friends: HashMap<Identity, Vec<Identity>> = HashMap::new();
        for (a, b) in pairs {
            let (a, b) = (Identity::new(a), Identity::new(b));
            for (x, y) in [(&a, &b), (&b, &a)] {
                let list = friends.entry(x.clone()).or_default();
                if !list.contains(y) {
                    list.push(y.clone());
                }
            }
        }
        Self {
            friends,
            presence,
            events,
        }
    }

    /// `who`'s friends.
    pub fn of(&self, who: &Identity) -> &[Identity] {
        self.friends.get(who).map(Vec::as_slice).unwrap_or_default()
    }

    /// Which of `who`'s friends are online — or, when the registry
    /// refused the question, all of them (see the module docs).
    async fn online_friends(&self, who: &Identity) -> Vec<Identity> {
        let friends = self.of(who);
        if friends.is_empty() {
            return Vec::new();
        }
        match self.presence.of(friends).await {
            Ok(states) => states
                .into_iter()
                .filter(|e| e.state != PresenceState::Offline)
                .map(|e| e.identity)
                .collect(),
            Err(e) => {
                tracing::debug!(identity = who.as_str(), error = %e, "presence refused; pushing to every friend");
                friends.to_vec()
            }
        }
    }
}

impl Friends for FriendGraph {
    async fn online(&self, cx: &CallCx) -> Result<Vec<String>, String> {
        let me = cx.player().ok_or("no verified player")?;
        let states = self
            .presence
            .of(self.of(me))
            .await
            .map_err(|e| e.to_string())?;
        Ok(states
            .into_iter()
            .filter(|e| e.state != PresenceState::Offline)
            .map(|e| e.identity.as_str().to_owned())
            .collect())
    }

    async fn on_engine(&self, e: EngineEvent) {
        // A push refused at the handle (the router's mailbox full) is
        // counted on the publisher (`events_refused_router_*`).
        match e {
            EngineEvent::Online { identity } | EngineEvent::Resumed { identity, .. } => {
                let online = self.online_friends(&identity).await;
                let me = FriendOnline {
                    who: identity.as_str().into(),
                };
                let _ = self.events.push_many(&online, &me);
                for f in &online {
                    let them = FriendOnline {
                        who: f.as_str().into(),
                    };
                    let _ = self.events.push(&identity, &them);
                }
            }
            EngineEvent::Offline { identity } | EngineEvent::Parked { identity, .. } => {
                let online = self.online_friends(&identity).await;
                let gone = FriendOffline {
                    who: identity.as_str().into(),
                };
                let _ = self.events.push_many(&online, &gone);
            }
            _ => {}
        }
    }
}
