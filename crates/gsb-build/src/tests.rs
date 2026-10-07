//! Unit tests: the generator's path rules and the game band's start.

use super::*;
use crate::check::{Index, Place};
use crate::generate::Requests;

fn place(package: &str, nesting: &[&str]) -> Place {
    Place {
        package: package.to_owned(),
        nesting: nesting.iter().map(|s| (*s).to_owned()).collect(),
    }
}

fn requests(externs: &[(&str, &str)]) -> Requests {
    let mut idx = Index::new();
    idx.insert(".game.GameError".into(), place("game", &["GameError"]));
    idx.insert(
        ".game.Outer.Inner".into(),
        place("game", &["Outer", "Inner"]),
    );
    idx.insert(".shared.err.Bad".into(), place("shared.err", &["Bad"]));
    idx.insert(".game.sub.Deep".into(), place("game.sub", &["Deep"]));
    idx.insert(".Root".into(), place("", &["Root"]));
    Requests::new(
        &Plan::default(),
        idx,
        externs
            .iter()
            .map(|(a, b)| ((*a).to_owned(), (*b).to_owned()))
            .collect(),
        CONTRACT_PATH.to_owned(),
    )
}

/// The error type is named as prost names it from the request's module:
/// same package → the bare type; nested → the parent's snake module;
/// another package → `super::` up to the common prefix; an extern
/// prefix wins.
#[test]
fn error_paths_follow_prost_naming() {
    let r = requests(&[(".shared", "::shared_crate")]);
    assert_eq!(r.rust_path("game", ".game.GameError"), "GameError");
    assert_eq!(r.rust_path("game", ".game.Outer.Inner"), "outer::Inner");
    assert_eq!(r.rust_path("game", ".game.sub.Deep"), "sub::Deep");
    assert_eq!(
        r.rust_path("game.sub", ".game.GameError"),
        "super::GameError"
    );
    assert_eq!(r.rust_path("game", ".Root"), "super::Root");
    assert_eq!(r.rust_path("", ".game.GameError"), "game::GameError");
    assert_eq!(
        r.rust_path("game", ".shared.err.Bad"),
        "::shared_crate::err::Bad"
    );
    let r = requests(&[]);
    assert_eq!(
        r.rust_path("game", ".shared.err.Bad"),
        "super::shared::err::Bad"
    );
}

/// The engine's range ends where the protocol's game band starts.
#[test]
fn the_game_band_is_the_protocols() {
    assert_eq!(GAME_BAND_START, gsb_protocol::op::GAME_BAND_START);
}
