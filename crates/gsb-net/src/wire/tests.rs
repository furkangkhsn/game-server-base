//! The count beneath a writer: both write paths count what the inner
//! writer took, reads count nothing, and the stamp is the write's time.

use std::io::IoSlice;

use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::*;

#[tokio::test(start_paused = true)]
async fn both_write_paths_count_and_stamp_reads_do_not() {
    let (a, mut b) = tokio::io::duplex(64);
    let count = WireCount::new();
    let born = count.last_at();
    let mut wire = Wire::new(a, count.clone());
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(wire.write(b"abc").await.expect("write"), 3);
    assert_eq!(count.bytes(), 3);
    assert_eq!(count.last_at(), born + Duration::from_secs(2));

    tokio::time::sleep(Duration::from_secs(1)).await;
    let bufs = [IoSlice::new(b"de"), IoSlice::new(b"f")];
    let n = wire.write_vectored(&bufs).await.expect("write_vectored");
    assert!(n > 0);
    assert_eq!(count.bytes(), 3 + n as u64);
    assert_eq!(count.last_at(), born + Duration::from_secs(3));

    b.write_all(b"xyz").await.expect("peer writes");
    let mut got = [0u8; 3];
    wire.read_exact(&mut got).await.expect("read");
    assert_eq!(count.bytes(), 3 + n as u64, "a read is not a write");
}
