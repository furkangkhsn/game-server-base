//! What a per-source limit counts: the SOURCE of a peer address (BACKLOG
//! D11, D12). One rule, shared by the doors' per-source handshake cap
//! (`gsb-net`'s handshake intake) and the registry's per-source cap on
//! unauthenticated connections, so both stages of the pre-auth pipeline
//! see the same sources.
//!
//! - An IPv4 address is its own source.
//! - An IPv6 address counts by its /64 — the smallest block a network
//!   hands one subscriber, inside which a host picks addresses at will
//!   (SLAAC, privacy addresses), so a per-/128 count would be no cap at
//!   all.
//! - An IPv4-mapped IPv6 address (a dual-stack socket's IPv4 client) is
//!   its IPv4 address: every one of them shares the /64 `::`, which would
//!   make all IPv4 clients one source.

use std::fmt;
use std::net::{IpAddr, Ipv6Addr};

/// The source of a peer address (see the module docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Source(IpAddr);

impl Source {
    /// The source a peer at `ip` counts against.
    pub fn of(ip: IpAddr) -> Self {
        Self(match ip {
            IpAddr::V4(_) => ip,
            IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
                Some(v4) => IpAddr::V4(v4),
                None => IpAddr::V6(Ipv6Addr::from(u128::from(v6) & (!0u128 << 64))),
            },
        })
    }
}

impl fmt::Display for Source {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            IpAddr::V4(v4) => write!(f, "{v4}"),
            IpAddr::V6(v6) => write!(f, "{v6}/64"),
        }
    }
}

#[cfg(test)]
mod tests;
