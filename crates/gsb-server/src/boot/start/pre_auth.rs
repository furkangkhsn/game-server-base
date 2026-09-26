//! The pre-auth bounds, resolved once at startup (docs/SECURITY.md §4):
//! the unauthenticated-connection cap the registry enforces, and the
//! handshake bound every handshaking door takes from it (BACKLOG B31).

use crate::config::*;

/// Resolve the effective unauthenticated-connection cap ONCE, at startup
/// (see [`Config::max_unauth_conns`] for the semantics): an explicit
/// positive value wins; `Some(0)` disables the cap entirely; omission
/// derives `max(max_connections / 4, 64)` from the total cap — falling
/// back to [`DEFAULT_MAX_CONNECTIONS`] as the formula's base when the
/// total cap itself is unlimited (one derivation, one documented base).
pub(super) fn unauth_cap_of(cfg: &Config) -> Option<u64> {
    match cfg.max_unauth_conns {
        Some(0) => None,
        Some(n) => Some(n),
        None => Some(derived_unauth_cap(cfg)),
    }
}

/// The formula behind an omitted `max_unauth_conns`.
fn derived_unauth_cap(cfg: &Config) -> u64 {
    let base = cfg.max_connections.unwrap_or(DEFAULT_MAX_CONNECTIONS);
    (base / 4).max(MIN_UNAUTH_CONNS)
}

/// The bound on each handshaking door's handshakes in flight (WebSocket,
/// TLS, QUIC): the unauthenticated-connection cap. A handshake in flight
/// is a connection on its way to being an unauthenticated one, so the
/// handshake stage is never cheaper to exhaust than the stage after it
/// (an attacker needs as many held sockets to refuse others at the
/// door as it needs silent sessions to fill the cap), and never holds
/// more than the cap already lets the server hold. A disabled cap
/// (`0`: an external gate bounds sessions) does not unbound the door —
/// the handshakes run before any gate sees them — so it takes the
/// derived default instead.
pub(super) fn handshake_bound_of(cfg: &Config) -> usize {
    let bound = unauth_cap_of(cfg).unwrap_or_else(|| derived_unauth_cap(cfg));
    usize::try_from(bound).unwrap_or(usize::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(max_connections: Option<u64>, max_unauth_conns: Option<u64>) -> Config {
        Config {
            max_connections,
            max_unauth_conns,
            ..Default::default()
        }
    }

    #[test]
    fn the_handshake_bound_is_the_unauth_cap() {
        assert_eq!(handshake_bound_of(&cfg(Some(1000), Some(300))), 300);
        assert_eq!(handshake_bound_of(&cfg(Some(1000), None)), 250);
        assert_eq!(handshake_bound_of(&cfg(Some(100), None)), 64);
        assert_eq!(handshake_bound_of(&cfg(None, None)), 25_000);
        assert_eq!(handshake_bound_of(&Config::default()), 25_000);
    }

    #[test]
    fn a_disabled_unauth_cap_leaves_the_door_bounded() {
        assert_eq!(unauth_cap_of(&cfg(Some(1000), Some(0))), None);
        assert_eq!(handshake_bound_of(&cfg(Some(1000), Some(0))), 250);
        assert_eq!(handshake_bound_of(&cfg(None, Some(0))), 25_000);
    }
}
