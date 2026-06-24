//! Timezone utilities for human-readable timestamp formatting.
//!
//! All timestamps in Recalld are stored as UTC epoch milliseconds. This module
//! provides timezone resolution and formatting for display contexts (MCP, CLI).

use chrono::Utc;
use chrono_tz::Tz;

/// Resolves a timezone configuration string to a `chrono_tz::Tz`.
///
/// Accepts:
/// - `"UTC"` or `"utc"` — explicit UTC
/// - `"local"` or `"Local"` — falls back to UTC (chrono-tz cannot reliably
///   detect the system timezone on all platforms)
/// - Any IANA timezone name (e.g. `"America/New_York"`, `"Europe/London"`)
///
/// Invalid timezone names log a warning and fall back to UTC.
pub fn resolve_timezone(config: &str) -> Tz {
    match config {
        "UTC" | "utc" => Tz::UTC,
        "local" | "Local" => Tz::UTC,
        iana_name => iana_name.parse::<Tz>().unwrap_or_else(|_| {
            tracing::warn!(
                timezone = iana_name,
                "Invalid timezone, falling back to UTC"
            );
            Tz::UTC
        }),
    }
}

/// Format a Unix epoch milliseconds timestamp as a human-readable string
/// in the given timezone.
///
/// Returns format: `"2024-06-24 10:00:00 EDT"` (date time timezone-abbreviation).
pub fn format_timestamp(millis: i64, tz: Tz) -> String {
    let datetime = match chrono::DateTime::<Utc>::from_timestamp_millis(millis) {
        Some(dt) => dt,
        None => {
            tracing::warn!(millis, "Invalid timestamp");
            return format!("<invalid timestamp: {}>", millis);
        }
    };

    let local_dt = datetime.with_timezone(&tz);

    // Format: "2024-06-24 10:00:00 EDT"
    format!(
        "{} {}",
        local_dt.format("%Y-%m-%d %H:%M:%S"),
        local_dt.format("%Z")
    )
}

/// Parse an ISO 8601 timestamp string to Unix epoch milliseconds.
///
/// Supports:
/// - RFC 3339: `"2024-06-24T10:00:00Z"`, `"2024-06-24T10:00:00-04:00"`
/// - Naive datetime (no timezone, assumes UTC): `"2024-06-24T10:00:00"`
/// - Date-only (assumes 00:00:00 UTC): `"2024-06-24"`
pub fn parse_iso8601_to_millis(s: &str) -> Result<i64, String> {
    // Try parsing as RFC 3339 first.
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s) {
        return Ok(dt.timestamp_millis());
    }

    // Try parsing as naive datetime (no timezone) and assume UTC.
    if let Ok(naive_dt) = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S") {
        return Ok(naive_dt.and_utc().timestamp_millis());
    }

    // Try parsing as date-only and assume 00:00:00 UTC.
    if let Ok(naive_date) = chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        return Ok(naive_date
            .and_hms_opt(0, 0, 0)
            .ok_or_else(|| "Failed to construct datetime".to_string())?
            .and_utc()
            .timestamp_millis());
    }

    Err(format!(
        "Failed to parse '{}' as ISO 8601 timestamp. Supported formats: \
         RFC 3339 (2024-06-24T10:00:00Z), naive datetime (2024-06-24T10:00:00), \
         or date (2024-06-24)",
        s
    ))
}

/// Parse a `serde_json::Value` that may be either an integer (epoch millis)
/// or a string (ISO 8601) into epoch milliseconds.
pub fn parse_time_value(value: &serde_json::Value) -> Option<Result<i64, String>> {
    match value {
        serde_json::Value::Number(n) => n.as_i64().map(Ok),
        serde_json::Value::String(s) => Some(parse_iso8601_to_millis(s)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resolve_utc() {
        assert_eq!(resolve_timezone("UTC"), Tz::UTC);
        assert_eq!(resolve_timezone("utc"), Tz::UTC);
    }

    #[test]
    fn test_resolve_local_falls_back_to_utc() {
        assert_eq!(resolve_timezone("local"), Tz::UTC);
        assert_eq!(resolve_timezone("Local"), Tz::UTC);
    }

    #[test]
    fn test_resolve_iana() {
        let tz = resolve_timezone("America/New_York");
        assert_eq!(tz, chrono_tz::America::New_York);
    }

    #[test]
    fn test_resolve_invalid_falls_back_to_utc() {
        assert_eq!(resolve_timezone("Not/A/Timezone"), Tz::UTC);
    }

    #[test]
    fn test_format_timestamp_utc() {
        // 2024-06-24 10:00:00 UTC in millis
        let millis = 1719223200000_i64;
        let formatted = format_timestamp(millis, Tz::UTC);
        assert_eq!(formatted, "2024-06-24 10:00:00 UTC");
    }

    #[test]
    fn test_format_timestamp_new_york() {
        // 2024-06-24 10:00:00 UTC => 06:00:00 EDT
        let millis = 1719223200000_i64;
        let formatted = format_timestamp(millis, chrono_tz::America::New_York);
        assert_eq!(formatted, "2024-06-24 06:00:00 EDT");
    }

    #[test]
    fn test_parse_rfc3339() {
        let result = parse_iso8601_to_millis("2024-06-24T10:00:00Z");
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), 1719223200000);
    }

    #[test]
    fn test_parse_rfc3339_with_offset() {
        let result = parse_iso8601_to_millis("2024-06-24T06:00:00-04:00");
        assert!(result.is_ok());
        // 06:00 EDT = 10:00 UTC
        assert_eq!(result.unwrap(), 1719223200000);
    }

    #[test]
    fn test_parse_naive_datetime() {
        let result = parse_iso8601_to_millis("2024-06-24T10:00:00");
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), 1719223200000);
    }

    #[test]
    fn test_parse_date_only() {
        let result = parse_iso8601_to_millis("2024-06-24");
        assert!(result.is_ok());
        // 2024-06-24 00:00:00 UTC
        assert_eq!(result.unwrap(), 1719187200000);
    }

    #[test]
    fn test_parse_invalid() {
        let result = parse_iso8601_to_millis("not a date");
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_time_value_integer() {
        let val = serde_json::json!(1719223200000_i64);
        let result = parse_time_value(&val);
        assert_eq!(result, Some(Ok(1719223200000)));
    }

    #[test]
    fn test_parse_time_value_string() {
        let val = serde_json::json!("2024-06-24T10:00:00Z");
        let result = parse_time_value(&val);
        assert_eq!(result, Some(Ok(1719223200000)));
    }

    #[test]
    fn test_parse_time_value_null() {
        let val = serde_json::Value::Null;
        let result = parse_time_value(&val);
        assert_eq!(result, None);
    }
}
