//! Validated, lowercased tag newtype with hierarchical naming support.

use std::collections::HashSet;
use std::fmt;
use std::ops::Deref;

use serde::{Deserialize, Serialize};

use crate::model::constants::TAG_MAX_BYTES;
use crate::model::error::TagError;

/// A validated, lowercased tag string.
///
/// Tags are hierarchical with `/` separators (e.g., `lang/rust`,
/// `project/recalld`). They are stored lowercased for
/// case-insensitive matching.
///
/// Allowed characters: `[a-z0-9/_.::-]` (alphanumeric, hyphens,
/// underscores, forward slashes, periods, colons).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Tag(String);

impl Tag {
    /// Maximum byte length of a tag.
    pub const MAX_LEN: usize = TAG_MAX_BYTES;

    /// Create a new tag, lowercasing the input and validating format.
    pub fn new(s: impl Into<String>) -> Result<Self, TagError> {
        let s = s.into().to_lowercase();

        if s.is_empty() {
            return Err(TagError::Empty);
        }

        if s.len() > Self::MAX_LEN {
            return Err(TagError::TooLong {
                len: s.len(),
                max: Self::MAX_LEN,
            });
        }

        // Must start with alphanumeric.
        let first = s.as_bytes()[0];
        if !first.is_ascii_alphanumeric() {
            return Err(TagError::InvalidStartChar(first as char));
        }

        // Remaining characters: alphanumeric, hyphens, underscores,
        // slashes, periods, colons.
        for ch in s.chars() {
            if !matches!(ch, 'a'..='z' | '0'..='9' | '-' | '_' | '/' | '.' | ':') {
                return Err(TagError::InvalidChar(ch));
            }
        }

        Ok(Tag(s))
    }

    /// Return the inner string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Deref for Tag {
    type Target = str;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl fmt::Display for Tag {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<String> for Tag {
    type Error = TagError;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        Tag::new(s)
    }
}

impl From<Tag> for String {
    fn from(tag: Tag) -> Self {
        tag.0
    }
}

/// Parse `raw` into [`Tag`]s, dropping — but never silencing — the
/// elements that fail validation.
///
/// Every call site used to spell this `filter_map(|t| Tag::new(t).ok())`,
/// so a store reported success while quietly keeping fewer tags than it
/// was given, and a search silently ran with a weaker filter than the
/// caller asked for. The drop is kept (rejecting a whole store over one
/// malformed tag would be a harder break than the bug) but each one now
/// leaves a `warn!` naming the field, the offending value and the
/// reason.
///
/// [`TagError::TooLong`] is unreachable from a validated store request:
/// [`validate_memory_input`](crate::model::validate_memory_input)
/// rejects tags over [`TAG_MAX_BYTES`] and reserves the derived-tag
/// prefix out of the `entities`/`topics`/`emotions` limits, so no
/// request that validates can produce an over-long tag here. What
/// remains reachable is the alphabet: `Tag::new` accepts only
/// `[a-z0-9/_.:-]`, which the request validator deliberately does not
/// duplicate (an entity named `José` is a legitimate thing to send).
pub fn parse_tags_lossy(context: &str, raw: &[String]) -> Vec<Tag> {
    raw.iter()
        .filter_map(|t| match Tag::new(t) {
            Ok(tag) => Some(tag),
            Err(e) => {
                tracing::warn!(
                    context = %context,
                    tag = %t,
                    error = %e,
                    "Dropping invalid tag"
                );
                None
            }
        })
        .collect()
}

/// A query filter that names a label no tag can carry.
#[derive(Debug, thiserror::Error)]
#[error(
    "Filter '{field}' cannot be applied: {value:?} is not a valid tag ({source}). \
     Tags allow only a-z, 0-9 and - _ / . : — entity, topic and emotion filters are \
     matched as `entity/<name>`, `topic/<name>` and `emotion/<name>` tags, so no \
     memory can carry this label."
)]
pub struct TagFilterError {
    /// Which request field the bad label came from.
    pub field: String,
    /// The label, as sent.
    pub value: String,
    /// Why it is not a tag.
    pub source: TagError,
}

