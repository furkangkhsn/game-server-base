//! BACKLOG F61: a `[[listeners]]` entry takes exactly its four keys
//! (`transport`, `bind`, `tls_cert`, `tls_key`). Any other key — a typo,
//! or a server-wide key written inside a door (`listen_backlog`) — stops
//! startup at the config file, naming the key and pointing at the entry's
//! line, instead of being silently dropped. Parse only: nothing binds.

use gsb_server::{Config, ConfigError, ListenerTransport};

/// Load `body` the way the server binary does (`Config::from_file`).
fn load(name: &str, body: &str) -> Result<Config, ConfigError> {
    let path = std::env::temp_dir().join(format!(
        "gsb-listener-keys-{}-{name}.toml",
        std::process::id()
    ));
    std::fs::write(&path, body).expect("write temp config");
    let loaded = Config::from_file(&path);
    let _ = std::fs::remove_file(&path);
    loaded
}

/// Two doors; the SECOND entry carries `line`, on line 8 of the file.
fn two_doors_with(line: &str) -> String {
    format!(
        "tick_hz = 30\n\
         \n\
         [[listeners]]\n\
         transport = \"tcp\"\n\
         bind = \"127.0.0.1:0\"\n\
         \n\
         [[listeners]]\n\
         {line}\n\
         transport = \"ws\"\n\
         bind = \"127.0.0.1:0\"\n"
    )
}

/// The refusal of `line` in the second entry, as the operator reads it.
fn refused(line: &str) -> String {
    let name = line.split(" = ").next().expect("a key");
    match load(name, &two_doors_with(line)) {
        Err(e @ ConfigError::Parse { .. }) => {
            // The binary prints the error and its source chain.
            let source = std::error::Error::source(&e).expect("the parse error");
            format!("{e}\n{source}")
        }
        Err(e) => panic!("{line}: not a parse error: {e}"),
        Ok(cfg) => panic!("{line}: parsed, dropping the key: {:?}", cfg.listeners),
    }
}

/// A server-wide key and a typo inside an entry each stop startup: the
/// error names the key, lists the keys an entry takes, and points at the
/// line inside the entry that carries it.
#[test]
fn an_unknown_key_in_a_listener_entry_stops_startup() {
    for line in [
        "listen_backlog = 4096",
        "tls_crt = \"/etc/gsb/cert.pem\"",
        "bnd = \"127.0.0.1:1\"",
        "max_connections = 5",
        "tick_hz = 60",
    ] {
        let e = refused(line);
        let name = line.split(" = ").next().expect("a key");
        assert!(
            e.contains(&format!("unknown field `{name}`")),
            "{line}: {e}"
        );
        for known in ["transport", "bind", "tls_cert", "tls_key"] {
            assert!(
                e.contains(&format!("`{known}`")),
                "{line} lists {known}: {e}"
            );
        }
        assert!(e.contains("line 8"), "{line}: the entry's line: {e}");
        assert!(e.contains(line), "{line}: the offending line quoted: {e}");
    }
}

/// The four keys an entry takes still parse, in every combination the
/// grammar documents (the TLS pair on a "tls"/"quic" door).
#[test]
fn an_entry_with_only_its_own_keys_parses() {
    let cfg = load(
        "known",
        "[[listeners]]\n\
         transport = \"tls\"\n\
         bind = \"127.0.0.1:0\"\n\
         tls_cert = \"cert.pem\"\n\
         tls_key = \"key.pem\"\n\
         \n\
         [[listeners]]\n\
         transport = \"udp\"\n\
         bind = \"127.0.0.1:0\"\n",
    )
    .expect("parses");
    let entries = cfg.listeners.expect("two doors");
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].transport, ListenerTransport::Tls);
    assert_eq!(entries[0].tls_cert.as_deref(), Some("cert.pem"));
    assert_eq!(entries[0].tls_key.as_deref(), Some("key.pem"));
    assert_eq!(entries[1].transport, ListenerTransport::Udp);
    assert!(entries[1].tls_cert.is_none() && entries[1].tls_key.is_none());
}

/// Written at the top level, `listen_backlog` is the server's key, as
/// before: only its place inside an entry is refused.
#[test]
fn the_server_wide_key_at_the_top_level_still_parses() {
    let cfg = load(
        "top",
        "listen_backlog = 4096\n\
         [[listeners]]\n\
         transport = \"tcp\"\n\
         bind = \"127.0.0.1:0\"\n",
    )
    .expect("parses");
    assert_eq!(cfg.listen_backlog, 4096);
    assert_eq!(cfg.listeners.map(|l| l.len()), Some(1));
}
