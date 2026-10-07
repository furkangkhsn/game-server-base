//! The typed request contract both ends share (`docs/SERVICES.md` §4):
//! [`Call`] for an in-process call, [`Request`] for a client-wire
//! request, [`WireMessage`] for the bytes.
//!
//! It lives in the protocol crate — not in `gsb-core`, which re-exports
//! it as `gsb_core::calls` — because the CLIENT speaks it too: a typed
//! client (`gsb_client::requests`) needs `R::OP`, `R::Response` and
//! `R::Error` without linking the server engine. `gsb-build` generates
//! the impls from a game's `.proto` `service` blocks (`gsb/options.proto`).

/// The wire form of a client-facing message. Every `prost` message has
/// it (a blanket impl); a game that wants another format implements it
/// on its own types.
pub trait WireMessage: Sized + Send + Sync + 'static {
    /// Encode for the wire.
    fn to_wire(&self) -> Vec<u8>;
    /// Decode from the wire.
    fn from_wire(bytes: &[u8]) -> Result<Self, DecodeError>;
}

/// A payload did not decode as the registered type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DecodeError;

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("payload did not decode as the expected message")
    }
}

impl std::error::Error for DecodeError {}

impl<T: prost::Message + Default + Send + Sync + 'static> WireMessage for T {
    fn to_wire(&self) -> Vec<u8> {
        self.encode_to_vec()
    }
    fn from_wire(bytes: &[u8]) -> Result<Self, DecodeError> {
        T::decode(bytes).map_err(|_| DecodeError)
    }
}

/// One in-process call: its answer type and its error type. No opcode,
/// no wire. `#[gsb::service]` generates one per method; `gsb-build`
/// (`.proto` codegen) one per `rpc`, together with [`Request`].
///
/// `Clone` (and `Clone` answers): the server's call table keeps what it
/// needs to re-issue a call after a migration and to copy an outcome
/// into the input log (SERVICES.md §4.2, §19 S5).
pub trait Call: Clone + Send + Sync + 'static {
    /// The successful answer.
    type Response: Clone + Send + Sync + 'static;
    /// The service's own typed error.
    type Error: Clone + Send + Sync + 'static;
}

/// A client-wire request: a [`Call`] whose request, answer and error all
/// have a wire form, plus its opcode (the inner op of the `RpcRequest`
/// envelope). Declared in a game's `.proto` with `option (gsb.op)` and
/// `option (gsb.error)`; `gsb-build` writes the impl.
pub trait Request: Call<Response: WireMessage, Error: WireMessage> + WireMessage {
    /// The request's inner opcode (game band, `>=`
    /// [`GAME_BAND_START`](crate::op::GAME_BAND_START)). Never reused; a
    /// collision is refused at build time (`gsb-build`) and at boot
    /// (`gsb_core::calls::BootError`).
    const OP: u16;
}
