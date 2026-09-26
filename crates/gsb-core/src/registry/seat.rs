//! What a successful join hands the connection actor.

use crate::channel::Mailbox;
use crate::id::EntityId;
use crate::room::{Action, InputRate};

/// A successful [`RegistryMsg::SpawnPlayer`](crate::registry::RegistryMsg::SpawnPlayer):
/// everything the connection actor needs to play in the room it joined.
///
/// The input rate rides here, not on the room's own join reply: the
/// registry holds every room's config (single or sharded — one limit per
/// room, whatever its shards), so ONE point stamps it for both shapes and
/// both join paths (fresh and resumed), and the room and shard actors
/// never see it.
#[derive(Debug)]
pub struct Seat {
    /// The entity (wire id) the room created, or resumed.
    pub entity: EntityId,
    /// The room's per-connection action channel for this session.
    pub actions: Mailbox<Action>,
    /// The room's input rate limit ([`RoomConfig::input_rate`](crate::room::RoomConfig::input_rate));
    /// `None` = the room does not limit input.
    pub input_rate: Option<InputRate>,
}
