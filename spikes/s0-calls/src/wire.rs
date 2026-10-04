//! The wire shapes the spike needs, mirrored by hand from
//! `crates/gsb-protocol/proto/base.proto` (same field numbers) plus the
//! ADDITIVE fields and frames SERVICES.md proposes. A mirror, not a
//! dependency: the spike must not need the protocol crate's build script,
//! and nothing here is a wire change yet.

/// Base-band opcodes. `RPC_REQ` exists today; `RPC_RESP` and `PUSH` are
/// the proposed additions (reliable band, ≤ 64).
pub mod op {
    pub const RPC_REQ: u16 = 12;
    /// Proposed: a correlated answer on the connection's OWN path (hub
    /// requests — there is no room private frame to ride).
    pub const RPC_RESP: u16 = 17;
    /// Proposed: a server push to the client (SERVICES.md §6).
    pub const PUSH: u16 = 18;
}

/// `RpcRequest.target` values (proposed field 4; 0 = today's meaning).
pub mod target {
    pub const ROOM: u32 = 0;
    pub const HUB: u32 = 1;
}

/// `gsb.base.RpcRequest` + the proposed `target` (field 4). A client that
/// never sets it sends today's bytes exactly (proto3 omits a zero).
#[derive(Clone, PartialEq, prost::Message)]
pub struct RpcRequest {
    #[prost(uint64, tag = "1")]
    pub id: u64,
    #[prost(uint32, tag = "2")]
    pub op: u32,
    #[prost(bytes = "vec", tag = "3")]
    pub payload: Vec<u8>,
    #[prost(uint32, tag = "4")]
    pub target: u32,
}

/// `gsb.base.RpcResponse` + the proposed `game_error` (field 6).
/// `ok = false, game_error = true`: `payload` is the typed game error
/// (`Request::Error`); `ok = false, game_error = false`: an engine
/// rejection whose text is `reason` (today's only shape — an old server
/// never sets 6). A flag, not "non-empty payload": a default-valued
/// error message encodes to ZERO bytes (spike finding, SERVICES.md §12).
#[derive(Clone, PartialEq, prost::Message)]
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

/// Proposed `gsb.base.Push` (opcode `PUSH`): a typed game event, opaque to
/// the engine.
#[derive(Clone, PartialEq, prost::Message)]
pub struct Push {
    #[prost(uint32, tag = "1")]
    pub op: u32,
    #[prost(bytes = "vec", tag = "2")]
    pub payload: Vec<u8>,
}

/// What a connection's outbound path carries in the spike: the room's
/// per-tick private frame (today: `Private.responses`), or one of the two
/// proposed base frames that use the connection's own path.
#[derive(Debug, Clone, PartialEq)]
pub enum Frame {
    Private { responses: Vec<RpcResponse> },
    RpcResp(RpcResponse),
    Push(Push),
}

/// Engine rejection texts the spike uses (today's spellings where they
/// exist in `gsb_core::rpc`).
pub mod reason {
    pub const TIMEOUT: &str = "request timed out";
    pub const DUPLICATE: &str = "duplicate request id (the request is still in flight)";
    pub const CONN_CAP: &str = "pending request limit reached (per connection)";
    pub const MALFORMED: &str = "malformed request envelope (or correlation id = 0)";
    pub const NO_HANDLER: &str = "no request handler for op";
    /// Proposed: the payload did not decode as the registered request type.
    pub const UNDECODABLE: &str = "undecodable request payload";
}
