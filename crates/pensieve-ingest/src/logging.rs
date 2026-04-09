//! Logging helpers for compact, operationally useful messages.
//!
//! These helpers intentionally collapse whitespace and cap message length so
//! logs don't dump giant event payloads, tag arrays, or multiline blobs into
//! journald/Loki.

use std::fmt::Display;

const DEFAULT_LOG_VALUE_LIMIT: usize = 240;
const NOTICE_LOG_VALUE_LIMIT: usize = 160;

/// Compact an external/log-derived string into a single-line value.
pub(crate) fn compact_log_value(value: &str) -> String {
    compact_log_value_with_limit(value, DEFAULT_LOG_VALUE_LIMIT)
}

/// Compact a relay/server notice into a shorter single-line value.
pub fn compact_notice(value: &str) -> String {
    compact_log_value_with_limit(value, NOTICE_LOG_VALUE_LIMIT)
}

/// Compact an error into a single-line value with a bounded length.
pub fn compact_error(err: impl Display) -> String {
    compact_log_value(&err.to_string())
}

fn compact_log_value_with_limit(value: &str, max_chars: usize) -> String {
    let collapsed = value.split_whitespace().collect::<Vec<_>>().join(" ");
    truncate_chars(&collapsed, max_chars)
}

fn truncate_chars(value: &str, max_chars: usize) -> String {
    let mut chars = value.chars();
    let truncated: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{truncated}…")
    } else {
        truncated
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compact_log_value_collapses_whitespace() {
        assert_eq!(compact_log_value("hello\n  world\t!"), "hello world !");
    }

    #[test]
    fn compact_log_value_truncates_long_strings() {
        let input = "a".repeat(300);
        let output = compact_log_value(&input);
        assert_eq!(output.chars().count(), 241);
        assert!(output.ends_with('…'));
    }

    #[test]
    fn compact_notice_uses_shorter_limit() {
        let input = "b".repeat(300);
        let output = compact_notice(&input);
        assert_eq!(output.chars().count(), 161);
        assert!(output.ends_with('…'));
    }
}
