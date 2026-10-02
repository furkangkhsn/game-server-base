//! The rig: a room that reports what its logic sees behind a live
//! registry, and a client that drives a real connection actor through
//! its inbox.

use super::*;

pub const WAIT: Duration = Duration::from_secs(5);

/// What the room's logic saw: `Seen::Budget(player, budget)` every
/// update, `Seen::Op(op)` for every action it ingested.
#[derive(Debug, PartialEq)]
pub enum Seen {
    Budget(u64, Option<usize>),
    Op(u16),
}

struct Watching {
    players: Vec<PlayerId>,
    seen: mpsc::UnboundedSender<Seen>,
}

impl GameLogic<()> for Watching {
    type GroupKey = ();
    type Strip = ();
    fn snapshot_op(&self) -> u16 {
        0x7F40
    }
    fn private_op(&self) -> u16 {
        0x7F41
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
        self.players.push(PlayerId(c.0));
        Admission {
            player: PlayerId(c.0),
            entity: c.0,
        }
    }
    fn on_leave(&mut self, _w: &mut (), p: PlayerId) {
        self.players.retain(|x| *x != p);
    }
    fn ingest(&mut self, _w: &mut (), _c: &TickCtx, actions: &mut Vec<Action>) {
        for a in actions.drain(..) {
            let _ = self.seen.send(Seen::Op(a.op));
        }
    }
    fn update(&mut self, _w: &mut (), ctx: &TickCtx) {
        for p in &self.players {
            let _ = self.seen.send(Seen::Budget(p.0, ctx.budget(*p)));
        }
    }
}

impl RoomLogic<()> for Watching {}

/// The one registered game-band opcode (input that fills a channel).
pub const GAME_OP: u16 = op::GAME_BAND_START;

pub fn table() -> gsb_protocol::MessageTable {
    let mut t = base_table();
    t.reg::<base::Heartbeat>(GAME_OP);
    t
}

pub async fn registry() -> (Mailbox<RegistryMsg>, mpsc::UnboundedReceiver<Seen>) {
    registry_with(RoomConfig::default().action_capacity).await
}

/// A live registry with room 1 at 30 Hz whose per-member action channels
/// hold `action_capacity` actions.
pub async fn registry_with(
    action_capacity: usize,
) -> (Mailbox<RegistryMsg>, mpsc::UnboundedReceiver<Seen>) {
    let (seen_tx, seen) = mpsc::unbounded_channel();
    let factory: RoomFactory<(), (), (), ()> = Arc::new(move |_id, _cfg| BuiltRoom::Single {
        world: (),
        logic: Box::new(Watching {
            players: Vec::new(),
            seen: seen_tx.clone(),
        }) as Box<dyn RoomLogic<(), GroupKey = (), Strip = ()>>,
    });
    let (tx, rx) = channel::<RegistryMsg>(256);
    let (ticker, _task) = Ticker::spawn(30.0, 64).expect("valid tick rate");
    let (metrics, _m) = mpsc::channel::<MetricsEvent>(64);
    tokio::spawn(Registry::new(rx, tx.clone(), factory, ticker, metrics, None, None, None).run());
    let (reply, created) = tokio::sync::oneshot::channel();
    let config = RoomConfig {
        id: RoomId(1),
        action_capacity,
        ..Default::default()
    };
    tx.send(RegistryMsg::CreateRoom { config, reply })
        .await
        .expect("registry alive");
    let created = tokio::time::timeout(WAIT, created).await.expect("in time");
    created.expect("reply").expect("room 1 created");
    (tx, seen)
}

pub struct Client {
    inbox: Mailbox<ConnIn>,
    out: mpsc::Receiver<FrameBatch>,
    _metrics: mpsc::Receiver<MetricsEvent>,
}

impl Client {
    pub async fn auth(reg: &Mailbox<RegistryMsg>, conn: u64) -> Self {
        let (inbox, inbox_rx) = channel::<ConnIn>(64);
        let (out_tx, out) = channel::<FrameBatch>(64);
        let (metrics_tx, metrics) = mpsc::channel::<MetricsEvent>(64);
        let actor = ConnectionActor::new(
            ConnectionId(conn),
            SocketAddr::from(([127, 0, 0, 1], 44_000 + conn as u16)),
            Arc::new(table()),
            reg.clone(),
            inbox_rx,
            out_tx,
            metrics_tx,
            None,
        );
        tokio::spawn(actor.run());
        let mut c = Self {
            inbox,
            out,
            _metrics: metrics,
        };
        let auth = base::Auth {
            name: String::new(),
            ticket: Vec::new(),
            protocol_version: 0,
        };
        c.send(FrameBody::new(op::base::AUTH_REQ, auth.encode_to_vec()))
            .await;
        c.expect(op::base::AUTH_RESULT).await;
        c
    }

    pub async fn join(&mut self) {
        let join = base::JoinRoom { room_id: 1 };
        self.send(FrameBody::new(
            op::base::JOIN_ROOM_REQ,
            join.encode_to_vec(),
        ))
        .await;
        self.expect(op::base::JOIN_ROOM_RESULT).await;
    }

    pub async fn send(&self, frame: FrameBody) {
        self.inbox
            .send(ConnIn::Frame(frame))
            .await
            .expect("actor alive");
    }

    pub async fn path(&self, state: PathState) {
        self.inbox
            .send(ConnIn::Path(state))
            .await
            .expect("actor alive");
    }

    /// The first frame with `code`; frames before it are skipped.
    pub async fn expect(&mut self, code: u16) -> FrameBody {
        loop {
            let batch = tokio::time::timeout(WAIT, self.out.recv()).await;
            for f in batch.expect("a frame in time").expect("out open") {
                if f.op == code {
                    return f;
                }
            }
        }
    }
}
