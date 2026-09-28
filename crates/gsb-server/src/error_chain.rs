//! A startup error as the operator reads it (BACKLOG F63): the error's
//! `Display` text and the causes that text does not already carry —
//! never its `Debug` (a config parse error's `Debug` carries the whole
//! file and no line number).

use std::error::Error;

/// `err`'s `Display` text, then one `caused by: …` line per error in its
/// `source()` chain whose text is not already in what came before;
/// trailing whitespace trimmed.
///
/// WHY the containment test: this workspace's errors put their cause in
/// their own message AND expose it as `source()` (`cannot parse config
/// file {path}: {source}`), so printing every link would repeat each
/// cause. A cause the text does not carry — a third-party game's boxed
/// error with a source of its own — still gets its line. What the
/// `gsb-server` and `gsb-loadgen` binaries print when startup is refused
/// (with exit status 1); public so a binary hosting its own game through
/// [`crate::start_game_server`] can report the same way.
pub fn error_chain(err: &dyn Error) -> String {
    // Trailing whitespace trimmed: a TOML parse error's text ends with a
    // newline, which would leave a blank line under the message.
    let mut text = err.to_string().trim_end().to_owned();
    let mut next = err.source();
    while let Some(cause) = next {
        let line = cause.to_string();
        let line = line.trim_end();
        if !text.contains(line) {
            text.push_str("\ncaused by: ");
            text.push_str(line);
        }
        next = cause.source();
    }
    text
}

#[cfg(test)]
mod tests;
