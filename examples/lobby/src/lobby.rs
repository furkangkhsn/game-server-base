//! The lobby: the platform the client already trusts (its HTTPS login).
//! It knows the accounts and their characters, mints the ticket and
//! answers with a join grant — here a plain function returning the
//! grant's JSON (what an HTTP endpoint would send; a real lobby is the
//! game's own service, e.g. a Nuxt server — docs/TICKETS.md).

use std::net::SocketAddr;

use gsb_ticket::{Claims, Door, Issuer, IssuerKey, JoinGrant, Transport, TrustedKey};

use crate::game::Loadout;

/// How long a ticket lives: long enough to connect, short enough that a
/// leaked one is soon worthless (the validator's ceiling is 900 s).
pub const TICKET_TTL_SECS: i64 = 120;

/// What the lobby knows about the game server it sends players to.
pub struct Lobby {
    issuer: Issuer,
    audience: String,
    /// The server's rUDP door and its static public key (to pin).
    udp: SocketAddr,
    udp_key: [u8; 32],
    /// The server's plain TCP door.
    tcp: SocketAddr,
    /// The room this lobby's match runs in.
    room: u64,
}

/// Why a login got no grant.
#[derive(Debug, PartialEq, Eq)]
pub enum LoginError {
    UnknownAccount,
}

impl Lobby {
    /// A fresh signing key for a lobby: its public half goes to every
    /// game server's validator before the lobby signs with it.
    pub fn key() -> IssuerKey {
        IssuerKey::generate("lobby-1").expect("OS entropy")
    }

    /// A lobby signing with `key` for `audience`, sending players to the
    /// server's rUDP door `udp` (pinning `udp_key`) or TCP door `tcp`.
    pub fn new(
        key: IssuerKey,
        audience: &str,
        udp: SocketAddr,
        udp_key: [u8; 32],
        tcp: SocketAddr,
    ) -> Self {
        Self {
            issuer: Issuer::new(key),
            audience: audience.into(),
            udp,
            udp_key,
            tcp,
            room: 1,
        }
    }

    /// The public key every game server of this lobby trusts.
    pub fn trusted(&self) -> TrustedKey {
        self.issuer.trusted()
    }

    /// The account's character: who may play, and as what. A real lobby
    /// reads its database; the example has three accounts (`eve` plays a
    /// class the game has not released — the game's own check refuses it).
    pub fn character(account: &str) -> Option<Loadout> {
        let (character, class) = match account {
            "ann" => (101, "mage"),
            "bob" => (102, "warrior"),
            "eve" => (103, "necromancer"),
            _ => return None,
        };
        Some(Loadout {
            character,
            class: class.into(),
        })
    }

    /// Log `account` in at `now` (Unix seconds): the grant's JSON.
    pub fn login(&self, account: &str, now: i64) -> Result<String, LoginError> {
        let loadout = Self::character(account).ok_or(LoginError::UnknownAccount)?;
        let claims = Claims::new(
            account,
            self.room,
            &self.audience,
            now,
            TICKET_TTL_SECS,
            loadout,
        )
        .expect("OS entropy");
        let ticket = self.issuer.mint(&claims).expect("the loadout serializes");
        let grant = JoinGrant {
            doors: vec![
                Door {
                    transport: Transport::Udp,
                    addr: self.udp.to_string(),
                },
                Door {
                    transport: Transport::Tcp,
                    addr: self.tcp.to_string(),
                },
            ],
            udp_server_key: Some(gsb_ticket::keys::hex32(&self.udp_key)),
            ticket,
            room: self.room,
        };
        Ok(grant.to_json())
    }
}
