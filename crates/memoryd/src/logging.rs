//! Zero-dependency stderr logger for daemon diagnostics.
//!
//! CLI command output (JSON results, doctor/stats reports) stays on stdout and
//! does not go through this module; only long-running `serve` diagnostics do.
//! Format: `{unix_ms} {LEVEL} memoryd: {message}`.

use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Level {
    Info,
    Warn,
    Error,
}

impl Level {
    fn as_str(self) -> &'static str {
        match self {
            Self::Info => "INFO",
            Self::Warn => "WARN",
            Self::Error => "ERROR",
        }
    }
}

pub(crate) fn emit(level: Level, args: fmt::Arguments<'_>) {
    eprintln!(
        "{} {} memoryd: {args}",
        crate::unix_ms_now(),
        level.as_str()
    );
}

macro_rules! log_info {
    ($($arg:tt)*) => {
        $crate::logging::emit($crate::logging::Level::Info, format_args!($($arg)*))
    };
}

macro_rules! log_warn {
    ($($arg:tt)*) => {
        $crate::logging::emit($crate::logging::Level::Warn, format_args!($($arg)*))
    };
}

macro_rules! log_error {
    ($($arg:tt)*) => {
        $crate::logging::emit($crate::logging::Level::Error, format_args!($($arg)*))
    };
}

pub(crate) use {log_error, log_info, log_warn};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levels_render_expected_labels() {
        assert_eq!(Level::Info.as_str(), "INFO");
        assert_eq!(Level::Warn.as_str(), "WARN");
        assert_eq!(Level::Error.as_str(), "ERROR");
    }
}
