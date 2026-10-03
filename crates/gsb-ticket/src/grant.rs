//! The join grant: the lobby's answer to a client that may play — where
//! to connect, which rUDP server key to pin, the ticket to present, the
//! room to join. One small JSON object, so a lobby in any language
//! writes it:
//!
//! ```json
//! { "doors": [ { "transport": "udp", "addr": "203.0.113.7:7777" },
//!              { "transport": "tcp", "addr": "203.0.113.7:7777" } ],
//!   "udp_server_key": "<64 hex: the server's static X25519 public key>",
//!   "ticket": "v4.public.…", "room": 42 }
//! ```
//!
//! The server key rides the grant (`docs/RUDP-SECURITY.md` decision 3):
//! the client pins it from the platform it already trusts (its HTTPS
//! connection to the lobby), never from the game server itself.
//! `gsb-client`'s `grant` feature connects from one.

use serde::{Deserialize, Serialize};

use crate::keys::{KeyError, parse_hex32};

/// A door of the game server, as the grant names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Transport {
    Tcp,
    Tls,
    Ws,
    Quic,
    Udp,
}

/// One door: its transport and its `host:port`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Door {
    pub transport: Transport,
    pub addr: String,
}

/// The lobby's grant. Unknown fields are ignored (a lobby may add its
/// own); `Debug` never shows the ticket (a bearer credential until it
/// expires).
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JoinGrant {
    /// The doors, in the lobby's order of preference.
    pub doors: Vec<Door>,
    /// The rUDP server's static public key, 64 hex characters — required
    /// to use a `udp` door (a sealed door; a grant never sends a client
    /// to a plaintext one).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub udp_server_key: Option<String>,
    /// The ticket to present in AUTH.
    pub ticket: String,
    /// The room the ticket pins: the room to join.
    pub room: u64,
}

impl JoinGrant {
    /// The grant as JSON.
    pub fn to_json(&self) -> String {
        // A grant is strings, numbers and a list: it always serializes.
        serde_json::to_string(self).unwrap_or_default()
    }

    /// A grant from the lobby's JSON.
    pub fn from_json(json: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(json)
    }

    /// The first door of `transport`, if the grant offers one.
    pub fn door(&self, transport: Transport) -> Option<&Door> {
        self.doors.iter().find(|d| d.transport == transport)
    }

    /// The rUDP server key to pin; an error when the grant has none or it
    /// is not 64 hex characters (never echoed).
    pub fn udp_key(&self) -> Result<[u8; 32], KeyError> {
        let hex = self.udp_server_key.as_deref().ok_or_else(|| {
            KeyError("the grant names no udp_server_key (a udp door needs one)".into())
        })?;
        parse_hex32(hex).map(|k| *k)
    }
}

impl std::fmt::Debug for JoinGrant {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JoinGrant")
            .field("doors", &self.doors)
            .field("udp_server_key", &self.udp_server_key)
            .field("ticket", &format_args!("<{} bytes>", self.ticket.len()))
            .field("room", &self.room)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_grant_round_trips_and_hides_its_ticket() {
        let json = r#"{"doors":[{"transport":"udp","addr":"127.0.0.1:7777"},
            {"transport":"tcp","addr":"127.0.0.1:7778"}],
            "udp_server_key":"0101010101010101010101010101010101010101010101010101010101010101",
            "ticket":"v4.public.SECRET","room":3,"lobby_note":"ignored"}"#;
        let g = JoinGrant::from_json(json).expect("a grant");
        assert_eq!(
            g.door(Transport::Tcp).map(|d| d.addr.as_str()),
            Some("127.0.0.1:7778")
        );
        assert_eq!(g.door(Transport::Quic), None);
        assert_eq!(g.udp_key().expect("a key"), [1u8; 32]);
        assert_eq!(JoinGrant::from_json(&g.to_json()).expect("again"), g);
        let shown = format!("{g:?}");
        assert!(!shown.contains("SECRET"), "{shown}");
        let none = JoinGrant {
            udp_server_key: None,
            ..g
        };
        assert!(none.udp_key().is_err());
    }
}
