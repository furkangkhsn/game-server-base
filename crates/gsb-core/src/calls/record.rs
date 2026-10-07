//! The table's side of the input log and of REPLAY (feature `record`,
//! `docs/SERVICES.md` §10): while recording, every input the logic sees
//! through the table — and the CONTROL/READ inputs the actor notes on it
//! — is kept in order for the tick's record; in REPLAY the table never
//! sends (a call is written pending under its key and waits), and its
//! endings come from the log, matched by key and checked by call type.

use std::borrow::Cow;

use crate::calls::WireMessage;
use crate::calls::contract::{Call, Erased, Owner};
use crate::calls::engine::{EXTERNAL, External};
use crate::calls::table::Calls;
use crate::determinism::{Arrival, Codecs, Input, Recorded, RecordedOutcome};
use crate::room::RoomCounters;

/// What a recording table keeps for the tick's record.
#[derive(Debug, Default)]
pub(crate) struct Rec {
    pub(crate) inputs: Vec<Input>,
    pub(crate) responses: Vec<Recorded>,
}

impl Calls {
    /// Cover `C`'s outcomes in a log FILE (a `forward` rule's call is
    /// covered on its own; a log replayed from memory needs nothing).
    /// Boot-time, like `accept`.
    pub fn record_as<C>(&mut self)
    where
        C: Call<Response: WireMessage, Error: WireMessage>,
    {
        self.codecs.register::<C>();
    }

    /// The codecs this table's calls are written with.
    pub fn codecs(&self) -> &Codecs {
        &self.codecs
    }

    /// Start keeping the inputs (the actor's recorder starts it).
    pub(crate) fn start_recording(&mut self) {
        if self.rec.is_none() {
            self.rec = Some(Box::default());
        }
    }

    /// Whether the table records.
    pub(crate) fn recording(&self) -> bool {
        self.rec.is_some()
    }

    /// Note one input (a no-op unless recording).
    pub(crate) fn note(&mut self, input: impl FnOnce() -> Input) {
        if let Some(r) = &mut self.rec {
            r.inputs.push(input());
        }
    }

    /// The tick's inputs (empty unless recording).
    pub(crate) fn take_inputs(&mut self) -> Vec<Input> {
        match &mut self.rec {
            Some(r) => std::mem::take(&mut r.inputs),
            None => Vec::new(),
        }
    }

    /// RESPONSES' endings, taken in as one input (only when there were
    /// any).
    pub(crate) fn note_responses(&mut self) {
        if let Some(r) = &mut self.rec
            && !r.responses.is_empty()
        {
            let list = std::mem::take(&mut r.responses);
            r.inputs.push(Input::Responses(list));
        }
    }

    /// Keep one ending of a call the table held.
    pub(crate) fn note_ending(
        &mut self,
        call: &'static str,
        copy: fn(&Erased) -> Erased,
        outcome: &Erased,
        arrival: Arrival,
        alive: bool,
        key: crate::calls::CallKey,
    ) {
        if let Some(r) = &mut self.rec {
            r.responses.push(Recorded {
                key,
                call: Cow::Borrowed(call),
                arrival,
                alive,
                outcome: RecordedOutcome::Value(copy(outcome)),
            });
        }
    }

    /// Keep one late answer (a key the table no longer held).
    pub(crate) fn note_late(&mut self, key: crate::calls::CallKey, external: bool) {
        if let Some(r) = &mut self.rec {
            r.responses.push(Recorded {
                key,
                call: Cow::Borrowed(if external { EXTERNAL } else { "" }),
                arrival: Arrival::Late,
                alive: false,
                outcome: RecordedOutcome::Missing,
            });
        }
    }

    /// REPLAY mode: the table never sends; endings come from the log.
    pub(crate) fn set_replay(&mut self) {
        self.replay = true;
    }

    /// REPLAY's RESPONSES: apply one tick's recorded endings, in order.
    /// An ending is applied only to a pending call of the SAME type
    /// (`replay_diverged` otherwise, and for a key nothing waits under —
    /// the log does not fit this game); an outcome no codec can decode
    /// is `replay_undecodable`.
    pub(crate) fn replay_responses(&mut self, list: &[Recorded], m: &mut RoomCounters) {
        for r in list {
            match r.arrival {
                Arrival::Late => {
                    if r.call == EXTERNAL {
                        m.requests_late += 1;
                    } else {
                        self.counts.results_late += 1;
                    }
                }
                Arrival::Deadline => self.expire_external(r.key, m),
                Arrival::Ended => {
                    let Some(e) = self.pending.get(&r.key) else {
                        self.counts.replay_diverged += 1;
                        continue;
                    };
                    if e.call != r.call {
                        self.counts.replay_diverged += 1;
                        continue;
                    }
                    let outcome = match &r.outcome {
                        RecordedOutcome::Value(o) => Some((e.copy)(o)),
                        RecordedOutcome::Encoded(b) => self.codecs.decode(e.call, b),
                        RecordedOutcome::Missing => None,
                    };
                    let Some(outcome) = outcome else {
                        self.counts.replay_undecodable += 1;
                        continue;
                    };
                    let e = self.pending.remove(&r.key).expect("checked above");
                    let alive = r.alive;
                    self.finish(r.key, e, outcome, &|_: Owner| alive);
                }
            }
        }
    }
}

/// The External call's outcome codec (bytes and a reason text).
pub(crate) fn external_codec() -> crate::determinism::Codec {
    crate::determinism::Codec {
        encode: |o| {
            crate::determinism::encode_with::<External>(
                o,
                |b| b.to_vec(),
                |s| s.as_bytes().to_vec(),
            )
        },
        decode: |b| {
            crate::determinism::decode_with::<External>(
                b,
                |b| Some(bytes::Bytes::copy_from_slice(b)),
                |b| String::from_utf8(b.to_vec()).ok(),
            )
        },
    }
}
