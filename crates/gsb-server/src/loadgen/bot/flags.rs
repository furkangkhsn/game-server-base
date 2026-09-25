//! Command-line flags that belong to one game: written for another game
//! they are an error, not silently ignored — the load generator's side of
//! the server's rule that an explicitly written key a game fixes refuses
//! startup (GAME-MODULE §4.3, §6 decision 2).

/// The 2D demo's own flags, each with why another game has no use for it.
const DEMO_ONLY: &[(&str, &str)] = &[
    (
        "--visibility",
        "it picks the demo's room strategy; the other games run their own \
         (the server refuses an explicit `visibility` for them)",
    ),
    (
        "--topology",
        "it picks the demo's room topology; the other games run their own \
         (the server refuses an explicit `topology` for them)",
    ),
    (
        "--shard-count",
        "it sizes the demo's shard grid; the arena is never sharded and the \
         MMO's world is its own 2×2 grid",
    ),
    (
        "--cell-size",
        "it is the demo's AOI cell (server and client view); the arena runs no \
         cell grid and the MMO's cell is its own 64 m",
    ),
    (
        "--vision-radius",
        "it is the demo's team-fog radius; the arena's is its own 15 m (3D)",
    ),
    (
        "--spawn-half-size",
        "it is the demo's spawn map (and the spread profile's homes); the \
         other games spawn on their own maps",
    ),
    (
        "--disconnect-grace-secs",
        "it is the demo's flat park grace, which the server refuses for the \
         other games (the arena reads `[arena] disconnect_grace_secs`, the MMO \
         `[mmo] logout_grace_secs`); their defaults hold",
    ),
    (
        "--profile",
        "it picks a demo bot's movement; the other games' bots move their own way",
    ),
    (
        "--still-frac",
        "it tunes the demo's still profile; the other games' bots move their own way",
    ),
];

/// The MMO's own flags, each with why another game has no use for it.
const MMO_ONLY: &[(&str, &str)] = &[
    (
        "--mmo-duel-frac",
        "it turns MMO bots into duelists across a shard seam; the other games' \
         bots have no seam to fight across",
    ),
    (
        "--mmo-crystallize",
        "it is the MMO's `[mmo] crystallize` setting; the other games do not \
         crystallize cross-seam fights",
    ),
];

/// Whether `flag` is one of the demo's own flags.
pub(crate) fn is_demo_only(flag: &str) -> bool {
    DEMO_ONLY.iter().any(|(f, _)| *f == flag)
}

/// Whether `flag` belongs to one game (the parser records the ones
/// written).
pub(crate) fn is_game_only(flag: &str) -> bool {
    is_demo_only(flag) || MMO_ONLY.iter().any(|(f, _)| *f == flag)
}

/// Refuse a run of `game` for which a flag in `written` was given that
/// does not apply to it (the demo takes all of them).
pub(crate) fn check_game_flags(game: &str, written: &[&str]) -> Result<(), String> {
    let foreign = |list: &'static [(&'static str, &'static str)]| {
        list.iter().find(|(f, _)| written.contains(f))
    };
    let refused = match game {
        g if g == gsb_server::games::demo::DemoModule::NAME => foreign(MMO_ONLY),
        "mmo" => foreign(DEMO_ONLY),
        _ => foreign(DEMO_ONLY).or_else(|| foreign(MMO_ONLY)),
    };
    match refused {
        None => Ok(()),
        Some((flag, why)) => Err(format!(
            "{flag} does not apply to --game {game}: {why} (try --help)"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every demo-only flag refuses a non-demo game and names itself and
    /// the game; the demo takes every one; an engine flag passes anywhere.
    #[test]
    fn a_demo_flag_refuses_another_game() {
        for (flag, _) in DEMO_ONLY {
            assert!(check_game_flags("demo", &[flag]).is_ok(), "{flag}");
            for game in ["arena", "mmo", "war"] {
                let e = check_game_flags(game, &["--duration", flag]).expect_err(flag);
                assert!(e.starts_with(flag), "{e}");
                assert!(e.contains(&format!("--game {game}")), "{e}");
            }
        }
        assert!(check_game_flags("arena", &[]).is_ok());
        assert!(!is_demo_only("--max-players"));
        assert!(is_demo_only("--visibility"));
    }

    /// The MMO's flags refuse the demo and the arena, and pass for the
    /// MMO; the parser records them.
    #[test]
    fn an_mmo_flag_refuses_another_game() {
        for (flag, _) in MMO_ONLY {
            assert!(is_game_only(flag) && !is_demo_only(flag), "{flag}");
            assert!(check_game_flags("mmo", &[flag]).is_ok(), "{flag}");
            for game in ["demo", "arena", "war"] {
                let e = check_game_flags(game, &[flag]).expect_err(flag);
                assert!(
                    e.starts_with(flag) && e.contains(&format!("--game {game}")),
                    "{e}"
                );
            }
        }
    }
}
