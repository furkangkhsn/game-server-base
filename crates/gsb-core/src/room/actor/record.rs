//! Recording a room's input (feature `record`, `docs/SERVICES.md`
//! §10.1): opt in per room with [`RoomActor::with_record`]; from then on
//! every step sends one [`TickRecord`] — `dt`, the inputs the logic saw in
//! order (the call table keeps them as the phases run), the step's state
//! hash — to the recorder's bounded sink with a `try_send` (the tick never
//! awaits; a full or gone sink loses the record and counts it).

use std::fmt::Debug;
use std::hash::Hash;
use std::time::Duration;

use crate::determinism::{LogHeader, RecordMsg, Recorder, TickRecord};
use crate::room::actor::RoomActor;

impl<W, G, Sp> RoomActor<W, G, Sp>
where
    G: Eq + Hash + Clone + Debug,
    Sp: Debug + Clone + PartialEq + Send + 'static,
{
    /// Record this room's input into `recorder`'s sink (its header first:
    /// the versions, the room, its seed). Opt-in per room; without it the
    /// room keeps no input at all.
    pub fn with_record(mut self, recorder: Recorder) -> Self {
        self.start_record(recorder);
        self
    }

    pub(in crate::room) fn start_record(&mut self, recorder: Recorder) {
        let header = LogHeader::of(&self.config, self.seed, &recorder.game);
        let start = RecordMsg::Start(header, self.calls.codecs().clone());
        self.send_record(&recorder, start);
        self.calls.start_recording();
        self.recorder = Some(recorder);
    }

    fn send_record(&mut self, recorder: &Recorder, msg: RecordMsg) {
        match recorder.sink.try_send(msg) {
            Ok(()) => self.calls.counts.records_written += 1,
            Err(_) => self.calls.counts.records_dropped += 1,
        }
    }

    /// The step's record (its end): the inputs the table kept, the hash.
    pub(in crate::room) fn finish_record(&mut self, tick: u64, dt: Duration) {
        let Some(recorder) = self.recorder.take() else {
            return;
        };
        let inputs = self.calls.take_inputs();
        let hash = self.state_hash();
        let msg = RecordMsg::Tick(TickRecord {
            tick,
            dt,
            inputs,
            hash,
        });
        self.send_record(&recorder, msg);
        self.recorder = Some(recorder);
    }
}
