//! The RESULT line's path segment: what the rUDP jitter/bottleneck
//! measurement (BACKLOG B104, `scripts/rudp-jitter.sh`) compares between
//! `--udp-congestion off` and `pace` beside the `transport_*` counters —
//! the connect latency, the clients' snapshot rate — and the rUDP
//! sessions the clients declared over, by reason (B128). Every key is
//! always present.

use super::*;

/// ` connect_p50_ms=… connect_p99_ms=… snap_per_s=… udp_ends_*=…
/// udp_congestion=…` (each key preceded by a space).
pub(crate) fn path_segment(
    connect_ms: (u128, u128),
    snaps_total: u64,
    dur_secs: f64,
    ends: &UdpEnds,
    congestion: Option<gsb_server::UdpCongestionKind>,
) -> String {
    let mode = match congestion {
        Some(gsb_server::UdpCongestionKind::Off) => "off",
        Some(gsb_server::UdpCongestionKind::Pace) => "pace",
        // The server's own config decides (its default, off; an external
        // server: whatever it was started with).
        None => "default",
    };
    format!(
        " connect_p50_ms={} connect_p99_ms={} snap_per_s={:.1}{} udp_congestion={mode}",
        connect_ms.0,
        connect_ms.1,
        snaps_total as f64 / dur_secs.max(1e-9),
        ends.keys(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_segment_names_every_key() {
        let s = path_segment(
            (3, 41),
            900,
            3.0,
            &UdpEnds::from_values([1, 0, 2]),
            Some(gsb_server::UdpCongestionKind::Pace),
        );
        assert_eq!(
            s,
            " connect_p50_ms=3 connect_p99_ms=41 snap_per_s=300.0 udp_ends_rel_dead=1 \
             udp_ends_reset=0 udp_ends_seal_limit=2 udp_congestion=pace"
        );
        assert!(path_segment((0, 0), 0, 0.0, &UdpEnds::default(), None).ends_with("=default"));
    }
}
