//! The typed mirrors the fixture tests decode the kit's frames with
//! (KIT-ARCHITECTURE §5: a game's typed mirror of the kit's envelope —
//! the fixture record and `Grid2`'s cell exit in place of opaque bytes).

use prost::Message;

/// One entity record (the fixture codec's body).
#[derive(Clone, PartialEq, Message)]
pub(crate) struct Record {
    #[prost(uint64, tag = "1")]
    pub entity: u64,
    #[prost(sint32, tag = "2")]
    pub x: i32,
    #[prost(sint32, tag = "3")]
    pub y: i32,
}

/// One cell exit (the `Grid2` preset's body).
#[derive(Clone, PartialEq, Message)]
pub(crate) struct CellExit {
    #[prost(sint32, tag = "1")]
    pub x: i32,
    #[prost(sint32, tag = "2")]
    pub y: i32,
}

/// The typed mirror of the kit's `WorldSnapshot`.
#[derive(Clone, PartialEq, Message)]
pub(crate) struct WorldSnapshot {
    #[prost(uint64, tag = "1")]
    pub sequence: u64,
    #[prost(message, repeated, tag = "2")]
    pub entities: Vec<Record>,
    #[prost(uint64, repeated, tag = "3")]
    pub removed: Vec<u64>,
    #[prost(message, repeated, tag = "4")]
    pub cell_exits: Vec<CellExit>,
    #[prost(bool, tag = "5")]
    pub delta: bool,
}

/// The typed mirror of the kit's `Private` (the game slot, field 4, is
/// left out — the fixture sends none).
#[derive(Clone, PartialEq, Message)]
pub(crate) struct Private {
    #[prost(oneof = "private::Payload", tags = "1, 2")]
    pub payload: Option<private::Payload>,
    #[prost(message, repeated, tag = "3")]
    pub responses: Vec<gsb_protocol::base::RpcResponse>,
}

/// The `Private.payload` oneof.
pub(crate) mod private {
    /// An ack or a one-shot full.
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub(crate) enum Payload {
        #[prost(message, tag = "1")]
        Ack(gsb_kit::proto::InputAck),
        #[prost(message, tag = "2")]
        Snapshot(super::WorldSnapshot),
    }
}
