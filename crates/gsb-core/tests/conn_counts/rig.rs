//! A connection actor under a test-played registry.

use super::*;

/// The game-band opcode the table registers.
pub const GAME_OP: u16 = op::GAME_BAND_START;

fn table() -> MessageTable {
    let mut t = base_table();
    t.reg::<Heartbeat>(GAME_OP);
    t
}

/// One game-band frame's payload.
pub fn game_frame() -> Vec<u8> {
    Heartbeat { tick: 1 }.encode_to_vec()
}

/// A connection actor; the test is its registry.
pub struct Conn {
    inbox: Mailbox<ConnIn>,
    pub out: mpsc::Receiver<FrameBatch>,
    metrics: mpsc::Receiver<MetricsEvent>,
    actor: tokio::task::JoinHandle<()>,
    registry: mpsc::Receiver<RegistryMsg>,
    /// The room's end of the session's action channel, once seated.
    pub actions: Option<mpsc::Receiver<Action>>,
}

impl Conn {
    /// A fresh connection (not authenticated) whose outbound channel
    /// holds `out_cap` batches.
    pub fn open(out_cap: usize) -> Self {
        let (reg_tx, registry) = channel::<RegistryMsg>(64);
        let (inbox, inbox_rx) = channel::<ConnIn>(64);
        let (out_tx, out) = channel::<FrameBatch>(out_cap);
        let (metrics_tx, metrics) = mpsc::channel::<MetricsEvent>(64);
        let actor = ConnectionActor::new(
            ConnectionId(1),
            SocketAddr::from(([127, 0, 0, 1], 45_002)),
            Arc::new(table()),
            reg_tx,
            inbox_rx,
            out_tx,
            metrics_tx,
            None,
        );
        Self {
            inbox,
            out,
            metrics,
            actor: tokio::spawn(actor.run()),
            registry,
            actions: None,
        }
    }

    /// Local auth, waited for.
    pub async fn auth(&mut self) {
        let auth = base::Auth {
            name: String::new(),
            ticket: Vec::new(),
            protocol_version: 0,
        };
        self.send(op::base::AUTH_REQ, auth.encode_to_vec()).await;
        self.until(op::base::AUTH_RESULT).await;
    }

    /// Join room 1; the test's registry seats the session on an action
    /// channel of `cap` actions.
    pub async fn join(&mut self, cap: usize) {
        let join = base::JoinRoom { room_id: 1 };
        self.send(op::base::JOIN_ROOM_REQ, join.encode_to_vec())
            .await;
        let (actions_tx, actions) = channel::<Action>(cap);
        loop {
            let msg = tokio::time::timeout(WAIT, self.registry.recv())
                .await
                .expect("the join in time")
                .expect("actor alive");
            if let RegistryMsg::SpawnPlayer { reply, .. } = msg {
                let seat = Seat {
                    entity: 7,
                    actions: actions_tx,
                    input_rate: None,
                };
                reply.send(Ok(seat)).expect("actor waits");
                break;
            }
        }
        self.actions = Some(actions);
        self.until(op::base::JOIN_ROOM_RESULT).await;
    }

    pub async fn send(&self, op: u16, payload: Vec<u8>) {
        self.inbox
            .send(ConnIn::Frame(FrameBody::new(op, payload)))
            .await
            .expect("actor alive");
    }

    /// Hand the actor a mailbox message.
    pub async fn tell(&self, msg: ConnIn) {
        self.inbox.send(msg).await.expect("actor alive");
    }

    /// Skip frames until one with `op`; return it.
    pub async fn until(&mut self, op: u16) -> FrameBody {
        loop {
            let batch = tokio::time::timeout(WAIT, self.out.recv())
                .await
                .expect("a frame in time")
                .expect("out open");
            if let Some(f) = batch.into_iter().find(|f| f.op == op) {
                return f;
            }
        }
    }

    /// End the session from the client's side and sum every sample the
    /// actor flushed.
    pub async fn close(self) -> ConnSample {
        self.tell(ConnIn::Closed {
            reason: "test over".into(),
        })
        .await;
        self.ended().await
    }

    /// Wait for the actor to end; its samples, summed.
    pub async fn ended(self) -> ConnSample {
        tokio::time::timeout(WAIT, self.actor)
            .await
            .expect("the actor exits")
            .expect("no panic");
        let mut metrics = self.metrics;
        let mut sum: Option<ConnSample> = None;
        while let Ok(ev) = metrics.try_recv() {
            let MetricsEvent::Conn(s) = ev else { continue };
            sum = Some(match sum {
                None => s,
                Some(t) => add(t, s),
            });
        }
        sum.expect("at least the final sample")
    }
}

/// Two samples' deltas, added (the later one's flags win).
fn add(t: ConnSample, s: ConnSample) -> ConnSample {
    ConnSample {
        bytes_in: t.bytes_in + s.bytes_in,
        bytes_out: t.bytes_out + s.bytes_out,
        frames_in: t.frames_in + s.frames_in,
        frames_out: t.frames_out + s.frames_out,
        actions_dropped: t.actions_dropped + s.actions_dropped,
        actions_dropped_closed: t.actions_dropped_closed + s.actions_dropped_closed,
        requests_dropped_closed: t.requests_dropped_closed + s.requests_dropped_closed,
        requests_dropped_full: t.requests_dropped_full + s.requests_dropped_full,
        requests_no_room: t.requests_no_room + s.requests_no_room,
        metrics_dropped: t.metrics_dropped + s.metrics_dropped,
        violations: t.violations + s.violations,
        input_rate_limited: t.input_rate_limited + s.input_rate_limited,
        ..s
    }
}
