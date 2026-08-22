//! Wire protocol for gsb servers.
//!
//! Wire format (per message, on top of the transport):
//!
//! ```text
//! [u32 LE total frame body length][u16 LE opcode][payload (protobuf)]
//! ```
//!
//! The 4-byte length prefix is added by the *transport* (see `gsb-net`); this
//! crate defines the frame body, the opcode registry, and the
//! [`MessageTable`] used to decode inbound frames and encode outbound ones.
//!
//! Payloads are protobuf messages (see `proto/*.proto`), which are shared
//! with the Unity client side via Google.Protobuf.
//!
//! Opcode layout:
//! - `1..=64`     base control band (defined in [`op`])
//! - `1000..=`    game band (each game crate defines its own opcodes)

pub mod op {
    /// Base control band.
    pub mod base {
        pub const AUTH_REQ: u16 = 1;
        pub const AUTH_RESULT: u16 = 2;
        pub const JOIN_ROOM_REQ: u16 = 3;
        pub const JOIN_ROOM_RESULT: u16 = 4;
        pub const LEAVE_ROOM_REQ: u16 = 5;
        pub const LEAVE_ROOM_RESULT: u16 = 6;
        pub const HEARTBEAT: u16 = 7;
        pub const HEARTBEAT_ACK: u16 = 8;
        pub const ERROR: u16 = 9;
        /// Correlated request envelope (client → server, room-scoped).
        /// Carries the client's correlation id + an inner (game-band)
        /// opcode and payload; the room answers on the per-connection
        /// private frame path (see `gsb_core::rpc`). The connection actor
        /// forwards it to the room as an opaque action (the room's core
        /// decodes the envelope); the response is a `Private.responses`
        /// entry, not a frame of its own.
        pub const RPC_REQ: u16 = 12;
        /// rUDP transport-level marker (NOT a message-table message): the
        /// stateless handshake challenge/proof that travels in its own
        /// datagram kind (see `gsb_net::udp`), handled entirely below the
        /// actor layer. Payload: `[u64 LE nonce][u64 LE cookie]`.
        pub const UDP_HELLO: u16 = 10;
        /// rUDP transport-level marker (NOT a message-table message): a
        /// cumulative reliable-band acknowledgment, handled below the
        /// actor layer (the connection actor never sees it). Payload:
        /// `[u32 LE next expected sequence]`.
        pub const UDP_ACK: u16 = 11;
    }

    /// First opcode reserved for game crates.
    pub const GAME_BAND_START: u16 = 1000;
}

/// Generated base protocol messages (package `gsb.base`, file `base.proto`).
pub mod base {
    include!(concat!(env!("OUT_DIR"), "/gsb.base.rs"));
}

use std::any::Any;
use std::collections::HashMap;

/// Errors produced by the protocol layer.
#[derive(Debug, thiserror::Error)]
pub enum ProtoError {
    #[error("unknown opcode {0:#04x}")]
    UnknownOpcode(u16),

    #[error("failed to decode message for opcode {op:#04x}: {reason}")]
    Decode { op: u16, reason: String },

    #[error("malformed frame body: {0} bytes (need >= 2)")]
    MalformedFrame(usize),

    #[error("not authenticated")]
    NotAuthenticated,

    #[error("already authenticated")]
    AlreadyAuthenticated,

    #[error("room {0} not found")]
    RoomNotFound(u64),

    #[error("not in a room")]
    NotInRoom,

    #[error("protocol error: {0}")]
    Other(String),
}

/// A decoded message payload without its envelope.
///
/// The *frame body* on the wire is `[u16 LE opcode][payload]`; the length
/// prefix around it is added by the transport.
#[derive(Debug, Clone)]
pub struct FrameBody {
    pub op: u16,
    pub payload: bytes::Bytes,
}

impl FrameBody {
    pub fn new(op: u16, payload: impl Into<bytes::Bytes>) -> Self {
        Self {
            op,
            payload: payload.into(),
        }
    }

    /// Encode to the wire body: `[u16 LE op][payload]`.
    pub fn encode(&self) -> bytes::Bytes {
        let mut out = bytes::BytesMut::with_capacity(2 + self.payload.len());
        out.extend_from_slice(&self.op.to_le_bytes());
        out.extend_from_slice(&self.payload);
        out.freeze()
    }

    /// Decode from the wire body.
    pub fn decode(mut body: bytes::Bytes) -> Result<Self, ProtoError> {
        if body.len() < 2 {
            return Err(ProtoError::MalformedFrame(body.len()));
        }
        let op = u16::from_le_bytes([body[0], body[1]]);
        let payload = body.split_off(2);
        Ok(Self { op, payload })
    }
}

