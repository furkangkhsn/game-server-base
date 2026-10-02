//! The actor-to-room hop: a [`PathState`] as an internal action on the
//! member's own action channel (see the parent's docs for why that
//! channel). A CHILD of [`super`].
//!
//! The marker opcode is `gsb_protocol::op::base::MEMBER_PATH`: a
//! base-band number no client frame can reach a room with (the
//! connection actor forwards only game-band opcodes and the RPC
//! envelope; any other base opcode from the wire is a hard violation).
//! The payload is server-internal, never on the wire — a fixed 20-byte
//! layout, little-endian:
//!
//! ```text
//! [u8 phase][u8 present][u32 rate][u32 demand][u16 loss‰][u32 rtt µs][u32 queue µs]
//! ```
//!
//! `present` has one bit per optional field (rate, demand, loss, rtt,
//! queue — bits 0..=4); an absent field's bytes are zero. Durations are
//! whole microseconds, saturating at `u32::MAX` (~71 minutes).

use std::time::Duration;

use bytes::{Buf, BufMut, Bytes, BytesMut};
use gsb_protocol::op::base::MEMBER_PATH;

use crate::id::{ConnectionId, PlayerId};
use crate::path::{PathPhase, PathState, PathTable};
use crate::room::Action;

const LEN: usize = 20;

const RATE: u8 = 1;
const DEMAND: u8 = 1 << 1;
const LOSS: u8 = 1 << 2;
const RTT: u8 = 1 << 3;
const QUEUE: u8 = 1 << 4;

/// The action that carries `state` from `conn`'s actor to its room (the
/// player is stamped by nobody: the room knows whose channel it pulled).
pub(crate) fn path_action(conn: ConnectionId, state: &PathState) -> Action {
    Action {
        conn,
        player: PlayerId(0),
        op: MEMBER_PATH,
        payload: encode(state),
    }
}

/// The state `a` carries, when it is a path marker (`None` for any other
/// action, and for a marker whose payload is not the layout above).
pub(crate) fn read_path(a: &Action) -> Option<Option<PathState>> {
    (a.op == MEMBER_PATH).then(|| decode(&a.payload))
}

/// Put a pulled marker's state in `table` for `player` (the member whose
/// channel it came from). A marker that does not decode — no connection
/// actor builds one; only a game's own synthesized action could — is
/// ignored: it was never a path.
pub(crate) fn settle(table: &mut PathTable, player: PlayerId, path: Option<PathState>) {
    match path {
        Some(state) => table.set(player, state),
        None => tracing::debug!(%player, "malformed path marker ignored"),
    }
}

fn micros(d: Option<Duration>) -> u32 {
    d.map_or(0, |d| u32::try_from(d.as_micros()).unwrap_or(u32::MAX))
}

pub(super) fn encode(s: &PathState) -> Bytes {
    let mut present = 0;
    for (bit, on) in [
        (RATE, s.rate.is_some()),
        (DEMAND, s.demand.is_some()),
        (LOSS, s.loss_permille.is_some()),
        (RTT, s.rtt.is_some()),
        (QUEUE, s.queue_delay.is_some()),
    ] {
        if on {
            present |= bit;
        }
    }
    let mut b = BytesMut::with_capacity(LEN);
    b.put_u8(match s.phase {
        PathPhase::Open => 0,
        PathPhase::Suspect => 1,
        PathPhase::Paced => 2,
    });
    b.put_u8(present);
    b.put_u32_le(s.rate.unwrap_or(0));
    b.put_u32_le(s.demand.unwrap_or(0));
    b.put_u16_le(s.loss_permille.unwrap_or(0));
    b.put_u32_le(micros(s.rtt));
    b.put_u32_le(micros(s.queue_delay));
    b.freeze()
}

pub(super) fn decode(payload: &[u8]) -> Option<PathState> {
    if payload.len() != LEN {
        return None;
    }
    let mut b = payload;
    let phase = match b.get_u8() {
        0 => PathPhase::Open,
        1 => PathPhase::Suspect,
        2 => PathPhase::Paced,
        _ => return None,
    };
    let present = b.get_u8();
    let has = |bit: u8| present & bit != 0;
    let rate = b.get_u32_le();
    let demand = b.get_u32_le();
    let loss = b.get_u16_le();
    let rtt = b.get_u32_le();
    let queue = b.get_u32_le();
    let us = |v: u32| Duration::from_micros(u64::from(v));
    Some(PathState {
        phase,
        rate: has(RATE).then_some(rate),
        demand: has(DEMAND).then_some(demand),
        loss_permille: has(LOSS).then_some(loss),
        rtt: has(RTT).then(|| us(rtt)),
        queue_delay: has(QUEUE).then(|| us(queue)),
    })
}
