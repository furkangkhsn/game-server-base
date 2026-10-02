//! BACKLOG F63: the binaries report a startup error the way the operator
//! reads it — the error's `Display` text (a config parse error: the key,
//! the line and the quoted line), exit status 1 — never the `Debug` dump
//! (`Error: Parse { … input: Some("<the whole file>") … }`). Every case
//! fails before the server binds anything, so each run is a few
//! milliseconds.

mod refusal;
use refusal::*;

/// F61's refusal, as the binary prints it: the key, the entry's line and
/// the offending line quoted — and not one other line of the file.
#[test]
fn an_unknown_listener_key_names_the_key_and_the_line() {
    let body = "tick_hz = 30\n\
                \n\
                [[listeners]]\n\
                transport = \"tcp\"\n\
                bind = \"127.0.0.1:0\"\n\
                \n\
                [[listeners]]\n\
                listen_backlog = 4096\n\
                transport = \"ws\"\n\
                bind = \"127.0.0.1:0\"\n";
    let run = server_with_config("listener-key", body);
    run.refused_with(&[
        "gsb-server: cannot parse config file ",
        "unknown field `listen_backlog`",
        "line 8",
        "listen_backlog = 4096",
    ]);
    // The file's other lines stay out of the report.
    for line in ["tick_hz = 30", "transport = \"ws\"", "transport = \"tcp\""] {
        run.lacks(line);
    }
}

/// F62's refusal, as the binary prints it: a top-level key nobody reads
/// (a typo'd engine key) names the key and the key it resembles.
#[test]
fn an_unknown_top_level_key_names_the_key() {
    let run = server_with_config("top-level-key", "bind = \"127.0.0.1:0\"\ntik_hz = 60\n");
    run.refused_with(&[
        "gsb-server: unknown top-level config key `tik_hz`",
        "did you mean `tick_hz`?",
    ]);
    run.lacks("UnknownKey");
}

/// A value the file's types hold but no socket takes: the server's own
/// refusal, before the first bind.
#[test]
fn an_out_of_range_listen_backlog_names_the_key() {
    let run = server_with_config("backlog", "bind = \"127.0.0.1:0\"\nlisten_backlog = 0\n");
    run.refused_with(&["gsb-server: invalid `listen_backlog` 0: must be 1..=2147483647"]);
    run.lacks("BadListenBacklog");
}

/// A negative backlog does not fit the key's type: a parse error with
/// the line, like every other file mistake.
#[test]
fn a_negative_listen_backlog_is_a_parse_error_with_its_line() {
    let run = server_with_config(
        "negative-backlog",
        "bind = \"127.0.0.1:0\"\nlisten_backlog = -1\n",
    );
    run.refused_with(&["listen_backlog", "line 2", "listen_backlog = -1"]);
    run.lacks("bind = ");
}

/// A TLS door whose files do not load: the door and the file, named.
#[test]
fn a_missing_tls_file_names_the_door_and_the_file() {
    let run = server_with_config(
        "tls",
        "[[listeners]]\n\
         transport = \"tls\"\n\
         bind = \"127.0.0.1:0\"\n\
         tls_cert = \"/nonexistent/gsb-f63/cert.pem\"\n\
         tls_key = \"/nonexistent/gsb-f63/key.pem\"\n",
    );
    run.refused_with(&[
        "gsb-server: listener `127.0.0.1:0` (tls) did not start: ",
        "cannot open `tls_cert` file `/nonexistent/gsb-f63/cert.pem`",
    ]);
    run.lacks("Custom {");
}

/// A taken port: the address that could not be claimed, named.
#[test]
fn a_taken_port_names_the_address() {
    let taken = std::net::TcpListener::bind("127.0.0.1:0").expect("claim a port");
    let addr = taken.local_addr().expect("its address");
    let run = server_with_config("taken", &format!("bind = \"{addr}\"\n"));
    run.refused_with(&[&format!(
        "gsb-server: listener `{addr}` (tcp) did not start: "
    )]);
    run.lacks("Os {");
    drop(taken);
}

/// A config path the server cannot read (a directory): the path and the
/// operating system's reason.
#[test]
fn an_unreadable_config_names_the_path() {
    let dir = std::env::temp_dir().join(format!("gsb-f63-dir-{}.toml", std::process::id()));
    std::fs::create_dir_all(&dir).expect("a directory where the file should be");
    let run = server_with_path(&dir);
    let _ = std::fs::remove_dir(&dir);
    run.refused_with(&[&format!(
        "gsb-server: cannot read config file {}: ",
        dir.display()
    )]);
    run.lacks("Io {");
}

/// The load generator's in-process server refusing its selection: the
/// reason as a message and status 1, not a panic carrying the `Debug`.
#[test]
fn the_load_generator_reports_an_in_process_refusal() {
    let run = Run::of(
        env!("CARGO_BIN_EXE_gsb-loadgen"),
        &[
            "1",
            "--duration",
            "1",
            "--topology",
            "sharded",
            "--visibility",
            "team",
        ],
    );
    run.refused_with(&[
        "gsb-loadgen: the in-process server did not start: ",
        "breaks shard locality",
    ]);
    run.lacks("panicked");
    run.lacks("ShardedCrossInterest");
}

/// B5a: a sealed rUDP door (the default `udp_security`) without its
/// static key refuses startup — never a silent plaintext fallback — and
/// says how to fix it; a malformed key refuses without echoing a
/// character of it.
#[test]
fn a_sealed_rudp_door_without_its_key_refuses_startup() {
    let run = server_with_config(
        "udp-no-key",
        "bind = \"127.0.0.1:0\"\ntransport = \"udp\"\n",
    );
    run.refused_with(&[
        "gsb-server: rUDP static key: missing",
        "udp_static_key",
        "udp_security = \"plaintext\"",
    ]);
    let run = server_with_config(
        "udp-bad-key",
        "bind = \"127.0.0.1:0\"\ntransport = \"udp\"\nudp_static_key = \"5ec7e75ec7e7\"\n",
    );
    run.refused_with(&["gsb-server: rUDP static key: malformed"]);
    run.lacks("5ec7e7");
}
