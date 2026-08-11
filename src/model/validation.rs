//! Shared input validation for memory-creating entry points.
//!
//! The HTTP API, the batch HTTP endpoint, and the MCP tool handlers all
//! accept the same logical payload but each used to carry its own hand-
//! rolled copy of the limit checks. Those copies drifted: the batch
//! endpoint checked nothing but emptiness, and every message claimed the
//! limits were measured in *characters* when the code measured UTF-8
//! *bytes*. This module is the single source of truth.
//!
//! The functions here operate on borrowed request data, before any
//! [`Memory`](crate::model::Memory) exists, which is why they are free
//! functions rather than methods on `Memory`. `Memory::validate()`
//! delegates its content-limit checks here so the two cannot diverge.

use super::constants::{
    FULL_TEXT_MAX_BYTES, MAX_EMOTIONS, MAX_ENTITIES, MAX_TAGS, MAX_TOPICS,
    NAMESPACE_NAME_MAX_BYTES, SUMMARY_MAX_BYTES,
};
use super::error::ValidationError;

/// A borrowed view of the user-supplied fields of a store request.
///
/// `entities`, `topics`, and `emotions` are input-only: callers merge
/// them into `tags` after validation, so they must be checked here while
/// they are still distinguishable.
pub struct MemoryInputRef<'a> {
    pub summary: &'a str,
    pub full_text: Option<&'a str>,
    pub tags: &'a [String],
    pub entities: &'a [String],
    pub topics: &'a [String],
    pub emotions: &'a [String],
}

/// Validate every content limit on a store request.
///
/// Checks run in a fixed order — summary emptiness, summary length,
/// full text length, then the array counts — so a request violating
/// several limits always reports the same one.
pub fn validate_memory_input(input: MemoryInputRef<'_>) -> Result<(), ValidationError> {
    if input.summary.is_empty() {
        return Err(ValidationError::SummaryEmpty);
    }
    validate_text_len("summary", input.summary, SUMMARY_MAX_BYTES)?;
    if let Some(ft) = input.full_text {
        validate_text_len("fullText", ft, FULL_TEXT_MAX_BYTES)?;
    }
    validate_count("tags", input.tags.len(), MAX_TAGS)?;
    validate_count("entities", input.entities.len(), MAX_ENTITIES)?;
    validate_count("topics", input.topics.len(), MAX_TOPICS)?;
    validate_count("emotions", input.emotions.len(), MAX_EMOTIONS)?;
    Ok(())
}

/// Check that `value` fits within `max_bytes` UTF-8 bytes.
///
/// The character count is computed only when the check fails, so the
/// happy path costs one `len()`.
pub fn validate_text_len(
    field: &'static str,
    value: &str,
    max_bytes: usize,
) -> Result<(), ValidationError> {
    let bytes = value.len();
    if bytes > max_bytes {
        return Err(ValidationError::FieldTooLong {
            field,
            bytes,
            chars: value.chars().count(),
            max_bytes,
        });
    }
    Ok(())
}

/// Check that an array field holds at most `max` items.
pub fn validate_count(
    field: &'static str,
    count: usize,
    max: usize,
) -> Result<(), ValidationError> {
    if count > max {
        return Err(ValidationError::TooManyItems { field, count, max });
    }
    Ok(())
}

/// Validate a namespace name: 1-64 bytes of ASCII alphanumerics,
/// hyphens, or underscores.
///
/// Namespace names become directory names on disk, so the character set
/// is deliberately restricted to ASCII rather than Unicode
/// alphanumerics.
pub fn validate_namespace_name(name: &str) -> Result<(), ValidationError> {
    let valid = !name.is_empty()
        && name.len() <= NAMESPACE_NAME_MAX_BYTES
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if valid {
        Ok(())
    } else {
        Err(ValidationError::InvalidNamespaceName {
            max: NAMESPACE_NAME_MAX_BYTES,
        })
    }
}

