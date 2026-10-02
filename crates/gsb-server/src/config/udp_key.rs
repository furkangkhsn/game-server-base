//! The rUDP doors' identity (BACKLOG B5a, `docs/RUDP-SECURITY.md`
//! decisions 3 and 6): the static X25519 key of a sealed door, from the
//! config, and the explicit plaintext switch.
//!
//! **Format: 64 hex characters** (32 bytes), inline (`udp_static_key`)
//! or in a file (`udp_static_key_file`, surrounding whitespace ignored).
//! Hex, like `udp_cookie_key`, so the config has one spelling for raw key
//! bytes; any 32 random bytes are a valid X25519 private key
//! (`openssl rand -hex 32`). The public half — what clients pin — is
//! logged at bind and is on [`crate::ServerHandle::udp_public_key`].
//!
//! **Never logged, never echoed:** an error names what is wrong (missing,
//! both set, a length, a position, the file's path), never a character
//! of the key.
//!
//! **The stateless reset key** (B5b, decision 9): `udp_reset_key` /
//! `udp_reset_key_file`, the same spelling — optional: unset, each door
//! derives it from the static key (`ResetKey::derived_from`), which
//! survives restarts already.

use std::sync::Arc;

use gsb_net::seal::{KEY_LEN, ResetKey, StaticKey};
use gsb_net::udp::UdpSecurity;
use tracing::warn;

use crate::{Config, ServerError, UdpSecurityKind};

/// The rUDP doors' security mode from the config: sealed under the
/// configured key (a missing or malformed key refuses startup — never a
/// silent plaintext fallback), or the explicit plaintext switch (warned
/// once).
pub(crate) fn udp_security(cfg: &Config) -> Result<UdpSecurity, ServerError> {
    let bad = ServerError::BadUdpStaticKey;
    if cfg.udp_security == UdpSecurityKind::Plaintext {
        warn!(
            "rUDP: udp_security = \"plaintext\": the rUDP doors neither encrypt nor \
             authenticate anything (dev/LAN only; the production default is \"sealed\")"
        );
        return Ok(UdpSecurity::Plaintext);
    }
    let spelled = (&cfg.udp_static_key, &cfg.udp_static_key_file);
    let Some(private) = read_key("udp_static_key", spelled).map_err(bad)? else {
        return Err(bad(
            "missing: a sealed rUDP door (udp_security = \"sealed\", the default) \
             needs udp_static_key or udp_static_key_file (64 hex characters, e.g. \
             `openssl rand -hex 32`); a dev/LAN door may set udp_security = \
             \"plaintext\" instead"
                .into(),
        ));
    };
    let key = StaticKey::from_private(*private)
        .map_err(|e| bad(format!("the X25519 backend refused it: {e:?}")))?;
    Ok(UdpSecurity::Sealed(Arc::new(key)))
}

/// The sealed rUDP doors' stateless reset key from the config (B5b):
/// `None` — unset, or a plaintext door — lets each door derive it from
/// the static key. Both spellings, an unreadable file or a malformed key
/// refuse startup.
pub(crate) fn udp_reset_key(cfg: &Config) -> Result<Option<Arc<ResetKey>>, ServerError> {
    if cfg.udp_security == UdpSecurityKind::Plaintext {
        return Ok(None);
    }
    let spelled = (&cfg.udp_reset_key, &cfg.udp_reset_key_file);
    let key = read_key("udp_reset_key", spelled).map_err(ServerError::BadUdpResetKey)?;
    Ok(key.map(|k| Arc::new(ResetKey::from_bytes(*k))))
}

/// A 32-byte key spelled inline (`name`) or in a file (`name_file`):
/// `None` when neither is set. The error never carries a key character.
fn read_key(
    name: &str,
    (inline, file): (&Option<String>, &Option<String>),
) -> Result<Option<zeroize::Zeroizing<[u8; KEY_LEN]>>, String> {
    let hex = match (inline, file) {
        (None, None) => return Ok(None),
        (Some(_), Some(_)) => {
            return Err(format!("both {name} and {name}_file are set; keep one"));
        }
        (Some(inline), None) => zeroize::Zeroizing::new(inline.clone()),
        (None, Some(path)) => zeroize::Zeroizing::new(
            std::fs::read_to_string(path).map_err(|e| format!("{name}_file `{path}`: {e}"))?,
        ),
    };
    let key = parse_key(hex.trim()).map_err(|why| format!("malformed: {why}"))?;
    Ok(Some(key))
}

/// 64 hex characters into 32 bytes. The error never carries a character
/// of the input.
fn parse_key(s: &str) -> Result<zeroize::Zeroizing<[u8; KEY_LEN]>, String> {
    if s.len() != KEY_LEN * 2 || !s.is_ascii() {
        return Err(format!(
            "expected {} hex characters, got {} characters",
            KEY_LEN * 2,
            s.chars().count()
        ));
    }
    let mut out = zeroize::Zeroizing::new([0u8; KEY_LEN]);
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16)
            .map_err(|_| format!("a non-hex character at position {}", i * 2))?;
    }
    Ok(out)
}

/// A client's pinned server key (a public key) from its 64 hex
/// characters — the load generator's `--udp-server-key`, a test's.
pub fn parse_udp_public_key(s: &str) -> Result<[u8; KEY_LEN], String> {
    parse_key(s.trim()).map(|k| *k)
}

/// Lowercase hex of a 32-byte key (a public key for a log line or the
/// load generator's `SERVING` line; [`ephemeral_udp_key`]'s private one).
pub fn udp_key_hex(key: &[u8; KEY_LEN]) -> String {
    key.iter().map(|b| format!("{b:02x}")).collect()
}

/// A fresh static key from the OS entropy source, as the config's
/// `udp_static_key` value, with its public half: for tests, load runs and
/// a local dev server — a deployment configures one stable key (its
/// clients pin it). `None` when the entropy source fails.
pub fn ephemeral_udp_key() -> Option<(String, [u8; KEY_LEN])> {
    let key = StaticKey::generate().ok()?;
    Some((udp_key_hex(&key.private_bytes()), key.public()))
}

#[cfg(test)]
mod tests;