/// Parse `raw` into [`Tag`]s for a query FILTER, failing on the first
/// element that will not validate. `prefix` is prepended to each element
/// (`"entity/"`, `"topic/"`, `"emotion/"`, or `""` for plain tags).
///
/// The mirror image of [`parse_tags_lossy`], and deliberately the
/// opposite trade-off. Dropping a tag from a store keeps the memory and
/// loses one label. Dropping a tag from a filter **widens the query**:
/// `list_memories {"entities": ["José"]}` derived `entity/josé`, failed
/// the alphabet, swallowed the error and returned EVERY memory in the
/// namespace as though it had matched a filter. A caller cannot tell a
/// widened answer from a genuine one, and an agent acting on "these are
/// the memories about José" gets the whole namespace instead.
///
/// An error is recoverable. A wrong answer that looks right is not.
pub fn parse_filter_tags(
    field: &str,
    prefix: &str,
    raw: &[String],
) -> Result<Vec<Tag>, TagFilterError> {
    raw.iter()
        .map(|label| {
            Tag::new(format!("{prefix}{}", label.to_lowercase())).map_err(|source| TagFilterError {
                field: field.to_string(),
                value: label.clone(),
                source,
            })
        })
        .collect()
}

/// Structured metadata extracted from hierarchical tags.
#[derive(Debug, Clone, Default)]
pub struct StructuredMetadata {
    pub entities: Vec<String>,
    pub topics: Vec<String>,
    pub emotions: Vec<String>,
}

/// Parse a slice of tags into structured metadata buckets by prefix.
///
/// - `entity/<name>` -> entities
/// - `topic/<topic>` -> topics
/// - `emotion/<emotion>` -> emotions
/// - All other tags (e.g. `activity/hiking`) are ignored.
pub fn parse_structured_tags(tags: &[Tag]) -> StructuredMetadata {
    let mut result = StructuredMetadata::default();
    for tag in tags {
        let s = tag.as_str();
        if let Some(name) = s.strip_prefix("entity/") {
            if !name.is_empty() {
                let display_name = name
                    .split_whitespace()
                    .map(|w| {
                        let mut chars = w.chars();
                        match chars.next() {
                            Some(c) => c.to_uppercase().to_string() + chars.as_str(),
                            None => String::new(),
                        }
                    })
                    .collect::<Vec<_>>()
                    .join(" ");
                result.entities.push(display_name);
            }
        } else if let Some(topic) = s.strip_prefix("topic/") {
            if !topic.is_empty() {
                result.topics.push(topic.to_string());
            }
        } else if let Some(emotion) = s.strip_prefix("emotion/") {
            if !emotion.is_empty() {
                result.emotions.push(emotion.to_string());
            }
        }
    }
    result
}

/// Order a tag histogram most-common-first, breaking ties on the tag
/// name so a report is stable between runs.
///
/// `MetadataStore::list_tags` reads a redb B-tree keyed by tag, so it
/// returns **lexicographic** order. Every caller that wants "top tags"
/// has to apply this: `health::report` did not, and published the first
/// ten tags alphabetically under the heading "top tags".
pub fn sort_tag_counts(counts: &mut [(String, u64)]) {
    counts.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
}

/// Compute the Jaccard similarity between two sets of entities.
/// Returns 0.0 if either set is empty. Comparison is case-insensitive.
pub fn entity_overlap(query_entities: &[String], memory_entities: &[String]) -> f32 {
    if query_entities.is_empty() || memory_entities.is_empty() {
        return 0.0;
    }

    let query_set: HashSet<String> = query_entities
        .iter()
        .flat_map(|e| crate::search::canonicalize_entity(e).into_iter().take(1))
        .collect();
    let memory_set: HashSet<String> = memory_entities
        .iter()
        .flat_map(|e| crate::search::canonicalize_entity(e).into_iter().take(1))
        .collect();

    let intersection_count = query_set.intersection(&memory_set).count();
    let union_size = query_set.union(&memory_set).count();

    if union_size == 0 {
        return 0.0;
    }

    intersection_count as f32 / union_size as f32
}

// ═══════════════════════════════════════════════════════════════════════
// Tests
// ═══════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;

    fn counts(pairs: &[(&str, u64)]) -> Vec<(String, u64)> {
        pairs.iter().map(|(n, c)| ((*n).to_string(), *c)).collect()
    }

    /// The health-report bug in miniature: `list_tags` arrives in
    /// lexicographic order, so without this the "top tags" heading names
    /// the alphabetically first tags instead of the most common ones.
    #[test]
    fn t27_sort_orders_most_common_first() {
        let mut c = counts(&[("aardvark", 1), ("zebra", 99), ("moose", 50)]);
        sort_tag_counts(&mut c);
        assert_eq!(
            c.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>(),
            ["zebra", "moose", "aardvark"]
        );
    }

    #[test]
    fn t28_ties_break_on_name_ascending() {
        let mut c = counts(&[("zebra", 7), ("aardvark", 7), ("moose", 7)]);
        sort_tag_counts(&mut c);
        assert_eq!(
            c.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>(),
            ["aardvark", "moose", "zebra"],
            "a stable order is what makes two runs of the same report comparable"
        );
    }
}
