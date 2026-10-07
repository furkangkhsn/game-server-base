//! The idle cost (SERVICES.md §3, spike `idle_cost`): a quiet room with
//! a table that claims requests allocates none of the table's per-tick
//! buffers and moves no counter. The proof is structural (counting
//! allocations needs `unsafe`).

use super::*;

#[test]
fn a_quiet_room_never_grows_the_tables_buffers() {
    let mut rig = Rig::new(RigCfg::default());
    let _a = rig.join(1, "alice");
    for _ in 0..1000 {
        rig.step();
    }
    assert_eq!(rig.room.calls.buffer_capacities(), [0; 5]);
    assert_eq!(rig.counts(), crate::metrics::CallCounts::default());
    assert_eq!(rig.room.calls.pending_len(), 0);
    assert!(rig.room.queued.is_empty());
}
