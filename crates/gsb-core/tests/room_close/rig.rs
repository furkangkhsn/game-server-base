//! The end-to-end rig: a live registry hosting room 1 with the input-idle
//! ceiling, and clients that drive real connection actors through their
//! inboxes (local auth: the name is the resume identity).

use super::*;

/// The one registered game-band opcode.
pub const GAME_OP: u16 = op::GAME_BAND_START;

/// The ceiling every end-to-end room runs with, in seconds.
pub const CEILING: u64 = 2;

pub fn table() -> MessageTable {
    let mut t = base_table();
    t.reg::<Heartbeat>(GAME_OP);
    t
}

/// A live registry with room 1 (30 Hz, the ceiling, `action`), built by
/// `factory`, capped at `cap` members.
pub async fn registry(
    factory: RoomFactory<(), (), (), ()>,
    action: AfkAction,
    cap: Option<usize>,
) -> Mailbox<RegistryMsg> {
    let (tx, rx) = channel::<RegistryMsg>(4096);
    let (ticker, _task) = Ticker::spawn(30.0, 64).expect("valid tick rate");
    let (metrics, _m) = mpsc::channel::<MetricsEvent>(64);
    tokio::spawn(Registry::new(rx, tx.clone(), factory, ticker, metrics, None, None, None).run());
    let config = RoomConfig {
        id: RoomId(1),
        max_idle_input_secs: Some(CEILING),
        afk_action: action,
        max_players: cap,
        ..Default::default()
    };
    create(&tx, config).await;
    tx
}

/// One client: its actor's inbox, outbound frames and metrics samples.
pub struct Client {
    pub inbox: Mailbox<ConnIn>,
    pub out: mpsc::Receiver<FrameBatch>,
    pub metrics: mpsc::Receiver<MetricsEvent>,
    pub actor: tokio::task::JoinHandle<()>,
    /// The entity the join answered with.
    pub entity: EntityId,
}

impl Client {
    /// Connect `conn`, authenticate as `name`, join room 1 — or return
    /// the join's ERROR code.
    pub async fn login(reg: &Mailbox<RegistryMsg>, conn: u64, name: &str) -> Self {
        let (inbox, inbox_rx) = channel::<ConnIn>(4096);
        let (out_tx, out) = channel::<FrameBatch>(4096);
        let (metrics_tx, metrics) = mpsc::channel::<MetricsEvent>(4096);
        // What the accept loop does before it spawns the actor: the
        // registry learns the inbox its verdicts travel on.
        reg.send(RegistryMsg::ConnOpened {
            conn: ConnectionId(conn),
            inbox: inbox.clone(),
            source: None,
        })
        .await
        .expect("registry alive");
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
        let mut c = Self {
            inbox,
            out,
            metrics,
            actor: tokio::spawn(actor.run()),
            entity: 0,
        };
        let auth = base::Auth {
            name: name.to_string(),
            ticket: Vec::new(),
            protocol_version: 0,
        };
        c.send(FrameBody::new(op::base::AUTH_REQ, auth.encode_to_vec()))
            .await;
        c.join().await;
        c
    }

    /// JOIN room 1 and wait for the result.
    pub async fn join(&mut self) {
        let join = base::JoinRoom { room_id: 1 };
        self.send(FrameBody::new(
            op::base::JOIN_ROOM_REQ,
            join.encode_to_vec(),
        ))
        .await;
        let f = self.expect(op::base::JOIN_ROOM_RESULT).await;
        self.entity = base::JoinRoomResult::decode(&f.payload[..])
            .expect("decodes")
            .entity;
    }

    pub async fn send(&self, frame: FrameBody) {
        self.inbox
            .send(ConnIn::Frame(frame))
            .await
            .expect("actor alive");
    }

    /// Wait for a frame with `code`; any ERROR on the way fails the test.
    pub async fn expect(&mut self, code: u16) -> FrameBody {
        loop {
            let batch = tokio::time::timeout(WAIT, self.out.recv()).await;
            for f in batch.expect("a frame in time").expect("out open") {
                assert_ne!(f.op, op::base::ERROR, "no ERROR was expected");
                if f.op == code {
                    return f;
                }
            }
        }
    }

    /// Every frame the client received until the connection closed (the
    /// actor dropped its outbound half), in order.
    pub async fn until_closed(&mut self) -> Vec<FrameBody> {
        let mut all = Vec::new();
        loop {
            match tokio::time::timeout(WAIT, self.out.recv()).await {
                Ok(Some(batch)) => all.extend(batch),
                Ok(None) => return all,
                Err(_) => panic!("the connection was not closed: {all:?}"),
            }
        }
    }

    /// Whatever arrived so far, without waiting.
    pub fn received(&mut self) -> Vec<FrameBody> {
        let mut all = Vec::new();
        while let Ok(batch) = self.out.try_recv() {
            all.extend(batch);
        }
        all
    }

    /// End the session (if the server has not), and return the verdict
    /// its final sample carried.
    pub async fn verdict(self) -> Option<ServerClose> {
        // A peer close (the registry holds the inbox too, so dropping
        // ours would not end the actor); a no-op once it has ended.
        let _ = self
            .inbox
            .send(ConnIn::Closed {
                reason: "test over".into(),
            })
            .await;
        tokio::time::timeout(WAIT, self.actor)
            .await
            .expect("the actor exits")
            .expect("no panic");
        let mut metrics = self.metrics;
        let mut last = None;
        while let Ok(ev) = metrics.try_recv() {
            if let MetricsEvent::Conn(s) = ev
                && s.last
            {
                last = Some(s.server_close);
            }
        }
        last.expect("the final sample")
    }
}
