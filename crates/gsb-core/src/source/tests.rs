//! The source rule: an IPv4 address, an IPv6 /64, a mapped address as
//! its IPv4 one.

use super::*;

fn src(s: &str) -> Source {
    Source::of(s.parse().expect("an address"))
}

#[test]
fn a_source_is_the_v4_address_or_the_v6_64() {
    assert_eq!(src("10.0.0.1"), src("10.0.0.1"));
    assert_ne!(src("10.0.0.1"), src("10.0.0.2"), "each IPv4 address");
    assert_eq!(
        src("2001:db8:1:2::1"),
        src("2001:db8:1:2:ffff::9"),
        "one /64"
    );
    assert_ne!(
        src("2001:db8:1:2::1"),
        src("2001:db8:1:3::1"),
        "the next /64"
    );
    // A dual-stack socket's IPv4 client is its IPv4 address, not the /64
    // `::` every mapped address shares.
    assert_eq!(src("::ffff:10.0.0.1"), src("10.0.0.1"));
    assert_ne!(src("::ffff:10.0.0.1"), src("::ffff:10.0.0.2"));
    assert_eq!(src("2001:db8:1:2::1").to_string(), "2001:db8:1:2::/64");
    assert_eq!(src("::ffff:10.0.0.1").to_string(), "10.0.0.1");
}
