//! `[ticket]` at startup: absent changes nothing; next to the caller's
//! own hook it refuses; without the `ticket` feature it refuses by name;
//! with it, a good table builds the hook and a bad one refuses without
//! echoing a key.

use super::*;

const KEY: &str = "1eb9dbbbbc047c03fd70604e0071f0987e16b28b757225c11f00415d0e20b1a2";

fn cfg(table: &str) -> Config {
    Config {
        ticket: Some(toml::from_str(table).expect("a table")),
        ..Default::default()
    }
}

#[test]
fn no_table_keeps_the_callers_hooks() {
    let hooks = super::hooks(&Config::default(), ServerHooks::default()).expect("ok");
    assert!(hooks.ticket.is_none());
}

#[test]
fn hex_runs_are_redacted() {
    let msg = format!("invalid type: string \"k1:{KEY}\", expected a sequence");
    let r = redact(&msg);
    assert!(!r.contains("1eb9dbbb"), "{r}");
    assert!(r.contains("\"k1:<redacted>\""), "{r}");
    assert_eq!(redact("a deadbeef b"), "a deadbeef b");
}

#[cfg(not(feature = "ticket"))]
#[test]
fn without_the_feature_a_table_refuses_by_name() {
    let c = cfg(&format!(
        "issuer_keys = [\"k1:{KEY}\"]\naudience = \"eu-1\""
    ));
    let e = super::hooks(&c, ServerHooks::default())
        .err()
        .expect("refused");
    assert!(matches!(e, ServerError::TicketNotBuilt), "{e}");
}

#[cfg(feature = "ticket")]
mod built {
    use super::*;

    #[tokio::test]
    async fn a_good_table_builds_the_hook() {
        let c = cfg(&format!(
            "issuer_keys = [\"k1:{KEY}\"]\naudience = \"eu-1\""
        ));
        let hooks = super::super::hooks(&c, ServerHooks::default()).expect("built");
        let auth = hooks.ticket.expect("a hook");
        assert_eq!(auth.timeout, std::time::Duration::from_millis(2_000));
        let conflict = super::super::hooks(&c, ServerHooks { ticket: Some(auth) });
        assert!(matches!(conflict, Err(ServerError::TicketHookConflict)));
    }

    #[test]
    fn a_bad_table_refuses_without_echoing_a_key() {
        for (table, says) in [
            (
                format!("issuer_keys = \"k1:{KEY}\"\naudience = \"eu-1\""),
                "sequence",
            ),
            (
                format!("issuer_keys = [\"k1:{KEY}\"]\naudince = \"eu-1\""),
                "audince",
            ),
            (
                format!("issuer_keys = [\"k1:{}zz\"]\naudience = \"a\"", &KEY[..62]),
                "position 62",
            ),
        ] {
            let e = super::super::hooks(&cfg(&table), ServerHooks::default());
            let e = e.err().expect("refused").to_string();
            assert!(e.contains(says), "{e}");
            assert!(!e.contains(&KEY[..16]), "{e}");
        }
    }
}
