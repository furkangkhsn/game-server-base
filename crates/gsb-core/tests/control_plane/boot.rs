//! A room whose logic did not boot its call table (two request types
//! claim one op — `GameLogic::boot_calls`, SERVICES.md §11) is refused
//! at create with `CoreError::RoomBoot` and never installed (BACKLOG
//! B153); a room whose table boots is created as before.

use super::*;
use gsb_core::calls::{BootError, Call, Calls, Request};

#[derive(Clone, PartialEq, prost::Message)]
struct First {}
#[derive(Clone, PartialEq, prost::Message)]
struct Second {}
impl Call for First {
    type Response = First;
    type Error = First;
}
impl Request for First {
    const OP: u16 = 1900;
}
impl Call for Second {
    type Response = Second;
    type Error = Second;
}
impl Request for Second {
    const OP: u16 = 1900;
}

/// [`ResultLogic`] whose boot claims op 1900 twice when `collide`.
struct Booting {
    inner: ResultLogic,
    collide: bool,
}

impl GameLogic<()> for Booting {
    type GroupKey = ();
    type Strip = ();
    fn snapshot_op(&self) -> u16 {
        self.inner.snapshot_op()
    }
    fn private_op(&self) -> u16 {
        self.inner.private_op()
    }
    fn group_of(&self, _w: &(), _p: PlayerId) -> Self::GroupKey {}
    fn snapshot(
        &mut self,
        w: &mut (),
        c: &TickCtx,
        g: &(),
        borrowed: &[gsb_core::shard::BorderRecord<()>],
        o: &mut bytes::BytesMut,
    ) -> bool {
        self.inner.snapshot(w, c, g, borrowed, o)
    }
    fn on_join(&mut self, w: &mut (), c: ConnectionId) -> Admission {
        self.inner.on_join(w, c)
    }
    fn on_leave(&mut self, w: &mut (), p: PlayerId) {
        self.inner.on_leave(w, p)
    }
    fn ingest(&mut self, w: &mut (), c: &TickCtx, a: &mut Vec<Action>) {
        self.inner.ingest(w, c, a)
    }
    fn update(&mut self, w: &mut (), c: &TickCtx) {
        self.inner.update(w, c)
    }
    fn boot_calls(&mut self, calls: &mut Calls) -> Result<(), BootError> {
        calls.accept::<First>()?;
        if self.collide {
            calls.accept::<Second>()?;
        }
        Ok(())
    }
}

impl RoomLogic<()> for Booting {}

#[tokio::test]
async fn a_room_whose_call_table_does_not_boot_is_refused_at_create() {
    // Room 1 collides; room 2 boots.
    let factory: RoomFactory<(), (), (), ()> = Arc::new(|id: RoomId, _config| BuiltRoom::Single {
        world: (),
        logic: Box::new(Booting {
            inner: ResultLogic { result: None },
            collide: id == RoomId(1),
        }) as Box<dyn RoomLogic<(), GroupKey = (), Strip = ()>>,
    });
    let (tx, _registry) = start(factory, None);

    match create(&tx, RoomId(1)).await {
        Err(CoreError::RoomBoot { room, error }) => {
            assert_eq!(room, 1);
            assert!(matches!(error, BootError::OpCollision { op: 1900, .. }));
        }
        other => panic!("expected RoomBoot, got {other:?}"),
    }
    assert!(
        matches!(status(&tx, RoomId(1)).await, RoomStatus::Absent),
        "the refused room is not installed"
    );
    assert!(matches!(
        create(&tx, RoomId(2)).await,
        Ok(RoomStatus::Running { members: 0 })
    ));
}
