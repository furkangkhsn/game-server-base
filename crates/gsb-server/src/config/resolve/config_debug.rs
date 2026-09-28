//! [`ConfigError`]'s `Debug` without the file (BACKLOG F63): a
//! `toml::de::Error` keeps a copy of the whole input for its `Display`
//! snippet, and its derived `Debug` prints that copy. Here a parse error
//! shows the path, the message and the byte span — what a
//! `Config::from_file(..).unwrap()` panic or a `{:?}` log needs.

use std::fmt;

use super::ConfigError;

impl fmt::Debug for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { path, source } => f
                .debug_struct("Io")
                .field("path", path)
                .field("source", source)
                .finish(),
            Self::Parse { path, source } => f
                .debug_struct("Parse")
                .field("path", path)
                .field("message", &source.message())
                .field("span", &source.span())
                .finish(),
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::Config;

    /// The parse error's `Debug` names the key and its span, and carries
    /// no other line of the file.
    #[test]
    fn a_parse_error_debug_leaves_the_file_out() {
        let path =
            std::env::temp_dir().join(format!("gsb-config-debug-{}.toml", std::process::id()));
        std::fs::write(&path, "tick_hz = 30\nlisten_backlog = -1\n").expect("write");
        let err = Config::from_file(&path).expect_err("refused");
        let _ = std::fs::remove_file(&path);
        let debug = format!("{err:?}");
        assert!(debug.starts_with("Parse { path: "), "{debug}");
        assert!(debug.contains("expected u32"), "{debug}");
        assert!(debug.contains("span: Some("), "{debug}");
        assert!(!debug.contains("tick_hz"), "{debug}");
    }
}
