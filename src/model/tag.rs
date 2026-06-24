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

    /// Wrap an already-validated, lowercased tag string without
    /// re-running validation. Used by the binary decoder (CS-02)
    /// where tags were validated at write time. **Not public** —
    /// only the serialization layer should call this.
    pub(crate) fn from_trusted(s: String) -> Self {
        Tag(s)
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

/// Compute the Jaccard similarity between two sets of entities.
/// Returns 0.0 if either set is empty. Comparison is case-insensitive.
pub fn entity_overlap(query_entities: &[String], memory_entities: &[String]) -> f32 {
    if query_entities.is_empty() || memory_entities.is_empty() {
        return 0.0;
    }

    let query_set: HashSet<String> = query_entities.iter().map(|e| e.to_lowercase()).collect();
    let memory_set: HashSet<String> = memory_entities.iter().map(|e| e.to_lowercase()).collect();

    let intersection_count = query_set.intersection(&memory_set).count();
    let union_size = query_set.union(&memory_set).count();

    intersection_count as f32 / union_size as f32
}
