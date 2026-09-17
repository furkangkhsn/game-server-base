//! Per-group snapshot isolation: a group sees its own world and its
//! own private frames, nobody else's.

use super::*;

#[tokio::test]
async fn per_connection_groups_isolate_snapshots_and_private() {
    let (step_tx, mut steps) = mpsc::channel(64);
    let mut room = GLRoom::new(
        RoomConfig {
            id: RoomId(1),
            ..Default::default()
        }, // keep-alive 1 Hz at 30 Hz: never due in this test's window
        GroupLogic {
            player_entity: HashMap::new(),
            next: 0,
            dirty: std::collections::HashSet::new(),
            step_no: 0,
            steps: step_tx,
        },
    );

    let (a_ent, mut a_rx) = room.join(ConnectionId(1)).await;
    let (b_ent, mut b_rx) = room.join(ConnectionId(2)).await;
    let (c_ent, mut c_rx) = room.join(ConnectionId(0x70)).await; // private target

    // Each join dirties exactly its own group (a per-connection
    // grouping: B joining changes nothing for A). So by step 3 each
    // connection's queue holds exactly its own single snapshot — and
    // never another connection's group content.
    wait_steps(&mut steps, 3).await;
    let a_all = drain_all(&mut a_rx).await;
    let b_all = drain_all(&mut b_rx).await;
    let c_all = drain_all(&mut c_rx).await;
    assert_eq!(a_all.len(), 1, "A emitted once (its own join)");
    assert_eq!(b_all.len(), 1, "B emitted once (its own join)");
    assert_eq!(c_all.len(), 1, "C emitted once (its own join)");
    assert_eq!(
        batch_frames(&a_all[0]),
        vec![(0x7010, a_ent.to_le_bytes().to_vec())],
        "A must see exactly its own group's snapshot"
    );
    assert_eq!(
        batch_frames(&b_all[0]),
        vec![(0x7010, b_ent.to_le_bytes().to_vec())],
        "B must see exactly its own group's snapshot"
    );
    assert_eq!(
        batch_frames(&c_all[0]),
        vec![
            (0x7010, c_ent.to_le_bytes().to_vec()),
            (0x7011, u32::MAX.to_le_bytes().to_vec())
        ],
        "the private frame goes only to the designated connection"
    );

    // C leaves: for a per-connection grouping the remaining groups are
    // unchanged, so nothing is re-emitted.
    room.leave(ConnectionId(0x70), c_ent).await;
    wait_steps(&mut steps, 4).await;
    assert!(
        drain_all(&mut a_rx).await.is_empty(),
        "A unchanged ⇒ no batch"
    );
    assert!(
        drain_all(&mut b_rx).await.is_empty(),
        "B unchanged ⇒ no batch"
    );

    // Nothing changed anymore: no snapshot is emitted at all.
    room.step().await;
    room.step().await;
    wait_steps(&mut steps, 6).await;
    assert!(
        drain_all(&mut a_rx).await.is_empty(),
        "no change (and no keep-alive due) ⇒ no batch"
    );
    assert!(
        drain_all(&mut b_rx).await.is_empty(),
        "no change ⇒ no batch"
    );

    room.shutdown().await;
}
