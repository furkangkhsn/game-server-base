//! The kit's input sequence/ack rule (KIT-ARCHITECTURE §4.4: the
//! high-water-mark rule and the `InputAck` are the kit's; decoding and
//! applying the input is the game's `ingest`) and the `Private` frame
//! that carries the ack and the queued RPC answers.

use std::collections::HashMap;

use bytes::BufMut;
use gsb_core::id::PlayerId;
use prost::Message;

use crate::proto;

/// Per-connection input sequence state (see [`Self::admit`] and
/// [`emit_private`]).
///
/// `hwm` is the highest input sequence this connection has *processed*
/// (the high-water mark); `acked` is the highest sequence already
/// *reported* to the connection. Both are reset on every (re)join — a
/// rejoin is a new session, and the client is expected to restart its
/// counter at 1 (the server-side reset makes the first input of the new
/// session processable even if the client forgets). A shard migration is
/// NOT a new session: the sharded rooms carry both across the seam.
#[derive(Debug, Default)]
struct InputState {
    /// Highest processed seq (0 = nothing numbered processed yet).
    hwm: u64,
    /// Highest seq already acked (the last `InputAck` sent).
    acked: u64,
    /// The session has started and the game's session payload
    /// (`Game::session_private`) has not been asked for yet: set by
    /// [`InputSeq::begin`] (a join, a resume), consumed by the first
    /// private frame. A carried session ([`InputSeq::adopt`]) and a
    /// defensive entry are not new sessions: never set there.
    greet: bool,
}

impl InputState {
    /// **Input sequence rule** (the client prediction-reconciliation
    /// signal) — whether an input numbered `seq` is processed. The client
    /// numbers its inputs (monotonic from 1 per session, `seq = 0` =
    /// unnumbered legacy). The server processes a numbered action only
    /// when it is *strictly newer* than this connection's high-water
    /// mark:
    ///
    /// - `seq > hwm` — process, and advance `hwm = seq`;
    /// - `seq <= hwm` — a duplicate or a reordered/late action: **dropped,
    ///   silently**. This is a *normal race* of the lossy game band (a
    ///   retransmission or an out-of-order arrival), not a protocol
    ///   violation: no error is answered and nothing is counted against the
    ///   connection's violation budget (which is spent on structural
    ///   protocol errors, and a client re-sending its own input is always
    ///   legitimate). Applying a stale target would regress the entity to an
    ///   old command, so dropping is the only correct behaviour;
    /// - `seq = 0` (legacy/unnumbered) — process, never advance `hwm`. This
    ///   keeps unnumbered clients (and all pre-seq tests) working unchanged.
    ///
    /// Gaps (a lost input) do not block the mark: the ack is a
    /// high-water mark, not a contiguity claim (see `InputAck` in
    /// `game.proto`).
    #[inline]
    fn admit(&mut self, seq: u64) -> bool {
        if seq == 0 {
            // Legacy/unnumbered: process, never advance the mark.
            true
        } else if seq > self.hwm {
            self.hwm = seq;
            true
        } else {
            false // duplicate / reordered late: dropped (normal race)
        }
    }
}

/// A room's input sequence ledger: one `InputState` per player (keyed
/// by the STABLE player id), strategy-independent — every room numbers
/// and acknowledges its clients' input the same way. The game's input
/// decoder sees only [`Self::admit`]; the session boundaries
/// (`Self::begin` / `Self::end`) and the ack read-out
/// (`emit_private`) are kit bookkeeping.
#[derive(Debug, Default)]
pub struct InputSeq {
    states: HashMap<PlayerId, InputState>,
}

impl InputSeq {
    /// The input sequence rule for `player`'s input numbered `seq` (see
    /// `InputState::admit`): `true` = process it. The game's `ingest`
    /// asks this for every decoded input. A player without a session
    /// entry gets a fresh one (defensive only: a join begins the
    /// session).
    #[inline]
    pub fn admit(&mut self, player: PlayerId, seq: u64) -> bool {
        self.states.entry(player).or_default().admit(seq)
    }

    /// Start `player`'s session (a join, a resume): a fresh mark, and
    /// the game's session payload owed on the next private frame.
    pub(crate) fn begin(&mut self, player: PlayerId) {
        self.states.insert(
            player,
            InputState {
                greet: true,
                ..InputState::default()
            },
        );
    }

