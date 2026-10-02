//! BACKLOG F64: a top-level key nobody owns is refused with the place
//! the file wrote it — `path:line` — when the config came from a file
//! (`Config::from_file` records where each top-level key was written);
//! a config built in code names no place, exactly as before.

use gsb_server::{Config, ServerError};

/// A module owning only its own table (`[lines]`): every other
/// non-engine key is refused.
struct Lines;

impl gsb_server::GameModule for Lines {
    fn name(&self) -> &'static str {
        "lines"
    }

    fn configure(&mut self, _raw: &toml::Table, _engine: &Config) -> Result<(), ServerError> {
        Ok(())
    }

    fn register(&self, _table: &mut gsb_protocol::MessageTable) {}

    fn spawn_registry(&self, _parts: gsb_server::RegistryParts) -> gsb_server::RegistryTask {
        unreachable!("configure-only test")
    }

    fn describe(&self) -> String {
        "lines".into()
    }
}

/// Write `body` to a file of its own and load it the way the binary
/// does; returns the config and the path as the error spells it.
fn load(name: &str, body: &str) -> (Config, String) {
    let path = std::env::temp_dir().join(format!(
        "gsb-top-level-key-lines-{}-{name}.toml",
        std::process::id()
    ));
    std::fs::write(&path, body).expect("write temp config");
    let loaded = Config::from_file(&path);
    let _ = std::fs::remove_file(&path);
    let cfg = loaded.unwrap_or_else(|e| panic!("{name}: the file parses: {e}"));
    (cfg, path.display().to_string())
}

/// The refusal of `cfg`, as its message.
fn refused(cfg: &Config) -> String {
    match cfg.check_top_level_keys(&Lines) {
        Err(e @ ServerError::UnknownKey { .. }) => e.to_string(),
        Err(e) => panic!("not an unknown-key refusal: {e}"),
        Ok(()) => panic!("accepted"),
    }
}

/// A flat key: the line it is written on, after comments, blank lines
/// and engine keys.
#[test]
fn a_flat_key_is_refused_at_its_line() {
    let body = "# a server\nbind = \"127.0.0.1:0\"\n\ntick_hz = 30\ntik_hz = 60\n[lines]\nx = 1\n";
    let (cfg, path) = load("flat", body);
    let msg = refused(&cfg);
    assert!(msg.contains(&format!("`tik_hz` at {path}:5")), "{msg}");
    assert!(msg.contains("did you mean `tick_hz`?"), "{msg}");
}

/// A table header (`[metric.otlp]`), an array of tables (`[[listener]]`,
/// written twice: the first one) and a dotted key (`room.2.tick_hz`)
/// are each refused at the line that first writes the key.
#[test]
fn a_table_is_refused_at_the_line_that_first_writes_it() {
    for (name, body, written, line) in [
        (
            "header",
            "tick_hz = 30\n\n[lines]\nx = 1\n\n[metric.otlp]\nendpoint = \"http://127.0.0.1:4318\"\n",
            "`[metric.otlp]`",
            6,
        ),
        (
            "array",
            "[lines]\nx = 1\n[[listener]]\ntransport = \"tcp\"\n[[listener]]\ntransport = \"ws\"\n",
            "`[[listener]]`",
            3,
        ),
        (
            "dotted",
            "tick_hz = 30\nroom.2.tick_hz = 15\n",
            "`[room.2]`",
            2,
        ),
    ] {
        let (cfg, path) = load(name, body);
        let msg = refused(&cfg);
        assert!(
            msg.contains(&format!("{written} at {path}:{line}")),
            "{name}: {msg}"
        );
    }
}

/// A config built in code names no place: the message is the one it
/// always was (no ` at `), and the key is still named.
#[test]
fn a_config_built_in_code_names_no_place() {
    let raw: toml::Table = toml::from_str("tik_hz = 60\n").expect("test toml parses");
    let cfg = Config {
        raw,
        ..Config::default()
    };
    let msg = refused(&cfg);
    assert!(
        msg.starts_with("unknown top-level config key `tik_hz` (did you mean `tick_hz`?): "),
        "{msg}"
    );
}

/// A file's location does not survive a key the code put into `raw`
/// afterwards: that key has no line, and its refusal names none.
#[test]
fn a_key_added_in_code_after_loading_names_no_place() {
    let (mut cfg, path) = load("added", "tick_hz = 30\n");
    cfg.raw.insert("tik_hz".into(), toml::Value::Integer(60));
    let msg = refused(&cfg);
    assert!(!msg.contains(&path), "{msg}");
    assert!(msg.contains("`tik_hz` (did you mean"), "{msg}");
}

/// The start refuses with the place too: the location travels with the
/// config from the loader to the server's first step (which hosts the
/// default game, the demo).
#[cfg(feature = "game-demo")]
#[tokio::test]
async fn the_start_names_the_place() {
    let (cfg, path) = load("start", "bind = \"127.0.0.1:0\"\n\ntik_hz = 60\n");
    match gsb_server::start_server(cfg).await {
        Err(e) => {
            let msg = e.to_string();
            assert!(msg.contains(&format!("`tik_hz` at {path}:3")), "{msg}");
        }
        Ok(handle) => {
            handle.stop().await;
            panic!("started, ignoring the unknown key");
        }
    }
}
