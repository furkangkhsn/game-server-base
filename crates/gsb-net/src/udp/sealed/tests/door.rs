//! The sealed door's configuration (B5b): its reset key is the
//! configured one or the static key's derivation, bound to the door's
//! address — the same door after a restart answers with the same tokens,
//! another door (or another key) never does; the reset budget and the
//! key-phase policy come from the transport config.

use super::*;
use crate::seal::ResetKey;

fn door(key: &Arc<StaticKey>, cfg: &UdpTransportConfig, at: &str) -> DoorSeal {
    DoorSeal::new(key.clone(), None).configure(cfg, at.parse().unwrap())
}

#[test]
fn the_reset_key_is_bound_to_the_door_and_survives_a_restart() {
    let key = Arc::new(StaticKey::generate().unwrap());
    let cfg = UdpTransportConfig::default();
    let a = door(&key, &cfg, "0.0.0.0:7000");
    let restarted = door(&key, &cfg, "0.0.0.0:7000");
    assert_eq!(a.reset.token(42), restarted.reset.token(42));
    let other_door = door(&key, &cfg, "0.0.0.0:7001");
    assert_ne!(a.reset.token(42), other_door.reset.token(42));
    let other_key = door(
        &Arc::new(StaticKey::generate().unwrap()),
        &cfg,
        "0.0.0.0:7000",
    );
    assert_ne!(a.reset.token(42), other_key.reset.token(42));
    // A configured reset key replaces the derivation (and is bound too).
    let configured = UdpTransportConfig {
        reset_key: Some(Arc::new(ResetKey::from_bytes([4; 32]))),
        ..UdpTransportConfig::default()
    };
    let c = door(&key, &configured, "0.0.0.0:7000");
    assert_ne!(c.reset.token(42), a.reset.token(42));
    let rotated_static = door(
        &Arc::new(StaticKey::generate().unwrap()),
        &configured,
        "0.0.0.0:7000",
    );
    assert_eq!(
        c.reset.token(42),
        rotated_static.reset.token(42),
        "the configured key does not depend on the static key"
    );
    assert_eq!(
        c.reset.token(42),
        ResetKey::from_bytes([4; 32])
            .for_door(&crate::udp::path::encode_addr(
                "0.0.0.0:7000".parse().unwrap()
            ))
            .token(42)
    );
}

#[test]
fn the_reset_budget_and_the_rekey_policy_come_from_the_config() {
    let key = Arc::new(StaticKey::generate().unwrap());
    let def = door(&key, &UdpTransportConfig::default(), "127.0.0.1:1");
    assert_eq!(
        def.resets.as_ref().map(|b| b.burst()),
        Some(u64::from(DEFAULT_STATELESS_RESETS_PER_SEC / budget::BURST))
    );
    assert_eq!(def.rekey, RekeyPolicy::default());
    let policy = RekeyPolicy {
        after: Duration::from_secs(5),
        after_records: 4096,
    };
    let off = UdpTransportConfig {
        stateless_resets_per_sec: 0,
        rekey: policy,
        ..UdpTransportConfig::default()
    };
    let d = door(&key, &off, "127.0.0.1:1");
    assert!(d.resets.is_none(), "0 = no resets");
    assert_eq!(d.rekey, policy);
    assert_eq!(
        (DEFAULT_REKEY_AFTER, DEFAULT_REKEY_AFTER_RECORDS),
        (Duration::from_secs(120), 1 << 20)
    );
}
