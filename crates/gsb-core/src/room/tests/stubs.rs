//! The room-logic stubs and the harness the suites share: a recording
//! logic, a grouped logic, and the two rigs that drive a real actor
//! off a manual ticker feed.

use super::*;

mod warn;

mod harness;
pub(in crate::room::tests) use harness::*;
pub(in crate::room::tests) use warn::*;

pub(super) struct RecLogic {
    pub(super) dts: mpsc::Sender<Duration>,
    pub(super) ops: mpsc::Sender<u16>,
}

impl GameLogic<()> for RecLogic {
    type GroupKey = ();
    type Strip = ();

    fn snapshot_op(&self) -> u16 {
        0x7000
    }
    fn private_op(&self) -> u16 {
        0x7001
    }

    fn group_of(&self, _w: &(), _p: PlayerId) -> Self::GroupKey {
        Default::default()
    }

    fn snapshot(
        &mut self,
        _w: &mut (),
        _c: &TickCtx,
        _g: &Self::GroupKey,
        _borrowed: &[crate::shard::BorderRecord<()>],
        _o: &mut bytes::BytesMut,
    ) -> bool {
        false
    }

    fn on_join(&mut self, _w: &mut (), c: ConnectionId) -> Admission {
        // Test identity policy: the conn id doubles as the player id.
        Admission {
            player: PlayerId(c.0),
            entity: 1,
        }
    }
    fn on_leave(&mut self, _w: &mut (), _p: PlayerId) {}
    fn ingest(&mut self, _w: &mut (), _c: &TickCtx, actions: &mut Vec<Action>) {
        for a in actions.drain(..) {
            let _ = self.ops.try_send(a.op);
        }
    }
    fn update(&mut self, _w: &mut (), ctx: &TickCtx) {
        let _ = self.dts.try_send(ctx.dt);
    }
}

// Faz 1 trait split: shared contract on `GameLogic`; no room-exclusive
// hook used (empty `RoomLogic` impl).
impl RoomLogic<()> for RecLogic {}

pub(super) struct GroupLogic {
    pub(super) player_entity: HashMap<PlayerId, u64>,
    pub(super) next: u64,
    pub(super) dirty: std::collections::HashSet<PlayerId>,
    pub(super) step_no: u64,
    pub(super) steps: mpsc::Sender<u64>,
}

impl GameLogic<()> for GroupLogic {
    type GroupKey = PlayerId;
    type Strip = ();

    fn snapshot_op(&self) -> u16 {
        0x7010
    }
    fn private_op(&self) -> u16 {
        0x7011
    }

    fn group_of(&self, _w: &(), player: PlayerId) -> Self::GroupKey {
        player
    }

    fn snapshot(
        &mut self,
        _w: &mut (),
        _c: &TickCtx,
        group: &Self::GroupKey,
        _borrowed: &[crate::shard::BorderRecord<()>],
        out: &mut bytes::BytesMut,
    ) -> bool {
        if !self.dirty.remove(group) {
            return false; // unchanged since the last emission
        }
        let entity = self.player_entity.get(group).copied().unwrap_or(0);
        out.extend_from_slice(&entity.to_le_bytes());
        true
    }

    fn private(
        &mut self,
        _w: &mut (),
        player: PlayerId,
        _group: &PlayerId,
        _responses: &[crate::rpc::RpcReply],
        out: &mut bytes::BytesMut,
    ) -> bool {
        if player == PlayerId(0x70) {
            out.extend_from_slice(&u32::MAX.to_le_bytes());
            true
        } else {
            false
        }
    }

    fn on_join(&mut self, _w: &mut (), conn: ConnectionId) -> Admission {
        self.next += 1;
        // Test identity policy: the conn id doubles as the player id
        // (and therefore as this logic's per-player group key).
        let player = PlayerId(conn.0);
        self.player_entity.insert(player, self.next);
        self.dirty.insert(player); // membership changed (this group)
        Admission {
            player,
            entity: self.next,
        }
    }
    fn on_leave(&mut self, _w: &mut (), player: PlayerId) {
        self.player_entity.remove(&player);
        // The leaver's group is gone (and clean); the remaining groups
        // are unchanged for a per-player grouping.
        self.dirty.remove(&player);
    }
    fn ingest(&mut self, _w: &mut (), _c: &TickCtx, actions: &mut Vec<Action>) {
        actions.clear();
    }
    fn update(&mut self, _w: &mut (), _c: &TickCtx) {
        self.step_no += 1;
        let _ = self.steps.try_send(self.step_no);
    }
}

// Faz 1 trait split: shared contract on `GameLogic`; no room-exclusive
// hook used (empty `RoomLogic` impl).
impl RoomLogic<()> for GroupLogic {}

/// Manual-ticker harness for `GroupLogic` rooms.
pub(super) struct GLRoom {
    pub(super) tick_tx: broadcast::Sender<TickInfo>,
    pub(super) control: Mailbox<RoomControl>,
    pub(super) handle: tokio::task::JoinHandle<()>,
    t0: Instant,
    pub(super) next_tick: u64,
}

impl GLRoom {
    pub(super) fn new(config: RoomConfig, logic: GroupLogic) -> Self {
        let (tick_tx, tick_rx) = broadcast::channel(64);
        let (control, control_rx) = channel(config.control_capacity);
        let actor = RoomActor::new(
            config,
            (),
            Box::new(logic),
            tick_rx,
            control_rx,
            1,
            null_metrics_tx(),
            None,
        );
        Self {
            tick_tx,
            control,
            handle: tokio::spawn(actor.run()),
            t0: Instant::now(),
            next_tick: 0,
        }
    }

    pub(super) fn tick(&mut self) {
        self.next_tick += 1;
        let at = self.t0 + Duration::from_secs_f64(self.next_tick as f64 / 30.0);
        self.tick_tx
            .send(TickInfo {
                tick: self.next_tick,
                at,
            })
            .expect("room subscriber alive");
    }

    pub(super) async fn join(
        &mut self,
        conn: ConnectionId,
    ) -> (EntityId, mpsc::Receiver<FrameBatch>) {
        let (out_tx, out_rx) = mpsc::channel::<FrameBatch>(64);
        let (reply_tx, reply_rx) =
            oneshot::channel::<Result<(EntityId, Mailbox<Action>), CoreError>>();
        self.control
            .send(RoomControl::Join {
                conn,
                out: out_tx,
                reply: reply_tx,
            })
            .await
            .expect("control alive");
        self.tick();
        let (entity, _actions) = tokio::time::timeout(Duration::from_secs(2), reply_rx)
            .await
            .expect("timed out waiting for join reply")
            .expect("join reply dropped")
            .expect("join accepted (room not full)");
        (entity, out_rx)
    }

    pub(super) async fn leave(&mut self, conn: ConnectionId, entity: EntityId) {
        self.control
            .send(RoomControl::Leave { conn, entity })
            .await
            .expect("control alive");
        self.tick();
    }

    /// Feed one tick and wait until the room has stepped it (step
    /// counter from the logic).
    pub(super) async fn step(&mut self) {
        self.tick();
    }

    pub(super) async fn shutdown(mut self) {
        self.control
            .send(RoomControl::Shutdown)
            .await
            .expect("control alive");
        self.tick();
        tokio::time::timeout(Duration::from_secs(2), &mut self.handle)
            .await
            .expect("room did not shut down")
            .expect("room task panicked");
    }
}
