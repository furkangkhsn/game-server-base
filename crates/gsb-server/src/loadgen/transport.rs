//! The transport the clients speak (`--transport tcp|udp|ws`) and how
//! the in-process / served server opens the matching door.
//!
//! TCP and rUDP are the server config's legacy scalar `transport` key,
//! exactly as before this flag grew a third value. WebSocket is not a
//! value of that key (the server keeps `"ws"` an array-only spelling, so
//! an old config never changes meaning): the loadgen's server gets ONE
//! `[[listeners]]` entry of kind `"ws"` on the same bind address.

/// `--transport`: the clients' door, and the in-process / served
/// server's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Transport {
    /// Length-prefixed frames over TCP (TLS too, with `--tls-ca`).
    Tcp,
    /// rUDP: cookie handshake, reliable control band, lossy game band.
    Udp,
    /// WebSocket (RFC 6455 over plain TCP): every binary message carries
    /// exactly one length-prefixed frame (`gsb_net::ws`).
    Ws,
}

impl Transport {
    /// The flag's spelling, or `None`.
    pub(crate) fn parse(s: &str) -> Option<Self> {
        match s {
            "tcp" => Some(Self::Tcp),
            "udp" => Some(Self::Udp),
            "ws" => Some(Self::Ws),
            _ => None,
        }
    }

    /// Open this transport's door on `cfg` (whose `bind` is already the
    /// address to listen on). TCP and rUDP set the scalar key; WebSocket
    /// moves the address into one `"ws"` listener entry and puts the
    /// scalar `bind` back to its default — the listener array takes
    /// precedence anyway, and a legacy key left changed beside it only
    /// makes the server warn that it is ignored.
    pub(crate) fn open_door(self, cfg: &mut gsb_server::Config) {
        match self {
            Self::Tcp => cfg.transport = gsb_server::TransportKind::Tcp,
            Self::Udp => cfg.transport = gsb_server::TransportKind::Udp,
            Self::Ws => {
                let bind = std::mem::replace(&mut cfg.bind, gsb_server::Config::default().bind);
                cfg.listeners = Some(vec![gsb_server::ListenerEntry {
                    transport: gsb_server::ListenerTransport::Ws,
                    bind,
                    tls_cert: None,
                    tls_key: None,
                }]);
            }
        }
    }
}

impl std::fmt::Display for Transport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Tcp => "tcp",
            Self::Udp => "udp",
            Self::Ws => "ws",
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// TCP and rUDP are the scalar key (no listener array: the served
    /// config is what it always was); WebSocket is one `"ws"` entry on
    /// the bind address, the scalar keys left at their defaults.
    #[test]
    fn each_transport_opens_its_door() {
        let base = || gsb_server::Config {
            bind: "127.0.0.1:0".into(),
            ..Default::default()
        };
        for (t, kind) in [
            (Transport::Tcp, gsb_server::TransportKind::Tcp),
            (Transport::Udp, gsb_server::TransportKind::Udp),
        ] {
            let mut cfg = base();
            t.open_door(&mut cfg);
            assert_eq!((cfg.transport, cfg.bind.as_str()), (kind, "127.0.0.1:0"));
            assert!(cfg.listeners.is_none(), "{t}");
        }
        let mut cfg = base();
        Transport::Ws.open_door(&mut cfg);
        let def = gsb_server::Config::default();
        assert_eq!((cfg.transport, &cfg.bind), (def.transport, &def.bind));
        let entries = cfg.listeners.expect("one listener");
        assert_eq!(entries.len(), 1);
        let e = &entries[0];
        assert_eq!(e.transport, gsb_server::ListenerTransport::Ws);
        assert_eq!(e.bind, "127.0.0.1:0");
        assert!(e.tls_cert.is_none() && e.tls_key.is_none());
    }

    /// The spelling round-trips (the orchestrator forwards `Display` to
    /// its children, which parse it back).
    #[test]
    fn the_spelling_round_trips() {
        for t in [Transport::Tcp, Transport::Udp, Transport::Ws] {
            assert_eq!(Transport::parse(&t.to_string()), Some(t));
        }
        assert_eq!(Transport::parse("quic"), None);
    }
}
