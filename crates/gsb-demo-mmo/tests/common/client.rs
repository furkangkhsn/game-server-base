//! A test client: keeps its view with the kit's reference client
//! (`gsb_kit::client`, the client rules of `kit.proto`) over the MMO's
//! decode seam — a FULL (group or one-shot private) replaces the view; a
//! DELTA with a baseline applies `removed`, then `cell_exits` (every held
//! record in that ground cell is forgotten), then the upserts; a delta
//! without a baseline is dropped; a duplicate sequence is discarded.
//!
//! Stream invariants are asserted on every batch: at most one snapshot
//! and one private frame per tick, and no frame the view applies lists an
//! entity twice (checked on the MMO's typed mirror of the frame).

use bytes::Bytes;
use gsb_core::PlayerId;
use gsb_core::channel::{FrameBatch, Mailbox};
use gsb_core::id::ConnectionId;
use gsb_core::room::Action;
use gsb_demo_mmo::codec::to_dm;
use gsb_demo_mmo::mmo::{self, CellExit, EntityRecord, Private, WorldSnapshot, private};
use gsb_demo_mmo::op;
use gsb_demo_mmo::world::client_cell;
use gsb_kit::client::{Apply, ClientDecoder, ClientError, ClientView, PrivateEvent};
use prost::Message;
use tokio::sync::mpsc;

/// The MMO's decode seam: a record is kept whole, in its ground cell
/// (`client_cell`); a `CellExit` names the ground cell `(x, z)`.
#[derive(Default)]
pub struct MmoDecoder;

impl ClientDecoder for MmoDecoder {
    type Record = EntityRecord;
    type Cell = (i32, i32);

    fn record(&self, body: &[u8]) -> Result<(u64, (i32, i32), EntityRecord), prost::DecodeError> {
        let r = EntityRecord::decode(body)?;
        Ok((r.entity, client_cell(r.x, r.z), r))
    }

    fn cell_exit(&self, body: &[u8]) -> Result<(i32, i32), prost::DecodeError> {
        let c = CellExit::decode(body)?;
        Ok((c.x, c.z))
    }
}

/// One logged-in character's session.
pub struct Client {
    pub conn: ConnectionId,
    /// The character's wire id (the join reply's value).
    pub id: u64,
    rx: mpsc::Receiver<FrameBatch>,
    actions: Mailbox<Action>,
    /// The current view: wire id → record.
    pub view: ClientView<MmoDecoder>,
    next_input: u64,
    /// Every input ack received, in order.
    pub acks: Vec<u64>,
    /// Every frame received: `(op, payload)`.
    pub raw: Vec<(u16, Bytes)>,
}

impl Client {
    pub(super) fn new(
        conn: ConnectionId,
        id: u64,
        rx: mpsc::Receiver<FrameBatch>,
        actions: Mailbox<Action>,
    ) -> Self {
        Self {
            conn,
            id,
            rx,
            actions,
            view: ClientView::default(),
            next_input: 1,
            acks: Vec::new(),
            raw: Vec::new(),
        }
    }

    /// The record of wire id `id` in the view.
    pub fn get(&self, id: u64) -> Option<&EntityRecord> {
        self.view.get(id)
    }

    /// This client's own record.
    pub fn me(&self) -> Option<&EntityRecord> {
        self.get(self.id)
    }

    /// The wire ids in the view, sorted.
    pub fn sees(&self) -> Vec<u64> {
        let mut ids: Vec<u64> = self.view.ids().collect();
        ids.sort_unstable();
        ids
    }

    /// The records of `kind` in the view, by wire id.
    pub fn of_kind(&self, kind: mmo::Kind) -> Vec<(u64, EntityRecord)> {
        let mut records: Vec<(u64, EntityRecord)> = self
            .view
            .iter()
            .filter(|(_, r)| r.kind == kind as i32)
            .map(|(id, r)| (id, *r))
            .collect();
        records.sort_unstable_by_key(|&(id, _)| id);
        records
    }

    async fn send(&mut self, op: u16, payload: Vec<u8>) {
        self.actions
            .send(Action {
                // The core stamps the stable player id from its binding.
                player: PlayerId(self.conn.0),
                conn: self.conn,
                op,
                payload: Bytes::from(payload),
            })
            .await
            .expect("action channel alive");
    }

    fn seq(&mut self) -> u64 {
        self.next_input += 1;
        self.next_input - 1
    }

    /// Walk toward `(x, z)` metres (a numbered input).
    pub async fn move_to(&mut self, x: f32, z: f32) {
        let seq = self.seq();
        let m = mmo::MoveTo {
            x: to_dm(x),
            z: to_dm(z),
            seq,
        };
        self.send(op::MMO_MOVE_TO, m.encode_to_vec()).await;
    }

    /// Hit the mob with wire id `target`.
    pub async fn attack(&mut self, target: u64) {
        let seq = self.seq();
        let m = mmo::Attack { target, seq };
        self.send(op::MMO_ATTACK, m.encode_to_vec()).await;
    }

    /// Use waystone `waystone`.
    pub async fn travel(&mut self, waystone: u32) {
        let seq = self.seq();
        let m = mmo::Travel { waystone, seq };
        self.send(op::MMO_TRAVEL, m.encode_to_vec()).await;
    }

    /// Decode everything received since the last call.
    pub(super) fn drain(&mut self) {
        while let Ok(batch) = self.rx.try_recv() {
            let snaps = batch.iter().filter(|f| f.op == op::MMO_SNAPSHOT).count();
            let privs = batch.iter().filter(|f| f.op == op::MMO_PRIVATE).count();
            assert!(
                snaps <= 1 && privs <= 1,
                "one snapshot + one private per tick"
            );
            for f in batch.iter() {
                self.raw.push((f.op, f.payload.clone()));
                match f.op {
                    op::MMO_SNAPSHOT => {
                        let s = WorldSnapshot::decode(f.payload.as_ref()).expect("snapshot");
                        let applied = self.view.apply_snapshot(f.payload.as_ref());
                        if let Apply::Full | Apply::Delta = applied.expect("snapshot").apply {
                            assert_unique(&s);
                        }
                    }
                    op::MMO_PRIVATE => {
                        let p = Private::decode(f.payload.as_ref()).expect("private");
                        match self.view.apply_private(f.payload.as_ref()) {
                            Ok(PrivateEvent::Ack(up_to)) => self.acks.push(up_to),
                            Ok(PrivateEvent::Full { .. }) => {
                                if let Some(private::Payload::Snapshot(s)) = &p.payload {
                                    assert_unique(s);
                                }
                            }
                            Ok(PrivateEvent::Empty) => {}
                            Err(ClientError::PrivateDelta) => {
                                panic!("the one-shot private view is a full")
                            }
                            Err(e) => panic!("private: {e}"),
                        }
                    }
                    other => panic!("unexpected frame op {other}"),
                }
            }
        }
    }
}

fn assert_unique(s: &WorldSnapshot) {
    let mut ids: Vec<u64> = s.entities.iter().map(|r| r.entity).collect();
    ids.sort_unstable();
    let n = ids.len();
    ids.dedup();
    assert_eq!(ids.len(), n, "a frame listed an entity twice: {s:?}");
}
