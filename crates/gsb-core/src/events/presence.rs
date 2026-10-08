//! Who is online now (`docs/EVENTS.md` §20 E12, round e4): the registry
//! answers from its own table, so a service that restarted — or saw a
//! gap in its engine events — re-reads the truth when it first needs it.
//! The engine keeps no presence history; this is the present.
//!
//! The handle ([`Presence`]) is the awaitable, `Handle`-style side: one
//! `try_send` into the registry's mailbox (a full mailbox refuses at
//! once, [`PresenceRefused::Busy`]) and one awaited reply. The registry
//! never awaits: it answers in place. A page is ordered by identity, so
//! paging is stable while the table changes (an identity that comes
//! online behind the cursor is missed by this walk — the engine events
//! carry it).

use tokio::sync::oneshot;

use crate::calls::Identity;
use crate::channel::{Mailbox, TrySend};
use crate::id::RoomId;
use crate::registry::RegistryMsg;

/// The most entries one page carries (a larger `limit` is clamped).
pub const MAX_PRESENCE_PAGE: usize = 4096;

/// An identity's presence (the same facts its engine events derive from).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PresenceState {
    /// No row: no session, nothing parked.
    Offline,
    /// A live authenticated session.
    Live,
    /// Online, nothing live: its entity waits in a room for a resume.
    Parked,
}

/// One identity's presence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PresenceEntry {
    /// Who.
    pub identity: Identity,
    /// Live, parked or offline.
    pub state: PresenceState,
    /// The room its WORLD legs go to (`None`: in no room, or offline).
    pub room: Option<RoomId>,
}

/// One page of the online identities, ordered by identity.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PresencePage {
    /// Up to the page's `limit` online identities (never `Offline`).
    pub entries: Vec<PresenceEntry>,
    /// The cursor of the next page (the last identity here), `None` when
    /// this page is the last.
    pub next: Option<Identity>,
}

/// Why a presence query got no answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PresenceRefused {
    /// The registry's mailbox was full (try again later).
    Busy,
    /// The registry is gone (the server is stopping), or it stopped
    /// before answering.
    Gone,
}

impl std::fmt::Display for PresenceRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Busy => f.write_str("the registry's mailbox is full"),
            Self::Gone => f.write_str("the registry is gone"),
        }
    }
}

impl std::error::Error for PresenceRefused {}

/// A presence question as the registry takes it
/// ([`RegistryMsg::Presence`]); built by [`Presence`] only.
#[derive(Debug)]
pub struct PresenceAsk {
    pub(crate) query: PresenceQuery,
    pub(crate) reply: oneshot::Sender<PresenceAnswer>,
}

#[derive(Debug)]
pub(crate) enum PresenceQuery {
    /// The online identities after `after`, at most `limit`.
    Page {
        after: Option<Identity>,
        limit: usize,
    },
    /// These identities, each (offline ones included).
    Of(Vec<Identity>),
}

#[derive(Debug)]
pub(crate) enum PresenceAnswer {
    Page(PresencePage),
    Of(Vec<PresenceEntry>),
}

/// The awaitable presence query handle (`RegistryParts::presence()` in
/// `gsb-server`). `Clone`, cheap.
///
/// ```ignore
/// // a restarted friends service, at its first need:
/// let mut after = None;
/// loop {
///     let page = presence.page(after.as_ref(), 1024).await?;
///     for e in &page.entries { self.mark(&e.identity, e.state) }
///     match page.next { Some(n) => after = Some(n), None => break }
/// }
/// ```
#[derive(Clone)]
pub struct Presence {
    registry: Mailbox<RegistryMsg>,
}

impl std::fmt::Debug for Presence {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Presence").finish_non_exhaustive()
    }
}

impl Presence {
    /// A handle over the registry's mailbox.
    pub fn new(registry: Mailbox<RegistryMsg>) -> Self {
        Self { registry }
    }

    /// The online identities (live or parked) after `after` in identity
    /// order, at most `limit` (clamped to `1..=`[`MAX_PRESENCE_PAGE`]).
    ///
    /// # Errors
    ///
    /// [`PresenceRefused`]: the registry's mailbox was full, or the
    /// registry is gone.
    pub async fn page(
        &self,
        after: Option<&Identity>,
        limit: usize,
    ) -> Result<PresencePage, PresenceRefused> {
        let query = PresenceQuery::Page {
            after: after.cloned(),
            limit: limit.clamp(1, MAX_PRESENCE_PAGE),
        };
        match self.ask(query).await? {
            PresenceAnswer::Page(p) => Ok(p),
            PresenceAnswer::Of(_) => Err(PresenceRefused::Gone),
        }
    }

    /// The presence of each of `who`, in its order (an identity with no
    /// row is [`PresenceState::Offline`]). One registry round trip,
    /// whatever the length.
    ///
    /// # Errors
    ///
    /// [`PresenceRefused`], as [`Self::page`].
    pub async fn of(&self, who: &[Identity]) -> Result<Vec<PresenceEntry>, PresenceRefused> {
        match self.ask(PresenceQuery::Of(who.to_vec())).await? {
            PresenceAnswer::Of(v) => Ok(v),
            PresenceAnswer::Page(_) => Err(PresenceRefused::Gone),
        }
    }

    async fn ask(&self, query: PresenceQuery) -> Result<PresenceAnswer, PresenceRefused> {
        let (reply, answer) = oneshot::channel();
        let msg = RegistryMsg::Presence(PresenceAsk { query, reply });
        match crate::channel::try_send(&self.registry, msg) {
            TrySend::Sent => answer.await.map_err(|_| PresenceRefused::Gone),
            TrySend::Full => Err(PresenceRefused::Busy),
            TrySend::Closed => Err(PresenceRefused::Gone),
        }
    }
}
