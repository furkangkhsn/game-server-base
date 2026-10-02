//! The per-source cap's rules, door-independent (D11): what a source is,
//! the cap refuses and counts only its own source, a slot given back —
//! taken, failed, timed out — is the source's again, and the table holds
//! only sources with a slot.

use std::io;
use std::net::{Ipv4Addr, SocketAddr};
use std::time::Duration;

use super::*;
use crate::transport::Endpoint;
use crate::transport::intake::tests::until;

const LONG: Duration = Duration::from_secs(60);

fn ip(s: &str) -> IpAddr {
    s.parse().expect("an address")
}

fn take(intake: &Arc<Intake>, table: &mut SourceTable, from: &str) -> Slot {
    match intake.admit(table, ip(from), true) {
        Admit::Slot(slot) => slot,
        _ => panic!("{from} admitted"),
    }
}

fn over(intake: &Arc<Intake>, table: &mut SourceTable, from: &str, proven: bool) -> bool {
    matches!(intake.admit(table, ip(from), proven), Admit::OverSource { unproven } if unproven != proven)
}

#[test]
fn a_source_is_the_v4_address_or_the_v6_64() {
    let key = |s: &str| SourceKey::of(ip(s), true);
    assert_eq!(key("10.0.0.1"), key("10.0.0.1"));
    assert_ne!(key("10.0.0.1"), key("10.0.0.2"), "each IPv4 address");
    assert_eq!(
        key("2001:db8:1:2::1"),
        key("2001:db8:1:2:ffff::9"),
        "one /64"
    );
    assert_ne!(
        key("2001:db8:1:2::1"),
        key("2001:db8:1:3::1"),
        "the next /64"
    );
    // A dual-stack socket's IPv4 client is its IPv4 address, not the /64
    // `::` every mapped address shares.
    assert_eq!(key("::ffff:10.0.0.1"), key("10.0.0.1"));
    assert_ne!(key("::ffff:10.0.0.1"), key("::ffff:10.0.0.2"));
    assert_ne!(
        SourceKey::of(ip("10.0.0.1"), false),
        key("10.0.0.1"),
        "unproven apart"
    );
    assert_eq!(key("2001:db8:1:2::1").to_string(), "2001:db8:1:2::/64");
    assert_eq!(
        SourceKey::of(ip("10.0.0.1"), false).to_string(),
        "10.0.0.1 (unproven)"
    );
}

#[tokio::test]
async fn over_the_cap_its_source_is_refused_and_counted_others_are_not() {
    let intake = Intake::with_source_cap("test", 8, Some(2));
    let mut table = SourceTable::new(&intake);
    let _a1 = take(&intake, &mut table, "10.0.0.1");
    let _a2 = take(&intake, &mut table, "10.0.0.1");
    assert!(over(&intake, &mut table, "10.0.0.1", true), "the third");
    intake.count_source_refusal();
    assert!(over(&intake, &mut table, "::ffff:10.0.0.1", true), "mapped");
    intake.count_source_refusal();
    let _b = take(&intake, &mut table, "10.0.0.2");
    let _c = take(&intake, &mut table, "2001:db8::1");
    let _c2 = take(&intake, &mut table, "2001:db8::2");
    assert!(
        over(&intake, &mut table, "2001:db8::ffff", true),
        "same /64"
    );
    let _d = take(&intake, &mut table, "2001:db8:0:1::1");
    let s = intake.stats();
    assert_eq!((s.in_flight, s.refused, s.refused_per_source), (6, 0, 2));
}

#[tokio::test]
async fn an_unproven_source_is_counted_apart_and_offered_a_retry() {
    let intake = Intake::with_source_cap("test", 8, Some(1));
    let mut table = SourceTable::new(&intake);
    let spoofed = match intake.admit(&mut table, ip("10.0.0.1"), false) {
        Admit::Slot(slot) => slot,
        _ => panic!("admitted"),
    };
    assert!(
        over(&intake, &mut table, "10.0.0.1", false),
        "unproven, over"
    );
    // The address's real owner, proven, is not refused for a spoofer.
    let _owner = take(&intake, &mut table, "10.0.0.1");
    assert!(over(&intake, &mut table, "10.0.0.1", true), "proven, over");
    drop(spoofed);
    assert!(matches!(
        intake.admit(&mut table, ip("10.0.0.1"), false),
        Admit::Slot(_)
    ));
}

#[tokio::test]
async fn a_slot_given_back_is_its_sources_again() {
    let intake = Intake::with_source_cap("test", 8, Some(1));
    let mut table = SourceTable::new(&intake);
    // Dropped (a refusal by the handshake, the close's drain).
    drop(take(&intake, &mut table, "10.0.0.1"));
    // Taken by the accept loop.
    let slot = take(&intake, &mut table, "10.0.0.1");
    let peer = SocketAddr::from((Ipv4Addr::new(10, 0, 0, 1), 1));
    let done = async move { Ok(Endpoint::new(|_, _, _, _| unreachable!()).with_peer(peer)) };
    intake.spawn(slot, peer, LONG, done);
    assert!(over(&intake, &mut table, "10.0.0.1", true), "queued, held");
    Arc::clone(&intake).next().await.expect("the endpoint");
    // Cut at the deadline.
    let slot = take(&intake, &mut table, "10.0.0.1");
    intake.spawn(
        slot,
        peer,
        Duration::from_millis(20),
        std::future::pending::<io::Result<Endpoint>>(),
    );
    until("timed out", || intake.stats().timed_out == 1).await;
    // Failed.
    let slot = take(&intake, &mut table, "10.0.0.1");
    intake.spawn(slot, peer, LONG, async { Err(io::Error::other("bad")) });
    until("failed", || intake.stats().failed == 1).await;
    let _again = take(&intake, &mut table, "10.0.0.1");
    assert_eq!(intake.stats().refused_per_source, 0);
}

#[tokio::test]
async fn the_table_holds_only_sources_with_a_slot_and_never_more_than_the_bound() {
    let intake = Intake::with_source_cap("test", 4, Some(1));
    let mut table = SourceTable::new(&intake);
    let mut held = Vec::new();
    for n in 0..100u32 {
        match intake.admit(
            &mut table,
            IpAddr::V4(Ipv4Addr::from(0x0a00_0000 + n)),
            true,
        ) {
            Admit::Slot(slot) => held.push(slot),
            Admit::Refused => {}
            Admit::OverSource { .. } => panic!("each source once"),
        }
    }
    assert_eq!(held.len(), 4, "the door's bound");
    assert_eq!(intake.stats().refused, 96);
    assert_eq!(table.sources(), 4);
    held.clear();
    assert_eq!(table.sources(), 0, "every entry goes with its last slot");
}

#[tokio::test]
async fn without_a_cap_one_source_may_take_the_whole_bound() {
    for cap in [None, Some(0)] {
        let intake = Intake::with_source_cap("test", 3, cap);
        let mut table = SourceTable::new(&intake);
        let _held: Vec<Slot> = (0..3)
            .map(|_| take(&intake, &mut table, "10.0.0.1"))
            .collect();
        assert!(matches!(
            intake.admit(&mut table, ip("10.0.0.1"), true),
            Admit::Refused
        ));
        assert_eq!(table.sources(), 0, "no table without a cap");
    }
}
