//! The same connection resuming the park its own idle-kick left, while
//! the leave request for it still waits in the room (B40).

use super::*;

/// A logic that parks every disconnect under a ledger, so the same
/// connection can resume its own park.
struct LedgerLogic {
    ledger: HashMap<String, PlayerId>,
}

impl GameLogic<()> for LedgerLogic {
    type GroupKey = ();
    type Strip = ();
    fn snapshot_op(&self) -> u16 {
        0x7510
    }
    fn private_op(&self) -> u16 {
        0x7511
    }
    fn group_of(&self, _w: &(), _p: PlayerId) -> Self::GroupKey {}
    fn snapshot(
        &mut self,
        _w: &mut (),
        _c: &TickCtx,
        _g: &(),
        _b: &[crate::shard::BorderRecord<()>],
        _o: &mut bytes::BytesMut,
    ) -> bool {
        false
    }
    fn on_join(&mut self, _w: &mut (), conn: ConnectionId) -> Admission {
        Admission {
            player: PlayerId(conn.0),
            entity: 1,
        }
    }
    fn on_leave(&mut self, _w: &mut (), _p: PlayerId) {}
    fn ingest(&mut self, _w: &mut (), _c: &TickCtx, a: &mut Vec<Action>) {
        a.clear();
    }
    fn update(&mut self, _w: &mut (), _c: &TickCtx) {}
    fn on_disconnect(&mut self, _w: &mut (), player: PlayerId, identity: &str) -> Detach {
        self.ledger.insert(identity.to_string(), player);
        PARK
    }
    fn resume_lookup(&self, _w: &(), identity: &str) -> ResumeFound {
        match self.ledger.get(identity) {
            Some(&p) => ResumeFound::Held(p),
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
}

impl RoomLogic<()> for LedgerLogic {}

/// The connection RESUMES its own park while the request still waits:
/// the request goes (the row it would move is that connection's live
/// membership again), and the park is the connection's once more.
#[test]
fn a_queued_request_goes_when_the_connection_resumes_its_park() {
    let (_tick_tx, tick_rx) = broadcast::channel(4);
    let (_control, control_rx) = channel(16);
    let mut actor: RoomActor<(), (), ()> = RoomActor::new(
        leave_room(75),
        (),
        Box::new(LedgerLogic {
            ledger: HashMap::new(),
        }),
        tick_rx,
        control_rx,
        1,
        null_metrics_tx(),
        None,
    );
    let (tx, mut reg) = channel(1);
    tx.try_send(RegistryMsg::Shutdown).expect("the one slot");
    actor.registry = Some(tx);
    let resume = |actor: &mut RoomActor<(), (), ()>, epoch: u64| {
        let (out, out_rx) = mpsc::channel::<FrameBatch>(64);
        let (rtx, mut rrx) = oneshot::channel();
        actor.handle_control(RoomControl::Resume {
            conn: ConnectionId(1),
            epoch,
            identity: "ana".into(),
            out,
            reply: rtx,
            claims: None,
        });
        let seat = rrx.try_recv().expect("sync").expect("admitted");
        (seat, out_rx)
    };
    let (_seat, _o1) = resume(&mut actor, 1);
    let t0 = Instant::now();
    actor.step_phases(&TickInfo {
        tick: 1,
        at: t0 + Duration::from_secs(30),
    });
    assert_eq!(actor.leave_requests.len(), 1, "the request waits");
    assert!(actor.conns[&PlayerId(1)].detached);
    let (_seat, _o2) = resume(&mut actor, 2);
    assert!(!actor.conns[&PlayerId(1)].detached, "resumed");
    assert_eq!(actor.conns[&PlayerId(1)].conn, ConnectionId(1));
    assert!(actor.leave_requests.is_empty(), "the resume dropped it");
    assert!(matches!(reg.try_recv(), Ok(RegistryMsg::Shutdown)));
}
