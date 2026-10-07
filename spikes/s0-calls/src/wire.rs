//! The wire shapes the spike needs, mirrored by hand from
//! `crates/gsb-protocol/proto/base.proto` (same field numbers) plus the
//! ADDITIVE `RpcResponse.game_error` flag SERVICES.md proposes. A mirror,
//! not a dependency.

/// `gsb.base.RpcRequest`.
#[derive(Clone, PartialEq, prost::Message)]
pub struct RpcRequest {
    #[prost(uint64, tag = "1")]
    pub id: u64,
    #[prost(uint32, tag = "2")]
    pub op: u32,
    #[prost(bytes = "vec", tag = "3")]
    pub payload: Vec<u8>,
}

/// `gsb.base.RpcResponse` + the proposed `game_error` (field 6).
/// `ok = false, game_error = true`: `payload` is the typed game error;
/// `ok = false, game_error = false`: an engine rejection, text in
/// `reason`. A flag because a default-valued error encodes to ZERO bytes.
#[derive(Clone, PartialEq, Eq, prost::Message)]
pub struct RpcResponse {
    #[prost(uint64, tag = "1")]
    pub id: u64,
    #[prost(bool, tag = "2")]
    pub ok: bool,
    #[prost(uint32, tag = "3")]
    pub op: u32,
    #[prost(string, tag = "4")]
    pub reason: String,
    #[prost(bytes = "vec", tag = "5")]
    pub payload: Vec<u8>,
    #[prost(bool, tag = "6")]
    pub game_error: bool,
}

/// Engine rejection texts (today's spellings where they exist in
/// `gsb_core::rpc`, the rest proposed).
pub mod reason {
    pub const NO_HANDLER: &str = "no request handler for op";
    pub const UNDECODABLE: &str = "undecodable request payload";
    pub const DUPLICATE: &str = "duplicate request id (the request is still in flight)";
    pub const MALFORMED: &str = "malformed request envelope (or correlation id = 0)";
    pub const NOT_BOUND: &str = "session not bound to an owner";
    pub const UNTAKEN: &str = "request not handled this tick";
    pub const UNANSWERED: &str = "request taken but not answered";
    pub const REFUSED_FULL: &str = "service busy";
    pub const REFUSED_GONE: &str = "service unavailable";
    pub const REFUSED_SHARE: &str = "service share exhausted";
    pub const REFUSED_SHED: &str = "service overloaded";
    pub const TIMED_OUT: &str = "request timed out";
    pub const ABANDONED: &str = "service dropped the request";
}