/// Return the longest prefix of `text` that is at most `max_bytes` long
/// and ends on a character boundary.
///
/// Slicing a `&str` at an arbitrary byte offset panics when the offset
/// splits a multi-byte character; this is the non-panicking form.
/// (`str::floor_char_boundary` would do the same job but is still
/// unstable.)
pub fn truncate_on_char_boundary(text: &str, max_bytes: usize) -> &str {
    if text.len() <= max_bytes {
        return text;
    }
    let end = text
        .char_indices()
        .map(|(i, _)| i)
        .take_while(|&i| i <= max_bytes)
        .last()
        .unwrap_or(0);
    &text[..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(summary: &str) -> MemoryInputRef<'_> {
        MemoryInputRef {
            summary,
            full_text: None,
            tags: &[],
            entities: &[],
            topics: &[],
            emotions: &[],
        }
    }

    fn strings(n: usize) -> Vec<String> {
        (0..n).map(|i| format!("t{i}")).collect()
    }

    const HINT: &str = "Limits are measured in UTF-8 bytes, not characters";

    // ── Summary length: bytes vs characters ──────────────────────────

    #[test]
    fn em_dash_summary_reports_both_units_and_hint() {
        // 1900 em dashes = 1900 characters but 5700 bytes. This is the
        // case that produced the misleading "exceeds maximum length of
        // 2000 characters" message.
        let summary = "—".repeat(1900);
        assert_eq!(summary.len(), 5700);
        let err = validate_memory_input(input(&summary)).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("5700 bytes (1900 characters)"), "{msg}");
        assert!(msg.contains("2000-byte limit"), "{msg}");
        assert!(msg.contains(HINT), "{msg}");
        assert_eq!(err.field(), Some("summary"));
    }

    #[test]
    fn ascii_1900_is_accepted() {
        assert!(validate_memory_input(input(&"a".repeat(1900))).is_ok());
    }

    #[test]
    fn ascii_2100_reports_equal_units_without_hint() {
        let err = validate_memory_input(input(&"a".repeat(2100))).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("2100 bytes (2100 characters)"), "{msg}");
        assert!(!msg.contains(HINT), "{msg}");
    }

    #[test]
    fn summary_byte_boundary_is_inclusive() {
        assert!(validate_memory_input(input(&"a".repeat(2000))).is_ok());
        assert!(validate_memory_input(input(&"a".repeat(2001))).is_err());
    }

    #[test]
    fn em_dash_boundary_counts_bytes_not_chars() {
        let under = "—".repeat(666); // 1998 bytes
        assert_eq!(under.len(), 1998);
        assert!(validate_memory_input(input(&under)).is_ok());

        let over = "—".repeat(667); // 2001 bytes, 667 characters
        assert_eq!(over.len(), 2001);
        let msg = validate_memory_input(input(&over)).unwrap_err().to_string();
        assert!(msg.contains("2001 bytes (667 characters)"), "{msg}");
    }

    #[test]
    fn four_byte_chars_at_exact_byte_limit_are_accepted() {
        let summary = "🧠".repeat(500); // 2000 bytes, 500 characters
        assert_eq!(summary.len(), 2000);
        assert!(validate_memory_input(input(&summary)).is_ok());
    }

    #[test]
    fn empty_summary_is_rejected() {
        let err = validate_memory_input(input("")).unwrap_err();
        assert!(matches!(err, ValidationError::SummaryEmpty));
        assert_eq!(err.field(), Some("summary"));
    }

    // ── full_text ────────────────────────────────────────────────────

    #[test]
    fn full_text_boundary() {
        let at_limit = "a".repeat(FULL_TEXT_MAX_BYTES);
        let mut inp = input("ok");
        inp.full_text = Some(&at_limit);
        assert!(validate_memory_input(inp).is_ok());

        let over = "a".repeat(FULL_TEXT_MAX_BYTES + 1);
        let mut inp = input("ok");
        inp.full_text = Some(&over);
        let err = validate_memory_input(inp).unwrap_err();
        assert_eq!(err.field(), Some("fullText"));
        assert!(err.to_string().contains("1048576-byte limit"));
    }

    // ── Array counts ─────────────────────────────────────────────────

    #[test]
    fn array_count_limits() {
        let cases: [(&str, usize); 4] = [
            ("tags", MAX_TAGS),
            ("entities", MAX_ENTITIES),
            ("topics", MAX_TOPICS),
            ("emotions", MAX_EMOTIONS),
        ];
        for (field, max) in cases {
            let at = strings(max);
            let over = strings(max + 1);
            for (items, should_err) in [(&at, false), (&over, true)] {
                let mut inp = input("ok");
                match field {
                    "tags" => inp.tags = items.as_slice(),
                    "entities" => inp.entities = items.as_slice(),
                    "topics" => inp.topics = items.as_slice(),
                    _ => inp.emotions = items.as_slice(),
                }
                let result = validate_memory_input(inp);
                if should_err {
                    let err = result.unwrap_err();
                    assert_eq!(err.field(), Some(field));
                    match err {
                        ValidationError::TooManyItems { count, max: m, .. } => {
                            assert_eq!(count, max + 1);
                            assert_eq!(m, max);
                        }
                        other => panic!("unexpected error for {field}: {other}"),
                    }
                } else {
                    assert!(result.is_ok(), "{field} at {max} should be accepted");
                }
            }
        }
    }

    #[test]
    fn summary_is_reported_before_tag_count() {
        let summary = "a".repeat(3000);
        let tags = strings(100);
        let mut inp = input(&summary);
        inp.tags = &tags;
        let err = validate_memory_input(inp).unwrap_err();
        assert_eq!(err.field(), Some("summary"));
    }

    // ── truncate_on_char_boundary ────────────────────────────────────

    #[test]
    fn truncate_never_splits_a_character() {
        let text = "—".repeat(300); // 900 bytes, 3 bytes per char
        let cut = truncate_on_char_boundary(&text, 200);
        assert!(cut.len() <= 200);
        assert_eq!(cut.len(), 198); // largest multiple of 3 <= 200
        assert!(text.starts_with(cut));
    }

    #[test]
    fn truncate_returns_whole_string_when_short_enough() {
        assert_eq!(truncate_on_char_boundary("hello", 200), "hello");
        assert_eq!(truncate_on_char_boundary("hello", 5), "hello");
    }

    #[test]
    fn truncate_to_zero_or_below_first_char_yields_empty() {
        assert_eq!(truncate_on_char_boundary("—abc", 0), "");
        assert_eq!(truncate_on_char_boundary("—abc", 2), "");
    }

    // ── Namespace names ──────────────────────────────────────────────

    #[test]
    fn namespace_names() {
        assert!(validate_namespace_name("a-b_c1").is_ok());
        assert!(validate_namespace_name(&"a".repeat(64)).is_ok());

        assert!(validate_namespace_name("").is_err());
        assert!(validate_namespace_name(&"a".repeat(65)).is_err());
        assert!(validate_namespace_name("a b").is_err());
        assert!(validate_namespace_name("ünïcode").is_err());
    }

    #[test]
    fn namespace_error_message_says_bytes() {
        let err = validate_namespace_name("").unwrap_err();
        assert!(err.to_string().contains("1-64 bytes"), "{err}");
        assert_eq!(err.field(), Some("name"));
    }
}