type Decoded = Box<dyn Any + Send>;
type Decoder = Box<dyn Fn(&[u8]) -> Result<Decoded, ProtoError> + Send + Sync>;
type Encoder = Box<dyn Fn(&dyn Any) -> Option<bytes::Bytes> + Send + Sync>;

/// Registry mapping opcodes to (de)coders for concrete protobuf message
/// types.
///
/// The table is built once at server startup (base messages + the game
/// crate's messages) and then shared read-only between actors via `Arc`.
#[derive(Default)]
pub struct MessageTable {
    dec: HashMap<u16, Decoder>,
    enc: HashMap<u16, Encoder>,
}

impl MessageTable {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register both the decoder and encoder for `T` under `op`.
    pub fn reg<T: prost::Message + Default + 'static>(&mut self, op: u16) {
        let dec = move |buf: &[u8]| -> Result<Decoded, ProtoError> {
            let mut msg = T::default();
            T::merge(&mut msg, buf).map_err(|e| ProtoError::Decode {
                op,
                reason: e.to_string(),
            })?;
            Ok(Box::new(msg))
        };
        let enc = move |any: &dyn Any| -> Option<bytes::Bytes> {
            any.downcast_ref::<T>()
                .map(|m| bytes::Bytes::from(m.encode_to_vec()))
        };
        self.dec.insert(op, Box::new(dec));
        self.enc.insert(op, Box::new(enc));
    }

    pub fn is_registered(&self, op: u16) -> bool {
        self.dec.contains_key(&op)
    }

    /// Decode a frame body into a type-erased message.
    pub fn decode(&self, op: u16, payload: &[u8]) -> Result<Decoded, ProtoError> {
        match self.dec.get(&op) {
            Some(dec) => dec(payload),
            None => Err(ProtoError::UnknownOpcode(op)),
        }
    }

    /// Encode a message (by type-erased reference) into a frame body.
    pub fn frame(&self, op: u16, msg: &dyn Any) -> Option<FrameBody> {
        let enc = self.enc.get(&op)?;
        enc(msg).map(|payload| FrameBody { op, payload })
    }

    pub fn opcodes(&self) -> Vec<u16> {
        let mut v: Vec<u16> = self.dec.keys().copied().collect();
        v.sort_unstable();
        v
    }
}

/// A table preloaded with all base (transport-agnostic) messages.
pub fn base_table() -> MessageTable {
    let mut t = MessageTable::new();
    t.reg::<base::Auth>(op::base::AUTH_REQ);
    t.reg::<base::AuthResult>(op::base::AUTH_RESULT);
    t.reg::<base::JoinRoom>(op::base::JOIN_ROOM_REQ);
    t.reg::<base::JoinRoomResult>(op::base::JOIN_ROOM_RESULT);
    t.reg::<base::LeaveRoom>(op::base::LEAVE_ROOM_REQ);
    t.reg::<base::LeaveRoomResult>(op::base::LEAVE_ROOM_RESULT);
    t.reg::<base::Heartbeat>(op::base::HEARTBEAT);
    t.reg::<base::HeartbeatAck>(op::base::HEARTBEAT_ACK);
    t.reg::<base::Error>(op::base::ERROR);
    t.reg::<base::RpcRequest>(op::base::RPC_REQ);
    t
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_roundtrip() {
        let fb = FrameBody::new(42, b"hello".as_slice());
        let wire = fb.encode();
        assert_eq!(&wire[..2], &42u16.to_le_bytes());
        let back = FrameBody::decode(wire).unwrap();
        assert_eq!(back.op, 42);
        assert_eq!(back.payload, b"hello".as_slice());
    }

    #[test]
    fn malformed_frame_rejected() {
        assert!(matches!(
            FrameBody::decode(bytes::Bytes::from_static(&[1])),
            Err(ProtoError::MalformedFrame(1))
        ));
    }

    #[test]
    fn table_roundtrip() {
        let table = base_table();
        let msg = base::Auth {
            name: "neo".into(),
            ticket: vec![],
        };
        let fb = table.frame(op::base::AUTH_REQ, &msg).expect("encode");
        let decoded = table.decode(fb.op, &fb.payload).expect("decode");
        let auth = decoded.downcast_ref::<base::Auth>().expect("type");
        assert_eq!(auth.name, "neo");
    }

    #[test]
    fn unknown_opcode_errors() {
        let table = base_table();
        assert!(matches!(
            table.decode(9999, &[]),
            Err(ProtoError::UnknownOpcode(9999))
        ));
    }
}
