//! `[ticket]` (BACKLOG B21): the signed-ticket validator from the
//! config, as the server's ticket hook — or a refused startup.

use tracing::info;

use crate::{Config, ServerError, ServerHooks};

/// The hooks this server runs with: the caller's, or — when the config
/// has a `[ticket]` table — the validator it describes. Both at once is
/// refused (one authority per server).
pub(super) fn hooks(cfg: &Config, hooks: ServerHooks) -> Result<ServerHooks, ServerError> {
    let Some(table) = &cfg.ticket else {
        return Ok(hooks);
    };
    if hooks.ticket.is_some() {
        return Err(ServerError::TicketHookConflict);
    }
    let auth = build(table)?;
    info!(
        keys = table
            .get("issuer_keys")
            .and_then(|k| k.as_array())
            .map_or(0, Vec::len),
        audience = table.get("audience").and_then(|a| a.as_str()).unwrap_or(""),
        "ticket auth: signed tickets (PASETO v4.public) from [ticket]"
    );
    Ok(ServerHooks { ticket: Some(auth) })
}

#[cfg(feature = "ticket")]
fn build(table: &toml::Table) -> Result<gsb_core::auth::TicketAuth, ServerError> {
    let bad = |e: String| ServerError::BadTicket(redact(&e));
    let cfg: gsb_ticket::ValidatorConfig = toml::Value::Table(table.clone())
        .try_into()
        .map_err(|e: toml::de::Error| bad(e.message().to_owned()))?;
    cfg.auth().map_err(|e| bad(e.0))
}

#[cfg(not(feature = "ticket"))]
fn build(_table: &toml::Table) -> Result<gsb_core::auth::TicketAuth, ServerError> {
    Err(ServerError::TicketNotBuilt)
}

/// `msg` with every run of 16+ hex characters replaced: a type error
/// (an issuer key written where a list belongs) must not echo the key.
#[cfg_attr(not(feature = "ticket"), allow(dead_code))]
fn redact(msg: &str) -> String {
    let mut out = String::with_capacity(msg.len());
    let mut run = String::new();
    for c in msg.chars().chain(std::iter::once(' ')) {
        if c.is_ascii_hexdigit() {
            run.push(c);
            continue;
        }
        if run.len() >= 16 {
            out.push_str("<redacted>");
        } else {
            out.push_str(&run);
        }
        run.clear();
        out.push(c);
    }
    out.pop();
    out
}

#[cfg(test)]
mod tests;
