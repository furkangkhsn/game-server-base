//! [`RecordCodec`] — one entity's wire record (KIT-ARCHITECTURE §4.1):
//! the seam through which the kit learns WHICH entities are broadcast,
//! WHAT their record's typed wire value is, WHEN it may have changed,
//! and HOW its bytes look — without knowing any of the game's types.
//!
//! The kit keeps everything around the record: the identity it is keyed
//! by ([`WireId`](crate::identity::WireId)), the envelopes it lands
//! in (sequence, delta flag, `removed`, `cell_exits`, the fixed ordering),
//! the framing of each record (length-delimited `entities`, or the
//! game's opt-in record run — [`RecordCodec::RUN`]), and the full/delta
//! decision.

use std::fmt::Debug;

use bevy_ecs::component::Component;
use bevy_ecs::query::{
    QueryFilter, QueryItem, ReadOnlyQueryData, ReleaseStateQueryData, SingleEntityQueryData,
};
use bytes::BytesMut;

/// How a game's entity becomes a wire record.
///
/// **`Wire` is the delta engine's unit** (§4.1, "Kritik karar"): the
/// engine compares typed, quantized wire values and encodes bytes only
/// when a record must actually be written, so the number of encodes per
/// tick is independent of how many entities were merely *touched*. A
/// game that wants opaque bytes picks `Wire = Bytes` — a special case,
/// not the rule.
///
/// Everything is statically dispatched: the rooms are generic over the
/// game and monomorphize over its codec (no trait objects on the hot
/// path).
pub trait RecordCodec: Send + 'static {
    /// The broadcast set: every entity carrying this component is
    /// broadcast. An entity with the marker but no wire identity yet
    /// (anything the game spawned outside a join — bullets, NPCs, traps)
    /// is stamped by the kit in the next pass, so nothing with the marker
    /// can be silently invisible.
    type Marker: Component;

    /// The components the record is built from. Read-only, and confined
    /// to the entity itself (`SingleEntityQueryData`,
    /// `ReleaseStateQueryData`): the kit also reads ONE entity's record
    /// outside a query pass (a joiner's group before its first update).
    type Query: ReadOnlyQueryData + SingleEntityQueryData + ReleaseStateQueryData;

    /// "This entity's record may have changed" — the change-detection
    /// filter the cell-delta engine visits (e.g. `Changed<Position>`; a
    /// game whose record also carries health writes
    /// `Or<(Changed<Pos>, Changed<Hp>)>`). The filter is the game's; the
    /// window it sees is the kit's (the kit calls
    /// `World::clear_trackers` once per tick — §4.4).
    type Dirty: QueryFilter;

    /// The record's typed wire value (the quantized content the client
    /// receives). Equality on it IS the change test — two values equal
    /// ⇔ the same bytes.
    type Wire: Clone + Eq + Debug + Send + 'static;

    /// The wire value of one entity's record, from its [`Self::Query`]
    /// item.
    fn wire(&self, item: QueryItem<'_, '_, Self::Query>) -> Self::Wire;

    /// The record framing this game's frames use (KIT-ARCHITECTURE §4.1,
    /// "A31"; `kit.proto`, `WorldSnapshot.records`).
    ///
    /// - `false` (the default): each record is one length-delimited
    ///   `entities` entry (field 2) — the kit writes the tag and the
    ///   length around [`Self::encode`]'s body, so the body may be
    ///   anything whose end the game's decoder learns from that length
    ///   (a protobuf message: a typed mirror decodes it).
    /// - `true` — the RECORD RUN: every record of a frame goes back to
    ///   back into ONE `records` field (6), each as the record's wire id
    ///   (a varint the kit writes) followed by [`Self::encode`]'s body,
    ///   with no per-record framing. The body must then be
    ///   SELF-DELIMITING: the game's client decoder
    ///   ([`ClientDecoder::run_record`](crate::client::ClientDecoder::run_record))
    ///   reads one body off the front of the rest of the run and knows
    ///   where it ends. Its format is entirely the game's (bit-packed,
    ///   tagless varints, MessagePack, a protobuf message behind the
    ///   game's own length prefix, …); the kit never looks inside, and
    ///   the body need not repeat the id.
    ///
    /// Every client rule is the same in both modes (the records are
    /// absolute, idempotent upserts; the order `removed` → `cell_exits`
    /// → records). The mode is a property of the codec TYPE, so every
    /// room — and every shard of a sharded room — running one game
    /// frames alike: a body a shard exports for another shard's team
    /// frame (`docs/CROSS-SHARD.md` §8b) is spliced in exactly as that
    /// shard's own records.
    const RUN: bool = false;

    /// Append the BODY of the record of entity `id` with value `wire` to
    /// `out` (e.g. a protobuf message's fields). The kit writes the
    /// framing around it: the `entities` field's tag and length, or —
    /// in the record run ([`Self::RUN`]) — the id in front of it.
    fn encode(&self, id: u64, wire: &Self::Wire, out: &mut BytesMut);
}
