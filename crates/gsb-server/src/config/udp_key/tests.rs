//! The sealed door's key from the config: every way to get it wrong
//! refuses startup, and no error ever carries a character of the key.

use super::*;

fn cfg(inline: Option<&str>, file: Option<&str>) -> Config {
    Config {
        udp_static_key: inline.map(str::to_string),
        udp_static_key_file: file.map(str::to_string),
        ..Config::default()
    }
}

fn refusal(c: &Config) -> String {
    match udp_security(c) {
        Err(ServerError::BadUdpStaticKey(why)) => why,
        Err(e) => panic!("another error: {e}"),
        Ok(s) => panic!("accepted: {s:?}"),
    }
}

/// Sealed is the default, and a sealed door without a key refuses
/// startup — never a plaintext fallback; plaintext must be asked for.
#[test]
fn a_sealed_door_without_a_key_refuses_and_plaintext_is_explicit() {
    assert_eq!(Config::default().udp_security, UdpSecurityKind::Sealed);
    assert!(refusal(&Config::default()).starts_with("missing"));
    let plain = Config {
        udp_security: UdpSecurityKind::Plaintext,
        ..Config::default()
    };
    assert!(matches!(udp_security(&plain), Ok(UdpSecurity::Plaintext)));
}

/// The inline key and the file give the same identity (the file's
/// whitespace ignored); both at once refuse.
#[test]
fn the_key_comes_inline_or_from_a_file_never_both() {
    let (hex, public) = ephemeral_udp_key().expect("entropy");
    let Ok(UdpSecurity::Sealed(k)) = udp_security(&cfg(Some(&hex), None)) else {
        panic!("inline key refused");
    };
    assert_eq!(k.public(), public);
    let path = std::env::temp_dir().join(format!("gsb-udp-key-{}", std::process::id()));
    std::fs::write(&path, format!("  {hex}\n")).unwrap();
    let file = path.to_str().unwrap();
    assert_eq!(
        udp_security(&cfg(None, Some(file))).unwrap().public_key(),
        Some(public)
    );
    assert!(refusal(&cfg(Some(&hex), Some(file))).starts_with("both"));
    let _ = std::fs::remove_file(&path);
    let gone = refusal(&cfg(None, Some("/nonexistent/gsb-udp-key")));
    assert!(gone.contains("/nonexistent/gsb-udp-key"), "{gone}");
}

/// A malformed key refuses with its length or a position — and never
/// echoes a character of it.
#[test]
fn a_malformed_key_refuses_without_echoing_it() {
    let secretish = "c0ffee".repeat(10) + "zz11"; // 64 chars, one non-hex
    for bad in [&secretish[..], &secretish[..63], "ünïcödé"] {
        let why = refusal(&cfg(Some(bad), None));
        assert!(why.starts_with("malformed"), "{why}");
        assert!(!why.contains("c0ffee") && !why.contains("zz"), "{why}");
    }
    assert_eq!(
        parse_udp_public_key(&udp_key_hex(&[7; 32])),
        Ok([7; 32]),
        "the public key round-trips"
    );
}
