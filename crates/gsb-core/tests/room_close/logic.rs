//! The test logic: a room (or shard) whose disconnect policy is one knob,
//! which reports every `on_disconnect` it runs and keeps a park ledger so
//! a parked identity can resume.

use super::*;

/// Wire ids per shard (`index * SHARD_SPAN + conn`).
const SHARD_SPAN: u64 = 1000;

/// Every `on_disconnect` the logic ran: `(player, identity)`.
pub type Disconnects = mpsc::UnboundedSender<(PlayerId, String)>;

pub struct AfkLogic {
    /// What `on_disconnect` answers.
    pub decision: Detach,
    pub disc: Disconnects,
    /// Shard index (0 for a single room).
    pub index: usize,
    /// Parked identities (the ledger lives in the logic, §4).
    ledger: HashMap<String, PlayerId>,
    live: HashMap<PlayerId, EntityId>,
}

impl AfkLogic {
    pub fn new(decision: Detach, disc: Disconnects, index: usize) -> Self {
        Self {
            decision,
            disc,
            index,
            ledger: HashMap::new(),
            live: HashMap::new(),
        }
    }
}

impl GameLogic<()> for AfkLogic {
    type GroupKey = ();
    type Strip = ();
    fn snapshot_op(&self) -> u16 {
        0x7E60
    }
    fn private_op(&self) -> u16 {
        0x7E61
    }
    fn group_of(&self, _w: &(), _p: PlayerId) -> Self::GroupKey {}
    fn snapshot(
        &mut self,
        _w: &mut (),
        _c: &TickCtx,
        _g: &(),
        _b: &[BorderRecord<()>],
        _o: &mut bytes::BytesMut,
    ) -> bool {
        false
    }
    fn on_join(&mut self, _w: &mut (), c: ConnectionId) -> Admission {
        // The conn id doubles as the player id.
        let player = PlayerId(c.0);
        let entity = self.index as u64 * SHARD_SPAN + c.0;
        self.live.insert(player, entity);
        Admission { player, entity }
    }
    fn on_leave(&mut self, _w: &mut (), player: PlayerId) {
        self.live.remove(&player);
        self.ledger.retain(|_, p| *p != player);
    }
    fn on_disconnect(&mut self, _w: &mut (), player: PlayerId, identity: &str) -> Detach {
        let _ = self.disc.send((player, identity.to_string()));
        if matches!(self.decision, Detach::Hold { .. }) && !identity.is_empty() {
            self.ledger.insert(identity.to_string(), player);
        }
        self.decision
    }
    fn resume_lookup(&self, _w: &(), identity: &str) -> ResumeFound {
        match self.ledger.get(identity) {
            Some(p) => ResumeFound::Held(*p),
            None => ResumeFound::Never,
        }
    }
    fn on_resume(
        &mut self,
        _w: &mut (),
        identity: &str,
        _c: ConnectionId,
        _p: PlayerId,
        _e: EntityId,
    ) {
        self.ledger.remove(identity);
    }
    fn ingest(&mut self, _w: &mut (), _c: &TickCtx, a: &mut Vec<Action>) {
        a.clear();
    }
    fn update(&mut self, _w: &mut (), _c: &TickCtx) {}
}

impl RoomLogic<()> for AfkLogic {}

// Driven as a shard: nothing migrates or borders — the sharded cases are
// about the close verb, not the seam.
impl ShardLogic<()> for AfkLogic {
    type State = ();
    fn index(&self) -> usize {
        self.index
    }
    fn shard_count(&self) -> usize {
        2
    }
    fn serial_capacity(&self) -> u64 {
        SHARD_SPAN
    }
    fn serial_used(&self) -> u64 {
        self.live.len() as u64
    }
    fn neighbors(&self) -> &[usize] {
        &[]
    }
    fn collect_migrations(&mut self, _w: &mut (), _nb: usize) -> Vec<Migrating<()>> {
        Vec::new()
    }
    fn on_migrate_in(&mut self, _w: &mut (), _wire: u64, _s: (), _p: Option<PlayerId>) {}
    fn on_migrate_out(&mut self, _w: &mut (), _wire: u64) {}
    fn collect_border(&self, _w: &()) -> Vec<BorderRecord<()>> {
        Vec::new()
    }
    fn own_wires(&self, _w: &()) -> Vec<u64> {
        self.live.values().copied().collect()
    }
}

/// A factory of `AfkLogic` rooms: one room, or two shards homing every
/// join to shard 0.
pub fn factory(decision: Detach, sharded: bool, disc: Disconnects) -> RoomFactory<(), (), (), ()> {
    Arc::new(move |_id, _cfg| {
        if sharded {
            let shard = |i: usize| {
                (
                    (),
                    Box::new(AfkLogic::new(decision, disc.clone(), i))
                        as Box<dyn ShardLogic<(), GroupKey = (), State = (), Strip = ()>>,
                )
            };
            BuiltRoom::Sharded {
                shards: vec![shard(0), shard(1)],
                home_shard: Arc::new(|_c, _i: &str| 0),
            }
        } else {
            BuiltRoom::Single {
                world: (),
                logic: Box::new(AfkLogic::new(decision, disc.clone(), 0)),
            }
        }
    })
}
