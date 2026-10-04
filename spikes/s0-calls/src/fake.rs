//! Developer experience (SERVICES.md §10): a fake service driven by the
//! test clock. No runtime, no HTTP: the test decides when the service
//! reads its mailbox and how many ticks later each answer goes out.
//!
//! ```text
//! loop { room.tick(); fake.step(room.now(), |msg, s| s.after(3, move || reply...)); }
//! ```

use tokio::sync::mpsc;

use crate::service::ServiceRef;

type Job = Box<dyn FnOnce() + Send>;

/// A service whose answers are scheduled in ticks.
pub struct FakeService<M> {
    rx: Option<mpsc::Receiver<M>>,
    later: Vec<(u64, u64, Job)>,
    seq: u64,
    /// Messages taken from the mailbox so far.
    pub received: u64,
}

/// Lets the handler schedule work relative to the test clock.
pub struct Scheduler<'a> {
    now: u64,
    seq: &'a mut u64,
    later: &'a mut Vec<(u64, u64, Job)>,
}

impl Scheduler<'_> {
    pub fn now(&self) -> u64 {
        self.now
    }

    /// Run `job` at the first `step` whose clock is ≥ now + `ticks`
    /// (0 = in this same step, after the mailbox is drained).
    pub fn after(&mut self, ticks: u64, job: impl FnOnce() + Send + 'static) {
        *self.seq += 1;
        self.later
            .push((self.now + ticks, *self.seq, Box::new(job)));
    }
}

impl<M> FakeService<M> {
    pub fn new(name: &'static str, capacity: usize) -> (ServiceRef<M>, Self) {
        let (svc, rx) = ServiceRef::channel(name, capacity);
        (
            svc,
            Self {
                rx: Some(rx),
                later: Vec::new(),
                seq: 0,
                received: 0,
            },
        )
    }

    /// Read everything queued, hand each message to `handle`, then run the
    /// jobs that are due at `now` (in schedule order).
    pub fn step(&mut self, now: u64, mut handle: impl FnMut(M, &mut Scheduler<'_>)) {
        if let Some(rx) = &mut self.rx {
            while let Ok(m) = rx.try_recv() {
                self.received += 1;
                let mut s = Scheduler {
                    now,
                    seq: &mut self.seq,
                    later: &mut self.later,
                };
                handle(m, &mut s);
            }
        }
        let (mut due, keep): (Vec<_>, Vec<_>) = std::mem::take(&mut self.later)
            .into_iter()
            .partition(|(at, _, _)| *at <= now);
        self.later = keep;
        due.sort_by_key(|(at, seq, _)| (*at, *seq));
        for (_, _, job) in due {
            job();
        }
    }

    /// Simulate the service going away: its mailbox closes (later calls
    /// are refused `Gone`) and scheduled answers are dropped — every
    /// `Reply` they held reports `Abandoned`.
    pub fn kill(&mut self) {
        self.rx = None;
        self.later.clear();
    }

    /// Answers still scheduled.
    pub fn scheduled(&self) -> usize {
        self.later.len()
    }
}
