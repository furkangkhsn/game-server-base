//! The demux's session table (BACKLOG B3): sessions under an internal
//! key, with two indices onto it — the client's current address and the
//! session's connection id. A migration moves one index entry; the
//! deadline set and the reap queue name the key, so neither has to
//! follow an address. A child of [`super`], so the table stays private to
//! the demux's module tree.

use std::collections::HashMap;
use std::net::SocketAddr;

use super::UdpSession;

/// A session's internal name in the demux: never reused, never on the
/// wire, independent of the address (the deadline set, the reap queue
/// and the indices name a session by it).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(in crate::udp) struct SessionKey(pub(in crate::udp) u64);

/// The sessions and their two indices (see the module docs).
#[derive(Debug, Default)]
pub(in crate::udp) struct Sessions {
    next: u64,
    by_key: HashMap<SessionKey, UdpSession>,
    by_addr: HashMap<SocketAddr, SessionKey>,
    by_cid: HashMap<u64, SessionKey>,
}

impl Sessions {
    /// Add a session under a fresh key, indexed by its address and (if it
    /// has one) its CID. The caller made sure neither is taken.
    pub(in crate::udp) fn insert(&mut self, s: UdpSession) -> SessionKey {
        self.next += 1;
        let key = SessionKey(self.next);
        self.by_addr.insert(s.addr, key);
        if let Some(cid) = s.cid {
            self.by_cid.insert(cid, key);
        }
        self.by_key.insert(key, s);
        key
    }

    /// The session at `addr`, by key.
    pub(in crate::udp) fn key_at(&self, addr: &SocketAddr) -> Option<SessionKey> {
        self.by_addr.get(addr).copied()
    }

    /// The session named by `cid`, by key.
    pub(in crate::udp) fn key_of_cid(&self, cid: u64) -> Option<SessionKey> {
        self.by_cid.get(&cid).copied()
    }

    /// Whether some session already carries `cid`.
    pub(in crate::udp) fn cid_taken(&self, cid: u64) -> bool {
        self.by_cid.contains_key(&cid)
    }

    pub(in crate::udp) fn get(&self, key: SessionKey) -> Option<&UdpSession> {
        self.by_key.get(&key)
    }

    pub(in crate::udp) fn get_mut(&mut self, key: SessionKey) -> Option<&mut UdpSession> {
        self.by_key.get_mut(&key)
    }

    /// Remove a session and both its index entries.
    pub(in crate::udp) fn remove(&mut self, key: SessionKey) -> Option<UdpSession> {
        let s = self.by_key.remove(&key)?;
        if self.by_addr.get(&s.addr) == Some(&key) {
            self.by_addr.remove(&s.addr);
        }
        if let Some(cid) = s.cid {
            self.by_cid.remove(&cid);
        }
        Some(s)
    }

    /// Move a session to `addr` (a validated path). Refused (`false`)
    /// when another session is at `addr`: one address, one session.
    pub(in crate::udp) fn move_to(&mut self, key: SessionKey, addr: SocketAddr) -> bool {
        if self.by_addr.get(&addr).is_some_and(|k| *k != key) {
            return false;
        }
        let Some(s) = self.by_key.get_mut(&key) else {
            return false;
        };
        if self.by_addr.get(&s.addr) == Some(&key) {
            self.by_addr.remove(&s.addr);
        }
        s.addr = addr;
        self.by_addr.insert(addr, key);
        true
    }

    /// Whether `addr` is another session's (not `key`'s).
    pub(in crate::udp) fn addr_taken_by_other(&self, addr: &SocketAddr, key: SessionKey) -> bool {
        self.by_addr.get(addr).is_some_and(|k| *k != key)
    }

    #[cfg(test)]
    pub(in crate::udp) fn len(&self) -> usize {
        self.by_key.len()
    }

    #[cfg(test)]
    pub(in crate::udp) fn is_empty(&self) -> bool {
        self.by_key.is_empty()
    }

    /// The session at `addr` (by address: the tests' and the handshake's
    /// view).
    pub(in crate::udp) fn at(&self, addr: &SocketAddr) -> Option<&UdpSession> {
        self.key_at(addr).and_then(|k| self.by_key.get(&k))
    }

    #[cfg(test)]
    /// [`Self::at`], mutable.
    pub(in crate::udp) fn at_mut(&mut self, addr: &SocketAddr) -> Option<&mut UdpSession> {
        let k = self.key_at(addr)?;
        self.by_key.get_mut(&k)
    }

    #[cfg(test)]
    /// Whether a session is at `addr`.
    pub(in crate::udp) fn contains_key(&self, addr: &SocketAddr) -> bool {
        self.by_addr.contains_key(addr)
    }
}

impl std::ops::Index<&SocketAddr> for Sessions {
    type Output = UdpSession;

    fn index(&self, addr: &SocketAddr) -> &UdpSession {
        self.at(addr).expect("a session at this address")
    }
}
