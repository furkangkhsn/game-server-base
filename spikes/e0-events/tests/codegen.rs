//! The `(gsb.event)` option form compiles with the engine's vendored
//! protoc, and the proposed gsb-build pass reads it, refuses what must
//! not ship, and writes exactly the impls `src/game.rs` carries
//! (docs/EVENTS.md §4.1).

use std::path::{Path, PathBuf};
use std::process::Command;

use e0_events::codegen::{Problem, generate, plan};
use e0_events::contract::Dest;

fn descriptor_set(file: &str) -> Vec<u8> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let out: PathBuf = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("{file}.pb"));
    let protoc = protoc_bin_vendored::protoc_bin_path().expect("vendored protoc");
    let include = protoc_bin_vendored::include_path().expect("protoc include");
    let st = Command::new(protoc)
        .arg("--include_imports")
        .arg(format!("--descriptor_set_out={}", out.display()))
        .arg(format!("-I{}", root.join("proto").display()))
        .arg(format!("-I{}", root.join("tests/proto").display()))
        .arg(format!("-I{}", include.display()))
        .arg(file)
        .status()
        .expect("protoc runs");
    assert!(st.success(), "protoc accepted {file}");
    std::fs::read(out).unwrap()
}

#[test]
fn the_option_form_compiles_and_generates_the_impls() {
    let p = plan(&descriptor_set("game.proto")).expect("a clean plan");
    let got: Vec<(&str, u16, Dest)> = p.events.iter().map(|e| (e.rust.as_str(), e.op, e.to)).collect();
    assert_eq!(
        got,
        [
            ("GoldReceived", 2001, Dest::World),
            ("ChatMessage", 2002, Dest::Client),
            ("FriendOnline", 2003, Dest::Client),
            ("FriendOffline", 2004, Dest::Client),
            ("BalanceChanged", 2005, Dest::Both),
        ]
    );
    assert_eq!(p.requests.get(&1001).map(String::as_str), Some(".game.TransferGold"));
    assert!(p.reserved.contains(&2099));

    let src = std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("src/game.rs")).unwrap();
    let in_game: Vec<&str> = src.lines().filter(|l| l.starts_with("impl ")).collect();
    let generated = generate(&p);
    assert_eq!(generated.lines().collect::<Vec<_>>(), in_game);
}

#[test]
fn the_checks_refuse_every_bad_declaration() {
    let problems = plan(&descriptor_set("bad.proto")).expect_err("refused");
    let want = [
        Problem::EngineOp(".bad.Low".into(), 17),
        Problem::OpTooLarge(".bad.Big".into(), 70000),
        Problem::NoDestination(".bad.NoWhere".into()),
        Problem::Duplicate {
            op: 1001,
            first: ".bad.Both".into(),
            second: ".bad.Clash".into(),
        },
        Problem::EventIsRequest(".bad.Both".into()),
        Problem::Reserved(".bad.Retired".into(), 2010),
    ];
    for w in &want {
        assert!(problems.contains(w), "missing {w:?} in {problems:?}");
    }
    assert_eq!(problems.len(), want.len(), "{problems:?}");
}
