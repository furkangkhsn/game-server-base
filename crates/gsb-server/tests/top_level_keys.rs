//! BACKLOG F62: the top level of the config file takes the engine's own
//! keys and the keys a game declares (`GameModule::owned_keys`) — of the
//! hosted game or of any other game compiled into this build (a file may
//! carry a sibling game's table). Any other top-level key — a typo'd
//! engine key (`tik_hz`), a misspelled table (`[metric.otlp]`,
//! `[[listener]]`, `[room.2]`) — stops startup before anything binds,
//! naming the key, instead of being silently ignored.

#![cfg(all(
    feature = "game-demo",
    feature = "game-arena",
    feature = "game-mmo",
    feature = "game-war"
))]

use gsb_server::{Config, ServerError};

/// Load `body` the way the server binary does (`Config::from_file`).
fn load(name: &str, body: &str) -> Config {
    let path = std::env::temp_dir().join(format!(
        "gsb-top-level-keys-{}-{name}.toml",
        std::process::id()
    ));
    std::fs::write(&path, body).expect("write temp config");
    let loaded = Config::from_file(&path);
    let _ = std::fs::remove_file(&path);
    loaded.unwrap_or_else(|e| panic!("{name}: the file parses: {e}"))
}

/// Start a server on `body` (an ephemeral port) and return its refusal.
async fn refusal(name: &str, body: &str) -> ServerError {
    let cfg = load(name, &format!("bind = \"127.0.0.1:0\"\n{body}"));
    match gsb_server::start_server(cfg).await {
        Err(e) => e,
        Ok(handle) => {
            handle.stop().await;
            panic!("{name}: started, ignoring the unknown key");
        }
    }
}

/// A typo'd engine key stops startup, naming the key and the engine key
/// it resembles.
#[tokio::test]
async fn a_typo_of_an_engine_key_stops_startup() {
    let msg = refusal("tik-hz", "tik_hz = 60\n").await.to_string();
    for part in ["`tik_hz`", "did you mean `tick_hz`?"] {
        assert!(msg.contains(part), "{part:?} missing: {msg}");
    }
}

/// A misspelled engine table stops startup, named as the file writes it.
#[tokio::test]
async fn a_misspelled_engine_table_stops_startup() {
    for (name, body, written, meant) in [
        (
            "metric",
            "[metric.otlp]\nendpoint = \"http://127.0.0.1:4318\"\n",
            "`[metric.otlp]`",
            "`metrics`",
        ),
        (
            "listener",
            "[[listener]]\ntransport = \"tcp\"\nbind = \"127.0.0.1:0\"\n",
            "`[[listener]]`",
            "`listeners`",
        ),
        ("room", "[room.2]\ntick_hz = 15\n", "`[room.2]`", "`rooms`"),
    ] {
        let msg = refusal(name, body).await.to_string();
        for part in [written, meant] {
            assert!(msg.contains(part), "{name}: {part} missing: {msg}");
        }
    }
}

/// Each in-tree game's own keys are accepted with that game hosted, and
/// every other compiled-in game's too (a file may carry a sibling's
/// table).
#[test]
fn each_game_owns_its_keys_and_accepts_its_siblings() {
    let files = [
        (
            "demo",
            "visibility = \"team\"\ntopology = \"single\"\ncommunication = \"always-full\"\nshard_count = 4\naoi_cell_size = 20.0\nteam_vision_radius = 25.0\nspawn_half_size = 50.0\ndisconnect_grace_secs = 30.0\n",
        ),
        ("arena", "[arena]\nteams = 2\n"),
        ("mmo", "[mmo]\nlogout = \"instant\"\n"),
        ("war", "[war]\nteam_budget = 30\n"),
    ];
    let all: String = files.iter().map(|(_, body)| *body).collect();
    let cfg = load("all-games", &all);
    for game in gsb_server::games::compiled_in() {
        let module = gsb_server::games::by_name(game).expect("compiled in");
        cfg.check_top_level_keys(&*module)
            .unwrap_or_else(|e| panic!("{game} refused a compiled-in game's keys: {e}"));
    }
    let owned = gsb_server::games::owned_keys();
    for (game, body) in files {
        let keys = &owned
            .iter()
            .find(|(g, _)| *g == game)
            .expect("compiled in")
            .1;
        let written: toml::Table = toml::from_str(body).expect("parses");
        let mut written: Vec<&str> = written.keys().map(String::as_str).collect();
        written.sort_unstable();
        let mut keys = keys.clone();
        keys.sort_unstable();
        assert_eq!(written, keys, "{game}'s owned keys");
    }
}

/// A sibling game's table does not stop a server hosting another game.
#[tokio::test]
async fn a_server_starts_with_a_sibling_games_table() {
    let cfg = load(
        "sibling",
        "bind = \"127.0.0.1:0\"\ngame = \"demo\"\n[arena]\nteams = 2\n",
    );
    let handle = gsb_server::start_server(cfg)
        .await
        .expect("the demo starts");
    handle.stop().await;
}

/// The demo reads no `[demo]` table: one refuses startup (it used to be
/// silently ignored).
#[tokio::test]
async fn a_demo_table_stops_startup() {
    let msg = refusal("demo-table", "[demo]\nvisibility = \"team\"\n")
        .await
        .to_string();
    assert!(msg.contains("`[demo]`"), "{msg}");
}

/// A third-party-shaped module that reads a flat key declares it; with
/// the default declaration only its own table is its.
struct Flat {
    keys: Option<Vec<&'static str>>,
}

impl gsb_server::GameModule for Flat {
    fn name(&self) -> &'static str {
        "flat"
    }

    fn owned_keys(&self) -> Vec<&'static str> {
        match &self.keys {
            Some(keys) => keys.clone(),
            None => vec![self.name()],
        }
    }

    fn configure(&mut self, _raw: &toml::Table, _engine: &Config) -> Result<(), ServerError> {
        Ok(())
    }

    fn register(&self, _table: &mut gsb_protocol::MessageTable) {}

    fn spawn_registry(&self, _parts: gsb_server::RegistryParts) -> gsb_server::RegistryTask {
        unreachable!("configure-only test")
    }

    fn describe(&self) -> String {
        "flat".into()
    }
}

/// A module overriding its owned keys gets its flat key; the default
/// gives it its table, and the flat key is refused.
#[test]
fn a_module_declaring_a_flat_key_owns_it() {
    let cfg = load("flat", "flat_speed = 3\n[flat]\nmode = 1\n");
    let declared = Flat {
        keys: Some(vec!["flat_speed", "flat"]),
    };
    cfg.check_top_level_keys(&declared).expect("declared");
    let default = Flat { keys: None };
    let msg = cfg
        .check_top_level_keys(&default)
        .expect_err("undeclared")
        .to_string();
    assert!(
        msg.contains("`flat_speed`") && msg.contains("flat: `flat`"),
        "{msg}"
    );
    let table_only = load("flat-table", "[flat]\nmode = 1\n");
    table_only
        .check_top_level_keys(&default)
        .expect("its table");
}