    /// End `player`'s input session (a leave).
    pub(crate) fn end(&mut self, player: PlayerId) {
        self.states.remove(&player);
    }

    /// `player`'s session state as `(hwm, acked)` — the high-water mark
    /// and the last reported ack — or `None` when no session is tracked.
    /// Read, not consumed: what a sharded room carries with a migrating
    /// player (the entry leaves only when the move commits).
    pub(crate) fn mark(&self, player: PlayerId) -> Option<(u64, u64)> {
        self.states.get(&player).map(|st| (st.hwm, st.acked))
    }

    /// Continue `player`'s input session from a carried state (a
    /// migration arrival): the sequence rule resumes at `hwm`, and a mark
    /// past `acked` is reported in the next private frame.
    pub(crate) fn adopt(&mut self, player: PlayerId, hwm: u64, acked: u64) {
        self.states.insert(
            player,
            InputState {
                hwm,
                acked,
                greet: false,
            },
        );
    }

    /// Whether `player`'s session payload is owed, consumed: `true` once
    /// per [`Self::begin`].
    pub(crate) fn take_greeting(&mut self, player: PlayerId) -> bool {
        self.states
            .get_mut(&player)
            .is_some_and(|st| std::mem::take(&mut st.greet))
    }

    /// The pending ack for `player`, consumed: `Some(hwm)` when the mark
    /// advanced past the last reported ack (and records it as reported).
    fn take_ack(&mut self, player: PlayerId) -> Option<u64> {
        let st = self.states.get_mut(&player)?;
        (st.hwm > st.acked).then(|| {
            st.acked = st.hwm;
            st.hwm
        })
    }
}

/// Emit this connection's private frame for the tick: the pending input
/// acknowledgment (the `ack` oneof) and/or the connection's queued RPC
/// answers (the `responses` repeated field, see `gsb_core::rpc`), as ONE
/// `Private` frame — the per-tick, per-connection slot of the batch, so
/// everything rides the SAME delivery as that tick's group snapshot (no
/// extra send, no extra await: the tick body stays synchronous).
///
/// Returns `true` when a frame was produced. The ack part advances
/// `acked` only when emitted (the mark is the highest processed seq, so
/// the client's reconciliation stays sound); the responses are delivered
/// exactly once (the actor's queue is drained per tick — see the core's
/// fan-out) and the logic decides their order (arrival order within the
/// tick, per the `gsb_core::rpc` contract). Passing an empty `responses`
/// slice is the ack-only form (the pre-Faz-3 shape every room had before
/// the shard actor gained its RPC machinery).
pub(crate) fn emit_private(
    input: &mut InputSeq,
    player: PlayerId,
    responses: &[gsb_core::rpc::RpcReply],
    out: &mut bytes::BytesMut,
) -> bool {
    let ack_up_to = input.take_ack(player);
    if ack_up_to.is_none() && responses.is_empty() {
        return false;
    }
    let frame = proto::Private {
        payload: ack_up_to.map(|upto| {
            proto::private::Payload::Ack(proto::InputAck {
                processed_up_to: upto,
            })
        }),
        // The core owns both halves of the RPC envelope (base.proto), so
        // the reply's wire shape comes from the core's conversion — the
        // game crate never re-derives the field mapping.
        responses: responses.iter().map(Into::into).collect(),
        // The game's session payload (field 4, the frame's last) is
        // appended behind this frame by `append_session_payload`.
        game: Vec::new(),
    };
    frame
        .encode(out)
        .expect("protobuf encode into an in-memory buffer failed");
    true
}

/// Append this connection's queued RPC answers to a HAND-ENCODED
/// `Private` frame body (the AOI one-shot full path, which cannot go
/// through the generated type without re-encoding the snapshot): field
/// 3 (`responses`, tag 0x1A), one length-delimited
/// `gsb.base.RpcResponse` per entry. No allocation beyond the
/// per-message length probe (responses are rare — the steady-state tick
/// has none).
pub(crate) fn append_responses(responses: &[gsb_core::rpc::RpcReply], out: &mut bytes::BytesMut) {
    for r in responses {
        let msg: gsb_protocol::base::RpcResponse = r.into();
        out.put_u8(0x1A); // Private field 3 (responses), LEN
        prost::encoding::varint::encode_varint(msg.encoded_len() as u64, out);
        msg.encode(out)
            .expect("protobuf encode into an in-memory buffer failed");
    }
}
