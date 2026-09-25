//! The parser's refusals are errors carrying the reason (the binary
//! prints them and exits 2 — `tests/loadgen_games.rs`), and a good line
//! still parses.

use super::*;

fn line(argv: &[&str]) -> Result<Cli, CliError> {
    parse(&argv.iter().map(|s| s.to_string()).collect::<Vec<_>>())
}

fn refused(argv: &[&str]) -> String {
    match line(argv) {
        Err(CliError(why)) => why,
        Ok(_) => panic!("{argv:?} parsed"),
    }
}

/// Each kind of refusal, with its message.
#[test]
fn a_bad_line_is_an_error_with_its_reason() {
    let cases: [(&[&str], &str); 10] = [
        (&["--duration"], "--duration needs a value (try --help)"),
        (
            &["--duration", "x"],
            "--duration: expected a number, got `x`",
        ),
        (&["many"], "N: expected a number, got `many`"),
        (&["--frobnicate"], "unknown flag --frobnicate (try --help)"),
        (
            &["--profile", "zigzag"],
            "--profile: expected ring|spread|still (try --help)",
        ),
        (
            &["--transport", "carrier-pigeon"],
            "--transport: expected tcp|udp, got carrier-pigeon",
        ),
        (&["--still-frac", "2"], "--still-frac must be in 0..=1"),
        (
            &["--orchestrate", "--serve"],
            "--orchestrate and --serve are mutually exclusive (try --help)",
        ),
        (&["0"], "N must be > 0"),
        (&["--capture-clients", "0"], "--capture-clients must be > 0"),
    ];
    for (argv, why) in cases {
        assert_eq!(refused(argv), why, "{argv:?}");
    }
    assert!(refused(&["--game", "chess"]).contains("unknown game `chess`"));
    assert_eq!(
        refused(&["--addr", "localhost"]),
        "--addr: expected HOST:PORT (an IP address and a port), got `localhost`"
    );
    assert!(refused(&["--serve", "--metrics-listen", "x:1"]).starts_with("--metrics-listen"));
    assert!(line(&["--addr", "127.0.0.1:7777"]).is_ok());
}

/// A good line is a run with its knobs; `--help` is a help request.
#[test]
fn a_good_line_parses() {
    let Ok(Cli::Run(args)) = line(&["7", "--duration", "2.5", "--game", "demo"]) else {
        panic!("a run");
    };
    assert_eq!(args.clients, 7);
    assert_eq!(args.duration, Duration::from_millis(2_500));
    assert!(matches!(line(&["5", "--help"]), Ok(Cli::Help)));
}
