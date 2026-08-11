//! The near-duplicate advisory a store returns.
//!
//! These are wire types, shared by every door into the system: the MCP
//! `store_memory` result and the HTTP create response both carry the same
//! report, for the same reason [`SupersedesOutcome`](crate::model::SupersedesOutcome)
//! lives here rather than in one transport's module — two copies of a wire
//! type drift, and the drift is only visible to whichever client happens to
//! use the door that fell behind.
//!
//! The report is *advisory*. It never blocks a store and never fails one;
//! it tells the caller that something very like what they just wrote is
//! already there, so they can supersede or reinforce instead of
//! accumulating restatements.

use serde::{Deserialize, Serialize};

use crate::model::MemoryId;

/// One existing memory that closely resembles a newly stored one.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NearDuplicateMatch {
    /// The existing memory.
    pub id: MemoryId,
    /// Cosine similarity to the new memory, in `[threshold, 1.0]`.
    pub score: f32,
    /// The existing memory's summary, truncated to
    /// [`NEAR_DUPLICATE_SUMMARY_MAX_BYTES`](crate::model::constants::NEAR_DUPLICATE_SUMMARY_MAX_BYTES).
    ///
    /// Carried inline on purpose: without it the caller has to round-trip
    /// `get_memory` before it can judge whether the new memory restates
    /// this one, which defeats an advisory meant to change behaviour in
    /// place. Empty when the record could not be loaded — a match with an
    /// unknown summary is still worth reporting.
    pub summary: String,
}

/// The set of existing memories that closely resemble a newly stored one.
///
/// `matches` is never empty: when nothing crosses the threshold the report
/// is *absent*, not empty. That is what [`from_matches`](Self::from_matches)
/// enforces, and it is why the field on each response type is an `Option`
/// with `skip_serializing_if`.
///
/// There is deliberately no `topId` convenience field. `matches[0]` is the
/// closest match — a duplicate scalar could only ever disagree with it.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NearDuplicateReport {
    /// The similarity at or above which a memory was included. Carried so
    /// the object is self-describing and a caller need not know which
    /// constant the server was built with.
    pub threshold: f32,
    /// Closest matches first.
    pub matches: Vec<NearDuplicateMatch>,
}

impl NearDuplicateReport {
    /// Build a report, or `None` when there is nothing to report.
    pub fn from_matches(threshold: f32, matches: Vec<NearDuplicateMatch>) -> Option<Self> {
        if matches.is_empty() {
            None
        } else {
            Some(Self { threshold, matches })
        }
    }
}

/// The marker appended to a summary that had to be cut.
const ELLIPSIS: &str = "…";

/// Truncate `summary` to at most `max_bytes` UTF-8 bytes, appending an
/// ellipsis when anything was cut.
///
/// The ellipsis is charged against the budget, so the result is never
/// longer than `max_bytes` (for any `max_bytes >= ELLIPSIS.len()`, which
/// every caller satisfies by a wide margin). The cut lands on a `char`
/// boundary, so the result is always valid UTF-8 — slicing a summary at a
/// fixed byte offset would panic the moment one contained an em dash.
pub fn truncate_summary(summary: &str, max_bytes: usize) -> String {
    if summary.len() <= max_bytes {
        return summary.to_string();
    }
    let mut end = max_bytes.saturating_sub(ELLIPSIS.len());
    while end > 0 && !summary.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{ELLIPSIS}", &summary[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_match_list_produces_no_report() {
        assert!(NearDuplicateReport::from_matches(0.85, Vec::new()).is_none());
    }

    #[test]
    fn truncate_summary_leaves_a_short_summary_alone() {
        assert_eq!(truncate_summary("short", 240), "short");
    }

    /// A fixed byte-offset slice would panic here; the boundary walk is
    /// the whole point of the helper.
    #[test]
    fn truncate_summary_cuts_on_a_char_boundary_and_stays_within_budget() {
        // Six 3-byte chars = 18 bytes.
        let s = "日本語日本語";
        let cut = truncate_summary(s, 10);
        assert!(cut.len() <= 10, "{} bytes", cut.len());
        assert!(cut.ends_with(ELLIPSIS), "{cut}");
        assert_eq!(cut, "日本…");
    }
}
