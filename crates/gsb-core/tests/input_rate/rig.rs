//! The rig: a counting room behind a live registry, and a client that
//! drives a real connection actor through its inbox.

use super::*;

/// Every ingested action: `(player, tick)`.
pub type Ingests = mpsc::UnboundedSender<(u64, u64)>;

/// A logic that reports every action it ingests and does nothing else.
pub struct Counting(Ingests);

impl GameLogic<()> for Counting {
    type GroupKey = ();
    type Strip = ();
    fn snapshot_op(&self) -> u16 {
        0x7F30
    }
    fn private_op(&self) -> u16 {
        0x7F31
    }
    fn group_of(&self, _w: &(), _p: PlayerId) -> Self::GroupKey {}
    fn snapshot(
        &mut self,
        _w: &mut (),
        _c: &TickCtx,
        _g: &(),
        _b: &[gsb_core::shard::BorderRecord<()>],
        _o: &mut bytes::BytesMut,
    ) -> bool {
        false
    }
    fn on_join(&mut self, _w: &mut (), c: ConnectionId) -> Admission {
        Admission {
            player: PlayerId(c.0),
            entity: c.0,
        }
    }
    fn on_leave(&mut self, _w: &mut (), _p: PlayerId) {}
    fn ingest(&mut self, _w: &mut (), ctx: &TickCtx, actions: &mut Vec<Action>) {
        for a in actions.drain(..) {
            let _ = self.0.send((a.player.0, ctx.tick));
        }
    }
    fn update(&mut self, _w: &mut (), _c: &TickCtx) {}
}

impl RoomLogic<()> for Counting {}

/// A live registry with room 1 at 30 Hz limiting input at `rate`.
pub async fn registry(
    rate: Option<InputRate>,
) -> (Mailbox<RegistryMsg>, mpsc::UnboundedReceiver<(u64, u64)>) {
    let (seen, ingests) = mpsc::unbounded_channel();
    let factory: RoomFactory<(), (), (), ()> = Arc::new(move |_id, _cfg| BuiltRoom::Single {
        world: (),
        logic: Box::new(Counting(seen.clone()))
            as Box<dyn RoomLogic<(), GroupKey = (), Strip = ()>>,
    });
    let (tx, rx) = channel::<RegistryMsg>(4096);
    let (ticker, _task) = Ticker::spawn(30.0, 64).expect("valid tick rate");
    let (metrics, _m) = mpsc::channel::<MetricsEvent>(64);
    tokio::spawn(Registry::new(rx, tx.clone(), factory, ticker, metrics, None, None, None).run());
    let (reply, created) = tokio::sync::oneshot::channel();
    let config = RoomConfig {
        id: RoomId(1),
        input_rate: rate,
        ..Default::default()
    };
    tx.send(RegistryMsg::CreateRoom { config, reply })
        .await
        .expect("registry alive");
    let created = tokio::time::timeout(WAIT, created).await.expect("in time");
    created.expect("reply").expect("room 1 created");
    (tx, ingests)
}

/// One client: its actor's inbox, outbound frames and metrics samples.
pub struct Client {
    pub inbox: Mailbox<ConnIn>,
    pub out: mpsc::Receiver<FrameBatch>,
    pub metrics: mpsc::Receiver<MetricsEvent>,
    pub actor: tokio::task::JoinHandle<()>,
}

impl Client {
    /// Connect `conn`, authenticate, join room 1.
    pub async fn login(reg: &Mailbox<RegistryMsg>, conn: u64) -> Self {
        let (inbox, inbox_rx) = channel::<ConnIn>(4096);
        let (out_tx, out) = channel::<FrameBatch>(4096);
        let (metrics_tx, metrics) = mpsc::channel::<MetricsEvent>(4096);
        let actor = ConnectionActor::new(
            ConnectionId(conn),
            SocketAddr::from(([127, 0, 0, 1], 43_000 + conn as u16)),
            Arc::new(table()),
            reg.clone(),
            inbox_rx,
            out_tx,
            metrics_tx,
            None,
        );
        let mut c = Self {
            inbox,
            out,
            metrics,
            actor: tokio::spawn(actor.run()),
        };
        let auth = base::Auth {
            name: String::new(),
            ticket: Vec::new(),
            protocol_version: 0,
        };
        c.send(FrameBody::new(op::base::AUTH_REQ, auth.encode_to_vec()))
            .await;
        let join = base::JoinRoom { room_id: 1 };
        c.send(FrameBody::new(
            op::base::JOIN_ROOM_REQ,
            join.encode_to_vec(),
        ))
        .await;
        c.expect(op::base::JOIN_ROOM_RESULT).await;
        c
    }

    pub async fn send(&self, frame: FrameBody) {
        self.inbox
            .send(ConnIn::Frame(frame))
            .await
            .expect("actor alive");
    }

    /// Wait for a frame with `code`; any ERROR on the way fails the test.
    pub async fn expect(&mut self, code: u16) {
        loop {
            let batch = tokio::time::timeout(WAIT, self.out.recv()).await;
            for f in batch.expect("a frame in time").expect("out open") {
                assert_ne!(f.op, op::base::ERROR, "no ERROR was expected");
                if f.op == code {
                    return;
                }
            }
        }
    }

    /// Close the session and return its samples, summed.
    pub async fn close(self) -> ConnSample {
        drop(self.inbox);
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
                Some(t) => ConnSample {
                    actions_dropped: t.actions_dropped + s.actions_dropped,
                    violations: t.violations + s.violations,
                    input_rate_limited: t.input_rate_limited + s.input_rate_limited,
                    frames_in: t.frames_in + s.frames_in,
                    ..s
                },
            });
        }
        sum.expect("at least the final sample")
    }
}
