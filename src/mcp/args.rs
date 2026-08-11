//! Strict argument parsing for the MCP store tools.
//!
//! The store handlers used to pull their fields out of the raw
//! `serde_json::Value` with `.get(k).and_then(|v| v.as_str())` and
//! `serde_json::from_value(..).ok()`, so every wrong-typed or
//! unparseable argument became a silent default: a `tags` array sent as
//! a comma-joined string stored zero tags, a `namespace` sent as an
//! array fell back to the default namespace, and an unparseable
//! `supersedes` UUID dropped the link. A client whose tool-call
//! serializer mangled its arguments could therefore write a long run of
//! quietly wrong memories without a single error reaching the agent's
//! transcript.
//!
//! This module is the boundary that makes those cases loud. It does
//! three things and delegates the fourth:
//!
//! 1. **Extraction** with explicit types ([`require_str`], [`opt_str`],
//!    [`opt_string_array`], [`opt_uuid`], [`require_uuid`]).
//! 2. **Type strictness** — a wrong-typed argument is an error naming
//!    the field and the type that arrived, never a default.
//! 3. **Markup detection** ([`scan_client_markup`]) — text fields that
//!    contain literal tool-call markup are rejected rather than stored.
//! 4. Everything about *how long* or *how many* is delegated to
//!    [`crate::model::validation::validate_memory_input`], which the
//!    HTTP API shares.
//!
//! Nothing here needs a bridge, a runtime, or storage: the composite
//! parser takes the default namespace as a `&str`, which is what makes
//! the whole boundary unit-testable with no mocks.
//!
//! # Explicit JSON `null` means "absent"
//!
//! [`field_opt`] deliberately maps both a missing key and an explicit
//! `null` to `None`. Many client SDKs serialize optional fields as
//! `null` rather than omitting them, and the previous
//! `.and_then(|v| v.as_str())` chain treated the two identically. Any
//! change to that would break those clients on every call.

use serde_json::Value;

use crate::mcp::bridge::StoreInput;
use crate::model::MemoryId;
use crate::model::validation::{MemoryInputRef, truncate_on_char_boundary, validate_memory_input};

// ═══════════════════════════════════════════════════════════════════════
// Error type
// ═══════════════════════════════════════════════════════════════════════

/// A rejected tool argument.
///
/// `Display` renders exactly `message`, so a handler can pass
/// `e.to_string()` straight to `ToolCallResult::error`, while `field`
/// feeds the per-item `field` member of the batch response. `field` is
/// the empty string for errors that are not attributable to one field
/// (such as "the item is not an object at all").
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct ArgError {
    /// camelCase wire name of the offending parameter, or `""`.
    pub field: String,
    /// Human-readable, agent-facing explanation.
    pub message: String,
}

impl ArgError {
    fn new(field: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            field: field.into(),
            message: message.into(),
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════
// Helpers
// ═══════════════════════════════════════════════════════════════════════

/// The JSON type name of `v`, as used in error messages.
fn json_type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// Truncate `s` to at most `max` UTF-8 bytes on a character boundary,
/// appending an ellipsis when anything was removed.
///
/// Used to echo a bad value back to the caller without letting a 1 MiB
/// argument turn into a 1 MiB error message.
fn elide(s: &str, max: usize) -> String {
    let kept = truncate_on_char_boundary(s, max);
    if kept.len() == s.len() {
        s.to_string()
    } else {
        format!("{kept}...")
    }
}

/// Look up `key`, treating an explicit JSON `null` exactly like an
/// absent key.
///
/// See the module docs: this equivalence is load-bearing for client
/// SDKs that serialize `Option::None` as `null`.
fn field_opt<'a>(obj: &'a Value, key: &str) -> Option<&'a Value> {
    match obj.get(key) {
        None | Some(Value::Null) => None,
        Some(v) => Some(v),
    }
}

/// The number of bytes of a bad value echoed back in an error message.
const ECHO_MAX_BYTES: usize = 64;

// ═══════════════════════════════════════════════════════════════════════
// Typed extractors
// ═══════════════════════════════════════════════════════════════════════

/// Extract a required string parameter.
///
/// A missing key and an explicit `null` produce the same "missing"
/// error; a present value of the wrong type names the type that
/// arrived.
pub fn require_str(obj: &Value, key: &str) -> Result<String, ArgError> {
    match field_opt(obj, key) {
        None => Err(ArgError::new(
            key,
            format!("Missing required parameter: {key}"),
        )),
        Some(v) => match v.as_str() {
            Some(s) => Ok(s.to_string()),
            None => Err(ArgError::new(
                key,
                format!(
                    "Parameter '{key}' must be a string (got {})",
                    json_type_name(v)
                ),
            )),
        },
    }
}

/// Extract an optional string parameter. Absent and `null` both yield
/// `None`; an empty string is a legitimate `Some("")`.
pub fn opt_str(obj: &Value, key: &str) -> Result<Option<String>, ArgError> {
    match field_opt(obj, key) {
        None => Ok(None),
        Some(v) => match v.as_str() {
            Some(s) => Ok(Some(s.to_string())),
            None => Err(ArgError::new(
                key,
                format!(
                    "Parameter '{key}' must be a string (got {})",
                    json_type_name(v)
                ),
            )),
        },
    }
}

/// Extract an optional array-of-strings parameter, defaulting to an
/// empty vector.
///
/// A bare string is *not* coerced into a one-element array: a client
/// sending `"tags": "a,b"` has been losing its tags silently, and
/// guessing here would re-establish exactly the pattern this module
/// exists to remove.
pub fn opt_string_array(obj: &Value, key: &str) -> Result<Vec<String>, ArgError> {
    let Some(v) = field_opt(obj, key) else {
        return Ok(Vec::new());
    };
    let Some(arr) = v.as_array() else {
        return Err(ArgError::new(
            key,
            format!(
                "Parameter '{key}' must be an array of strings (got {})",
                json_type_name(v)
            ),
        ));
    };
    let mut out = Vec::with_capacity(arr.len());
    for (index, item) in arr.iter().enumerate() {
        match item.as_str() {
            Some(s) => out.push(s.to_string()),
            None => {
                return Err(ArgError::new(
                    key,
                    format!(
                        "Parameter '{key}' must be an array of strings \
                         (element at index {index} is a {})",
                        json_type_name(item)
                    ),
                ));
            }
        }
    }
    Ok(out)
}

/// Extract an optional UUID parameter.
///
/// Parsing is `Uuid::parse_str`, which also accepts the hyphenless,
/// braced, and `urn:uuid:` forms. A present but unparseable value is an
/// error rather than a silently dropped link.
pub fn opt_uuid(obj: &Value, key: &str) -> Result<Option<MemoryId>, ArgError> {
    let Some(v) = field_opt(obj, key) else {
        return Ok(None);
    };
    let Some(s) = v.as_str() else {
        return Err(ArgError::new(
            key,
            format!(
                "Parameter '{key}' must be a string containing a UUID (got {})",
                json_type_name(v)
            ),
        ));
    };
    match uuid::Uuid::parse_str(s) {
        Ok(u) => Ok(Some(MemoryId::from_uuid(u))),
        Err(_) => Err(ArgError::new(
            key,
            format!(
                "Invalid UUID in parameter '{key}': \"{}\"",
                elide(s, ECHO_MAX_BYTES)
            ),
        )),
    }
}

/// Extract a required UUID parameter.
pub fn require_uuid(obj: &Value, key: &str) -> Result<MemoryId, ArgError> {
    match opt_uuid(obj, key)? {
        Some(id) => Ok(id),
        None => Err(ArgError::new(
            key,
            format!("Missing required parameter: {key}"),
        )),
    }
}

// ═══════════════════════════════════════════════════════════════════════
// Client tool-call markup detection
// ═══════════════════════════════════════════════════════════════════════

// The literals below are assembled with `concat!` rather than written
// out in one piece. Emitting the reserved vendor prefix verbatim inside
// a tool call is exactly the corruption this detector defends against,
// and it damages files written through such a call.

/// Reserved client vendor-namespace prefix in an opening tag.
const VENDOR_OPEN: &str = concat!("<", "ant", "ml", ":");
/// Reserved client vendor-namespace prefix in a closing tag.
const VENDOR_CLOSE: &str = concat!("</", "ant", "ml", ":");

/// R1 anchors. The prefix is reserved to the client's serializer and
/// appears in no legitimate corpus, so one occurrence is decisive.
const VENDOR_LITERALS: [&str; 2] = [VENDOR_OPEN, VENDOR_CLOSE];

/// R2 anchors.
///
/// `function_calls` is underscore-joined and plural — not an element in
/// any real schema — so both spellings of it are anchored. `invoke` and
/// `parameter` are anchored **only** in the vendor-prefixed form.
///
/// The bare `<invoke name=` / `<parameter name=` spellings used to be
/// here too and were a false-positive machine: TestNG suite XML is built
/// out of `<parameter name="browser" value="chrome"/>`, which is
/// commonplace in Java repositories and was rejected outright. They
/// bought nothing, either — the corruption this detector exists to catch
/// always carries the reserved vendor prefix, which R1 ([`VENDOR_LITERALS`])
/// already treats as decisive on its own.
const INVOKE_LITERALS: [&str; 6] = [
    "<function_calls>",
    "</function_calls>",
    concat!("<", "ant", "ml", ":", "function_calls>"),
    concat!("</", "ant", "ml", ":", "function_calls>"),
    concat!("<", "ant", "ml", ":", "invoke name="),
    concat!("<", "ant", "ml", ":", "parameter name="),
];

/// R3 anchors: exact open/close tags of the two camelCase parameter
/// names.
///
/// Real XML element names are kebab-case, snake_case, or PascalCase; an
/// element spelled exactly `fullText` or `parentId` in lowerCamel,
/// matching one of our own parameters character for character, is
/// essentially only producible by a serializer that turned our schema
/// into tags. The lowercase English names (`summary`, `tags`,
/// `namespace`, `supersedes`) are deliberately excluded — they get no
/// single-signal power.
const DISTINCTIVE_PARAM_TOKENS: [&str; 4] =
    ["</fullText>", "<fullText>", "</parentId>", "<parentId>"];

/// Parameter names that may participate in an R4 adjacency.
const PARAM_TAGS: [&str; 10] = [
    "emotions",
    "entities",
    "fullText",
    "memories",
    "namespace",
    "parentId",
    "summary",
    "supersedes",
    "tags",
    "topics",
];

/// Terminators that may *follow* a parameter close tag (R4).
const FOLLOWER_TERMINATORS: [&str; 3] = ["</invoke>", "</parameter>", "</function_calls>"];

/// Terminators that may *end* a text field (R5).
///
/// `</parameter>` is excluded here: DocBook makes a note ending in one
/// plausible. It participates only as an R4 follower, where a second
/// signal backs it up.
const TRAILING_TERMINATORS: [&str; 2] = ["</invoke>", "</function_calls>"];

/// Whitespace permitted between an R4 close tag and its follower.
const WS: [char; 4] = [' ', '\t', '\n', '\r'];

/// Longest window searched for a tag name after a `</`.
///
/// The longest name in [`PARAM_TAGS`] is 10 bytes; bounding the search
/// keeps the R4 pass linear on adversarial input such as a megabyte of
/// `</` with no `>` anywhere.
const TAG_NAME_SCAN_LIMIT: usize = 32;

/// Which detection rule fired.
///
/// The variants are numbered R1-R5 in the design note in that
/// declaration order. Evaluation order is *not* the same: R2 is tested
/// first because every vendor-prefixed invoke literal also contains a
/// vendor-namespace literal, and reporting the longer, more specific
/// token is strictly more useful to whoever has to debug the client.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarkupRule {
    /// R1 — the reserved client vendor-namespace prefix in tag position.
    VendorNamespace,
    /// R2 — a `function_calls` literal, or a vendor-prefixed
    /// attribute-first `<invoke name=` / `<parameter name=`.
    InvokeAttribute,
    /// R3 — an exact open/close tag of a camelCase parameter name.
    DistinctiveParamTag,
    /// R4 — a parameter close tag directly followed by another
    /// parameter tag or by a call terminator.
    TagAdjacency,
    /// R5 — the text ends with a call terminator.
    TrailingTerminator,
}

/// Where and why the detector fired.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarkupFinding {
    /// The rule that matched.
    pub rule: MarkupRule,
    /// The literal that matched, echoed into the error message.
    pub token: String,
    /// Byte offset of `token` within the scanned text.
    pub offset: usize,
}

/// Advice appended to every markup rejection.
const MARKUP_ADVICE: &str = "Arguments appear to have been serialized incorrectly; nothing was \
     stored. Re-send with structured JSON arguments and no XML tool-call tags in text fields.";

/// Find the earliest occurrence of any literal in `literals`.
///
/// Ties on offset are broken toward the longer literal, so a
/// vendor-prefixed form is reported in full rather than as its shorter
/// prefix.
fn first_literal_match(text: &str, literals: &[&'static str]) -> Option<(usize, &'static str)> {
    literals
        .iter()
        .filter_map(|lit| text.find(lit).map(|at| (at, *lit)))
        .min_by(|a, b| a.0.cmp(&b.0).then(b.1.len().cmp(&a.1.len())))
}

/// Run one literal-table rule.
fn scan_literals(text: &str, literals: &[&'static str], rule: MarkupRule) -> Option<MarkupFinding> {
    first_literal_match(text, literals).map(|(offset, token)| MarkupFinding {
        rule,
        token: token.to_string(),
        offset,
    })
}

/// True when `s` begins with an opening tag whose name is a parameter
/// name, i.e. `<tags>` or `<tags attr=...`.
fn starts_with_param_open(s: &str) -> bool {
    let Some(rest) = s.strip_prefix('<') else {
        return false;
    };
    PARAM_TAGS.iter().any(|tag| {
        rest.strip_prefix(tag)
            .is_some_and(|after| after.starts_with('>') || after.starts_with(' '))
    })
}

/// R4 — a `</param>` close tag followed, across whitespace only, by
/// another parameter tag or by a call terminator.
///
/// Two independent signals, which is what lets generic names such as
/// `summary` participate without firing on prose. Real documentation
/// quoting `<details><summary>…</summary>` continues with `<p>`,
/// `</details>`, or text — never with a second recalld parameter name
/// separated by nothing but whitespace.
fn scan_tag_adjacency(text: &str) -> Option<MarkupFinding> {
    for (start, _) in text.match_indices("</") {
        let rest = &text[start + 2..];
        let window = truncate_on_char_boundary(rest, TAG_NAME_SCAN_LIMIT);
        let Some(gt) = window.find('>') else {
            continue;
        };
        let name = &window[..gt];
        if !PARAM_TAGS.contains(&name) {
            continue;
        }
        let follower = rest[gt + 1..].trim_start_matches(WS);
        let adjacent = FOLLOWER_TERMINATORS.iter().any(|t| follower.starts_with(t))
            || starts_with_param_open(follower);
        if adjacent {
            return Some(MarkupFinding {
                rule: MarkupRule::TagAdjacency,
                token: format!("</{name}>"),
                offset: start,
            });
        }
    }
    None
}

/// R5 — the text ends with a call terminator.
///
/// This catches truncated corruption where the parameter close tag was
/// consumed but the invoke terminator survived. It carries the highest
/// false-positive risk of the five rules (a note ending in a quoted
/// BPEL snippet fires), which is accepted deliberately: the message
/// names the token and offset, and this is the first rule to drop if a
/// real user is ever bitten.
fn scan_trailing_terminator(text: &str) -> Option<MarkupFinding> {
    let trimmed = text.trim_end();
    TRAILING_TERMINATORS
        .iter()
        .find(|t| trimmed.ends_with(**t))
        .map(|t| MarkupFinding {
            rule: MarkupRule::TrailingTerminator,
            token: (*t).to_string(),
            offset: trimmed.len() - t.len(),
        })
}

/// Scan `text` for literal client tool-call markup.
///
/// Returns the first rule that fires, evaluated most-specific-first
/// (see [`MarkupRule`]). Every rule is a linear pass over fixed
/// literals, so the whole scan is linear in the length of `text` and is
/// dwarfed by the embedding generation that follows a successful store.
pub fn scan_client_markup(text: &str) -> Option<MarkupFinding> {
    scan_literals(text, &INVOKE_LITERALS, MarkupRule::InvokeAttribute)
        .or_else(|| scan_literals(text, &VENDOR_LITERALS, MarkupRule::VendorNamespace))
        .or_else(|| {
            scan_literals(
                text,
                &DISTINCTIVE_PARAM_TOKENS,
                MarkupRule::DistinctiveParamTag,
            )
        })
        .or_else(|| scan_tag_adjacency(text))
        .or_else(|| scan_trailing_terminator(text))
}

/// Reject `text` if it contains literal client tool-call markup.
///
/// The whole call is refused rather than sanitised: stripping the tags
/// out cannot recover the fields that never arrived (in the reported
/// incident `tags` and `entities` were absent from the call entirely),
/// and a sanitised write becomes a permanent record whose embedding,
/// full-text index, and autolink edges were all computed from
/// garbage-adjacent text. A rejection costs one round trip and lands in
/// the agent's transcript, where a well-behaved client retries.
pub fn reject_client_markup(field: &str, text: &str) -> Result<(), ArgError> {
    match scan_client_markup(text) {
        None => Ok(()),
        Some(f) => Err(ArgError::new(
            field,
            format!(
                "Parameter '{field}' contains literal tool-call markup \
                 (\"{}\" at byte offset {}). {MARKUP_ADVICE}",
                f.token, f.offset
            ),
        )),
    }
}

// ═══════════════════════════════════════════════════════════════════════
// Composite parser
// ═══════════════════════════════════════════════════════════════════════

/// Parse the arguments of a `store_memory` call (or one item of a
/// `store_memories` batch) into a [`StoreInput`].
///
/// `default_namespace` is used only when the caller omits `namespace`;
/// a *wrong-typed* `namespace` is an error, never a fallback.
///
/// Checks run in a fixed order so that an argument object broken in
/// several ways always reports the same failure: object-ness, then
/// `summary` (presence, type, markup), `fullText` (type, markup), the
/// four array fields, `namespace`, `parentId`, `supersedes`, and
/// finally the shared content limits from
/// [`validate_memory_input`](crate::model::validation::validate_memory_input).
pub fn parse_store_input(item: &Value, default_namespace: &str) -> Result<StoreInput, ArgError> {
    if !item.is_object() {
        return Err(ArgError::new(
            "",
            format!("Item must be an object (got {})", json_type_name(item)),
        ));
    }

    let summary = require_str(item, "summary")?;
    reject_client_markup("summary", &summary)?;

    let full_text = opt_str(item, "fullText")?;
    if let Some(ft) = full_text.as_deref() {
        reject_client_markup("fullText", ft)?;
    }

    let tags = opt_string_array(item, "tags")?;
    let entities = opt_string_array(item, "entities")?;
    let topics = opt_string_array(item, "topics")?;
    let emotions = opt_string_array(item, "emotions")?;

    let namespace = opt_str(item, "namespace")?.unwrap_or_else(|| default_namespace.to_string());

    let parent_id = opt_uuid(item, "parentId")?;
    let supersedes = opt_uuid(item, "supersedes")?;

    validate_memory_input(MemoryInputRef {
        summary: &summary,
        full_text: full_text.as_deref(),
        tags: &tags,
        entities: &entities,
        topics: &topics,
        emotions: &emotions,
    })
    .map_err(|e| ArgError::new(e.field().unwrap_or(""), e.to_string()))?;

    Ok(StoreInput {
        summary,
        full_text,
        tags,
        entities,
        topics,
        emotions,
        namespace,
        embedding: None,
        initial_stability: None,
        parent_id,
        supersedes,
    })
}

/// Parse one item of a `store_memories` batch.
///
/// Identical to [`parse_store_input`] except that an item which is not
/// an object names its position, which is the only context the caller
/// cannot recover from the message alone.
pub fn parse_store_item(
    item: &Value,
    index: usize,
    default_namespace: &str,
) -> Result<StoreInput, ArgError> {
    if !item.is_object() {
        return Err(ArgError::new(
            "",
            format!(
                "Item at index {index} must be an object (got {})",
                json_type_name(item)
            ),
        ));
    }
    parse_store_input(item, default_namespace)
}

// ═══════════════════════════════════════════════════════════════════════
// Tests
// ═══════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ── Helpers ──────────────────────────────────────────────────────

    /// Assert that `text` is rejected by exactly `rule`, with exactly
    /// `token` at exactly `offset`.
    fn assert_rejected(text: &str, rule: MarkupRule, token: &str, offset: usize) {
        let found = scan_client_markup(text)
            .unwrap_or_else(|| panic!("expected a rejection, got none for {text:?}"));
        assert_eq!(found.rule, rule, "rule for {text:?}");
        assert_eq!(found.token, token, "token for {text:?}");
        assert_eq!(found.offset, offset, "offset for {text:?}");
        assert_eq!(
            &text[found.offset..found.offset + found.token.len()],
            found.token,
            "offset does not point at the token for {text:?}"
        );
    }

    /// Assert that `text` is legitimate technical prose.
    fn assert_accepted(text: &str) {
        assert_eq!(
            scan_client_markup(text),
            None,
            "false positive on legitimate text: {text:?}"
        );
    }

    /// The corrupted `fullText` tail from the incident report.
    fn report_artifact() -> String {
        "Reviewed the storage layer and wrote up the design notes</fullText>\n\
         <entities>[\"recalld\"]</entities>\n\
         <tags>[\"topic/rust\"]</tags>\n\
         </invoke>\n"
            .to_string()
    }

    // ═══════════════════════════════════════════════════════════════
    // A. Markup detector MUST REJECT
    // ═══════════════════════════════════════════════════════════════

    #[test]
    fn a1_report_artifact_tail_is_rejected() {
        let text = report_artifact();
        // R3 wins over the R4 adjacency and the R5 terminator that the
        // same string also contains.
        assert_rejected(
            &text,
            MarkupRule::DistinctiveParamTag,
            "</fullText>",
            text.find("</fullText>").unwrap(),
        );
    }

    #[test]
    fn a2_vendor_prefix_inside_a_sentence_is_rejected() {
        let text = format!("The client spliced {VENDOR_OPEN}invoke tags into my note.");
        assert_rejected(
            &text,
            MarkupRule::VendorNamespace,
            VENDOR_OPEN,
            text.find(VENDOR_OPEN).unwrap(),
        );
    }

    #[test]
    fn a3_vendor_prefixed_parameter_open_tag_is_rejected() {
        let text = format!("{VENDOR_OPEN}parameter>value");
        assert_rejected(&text, MarkupRule::VendorNamespace, VENDOR_OPEN, 0);
    }

    #[test]
    fn a4_vendor_prefixed_invoke_attribute_is_rejected() {
        let literal = concat!("<", "ant", "ml", ":", "invoke name=");
        let text = format!("{literal}\"store_memory\">");
        assert_rejected(&text, MarkupRule::InvokeAttribute, literal, 0);
    }

    #[test]
    fn a5_vendor_prefixed_parameter_attribute_is_rejected() {
        let literal = concat!("<", "ant", "ml", ":", "parameter name=");
        let text = format!("{literal}\"fullText\">");
        assert_rejected(&text, MarkupRule::InvokeAttribute, literal, 0);
    }

    #[test]
    fn a6_vendor_prefixed_function_calls_open_and_close_are_rejected() {
        let open = concat!("<", "ant", "ml", ":", "function_calls>");
        let close = concat!("</", "ant", "ml", ":", "function_calls>");
        assert_rejected(open, MarkupRule::InvokeAttribute, open, 0);
        let text = format!("note body{close}");
        assert_rejected(
            &text,
            MarkupRule::InvokeAttribute,
            close,
            text.find(close).unwrap(),
        );
    }

    #[test]
    fn a6b_bare_function_calls_tags_are_rejected() {
        // Underscore-joined and plural: not an element in any real
        // schema, so the unprefixed spelling is anchored too.
        assert_rejected(
            "<function_calls>",
            MarkupRule::InvokeAttribute,
            "<function_calls>",
            0,
        );
        assert_rejected(
            "body</function_calls>",
            MarkupRule::InvokeAttribute,
            "</function_calls>",
            4,
        );
    }

    #[test]
    fn a6c_bare_attribute_first_invoke_still_falls_to_another_rule_when_corrupt() {
        // The bare spellings are no longer anchors of their own (see
        // `b17`/`b18`). Real corruption still trips a rule, because the
        // parameter tags it splices in are what the later rules are for.
        let text =
            "<invoke name=\"store_memory\">\n<parameter name=\"summary\">x</summary>\n</invoke>";
        assert_rejected(
            text,
            MarkupRule::TagAdjacency,
            "</summary>",
            text.find("</summary>").unwrap(),
        );
    }

    #[test]
    fn a7_bare_full_text_close_tag_is_rejected() {
        assert_rejected(
            "body</fullText>",
            MarkupRule::DistinctiveParamTag,
            "</fullText>",
            4,
        );
    }

    #[test]
    fn a8_bare_parent_id_close_tag_is_rejected() {
        assert_rejected(
            "x</parentId>y",
            MarkupRule::DistinctiveParamTag,
            "</parentId>",
            1,
        );
    }

    #[test]
    fn a8b_bare_camel_case_open_tags_are_rejected() {
        assert_rejected(
            "a<fullText>b",
            MarkupRule::DistinctiveParamTag,
            "<fullText>",
            1,
        );
        assert_rejected(
            "a<parentId>b",
            MarkupRule::DistinctiveParamTag,
            "<parentId>",
            1,
        );
    }

    #[test]
    fn a9_two_generic_param_tags_in_adjacency_are_rejected() {
        assert_rejected(
            "</summary>\n<tags>[]</tags>",
            MarkupRule::TagAdjacency,
            "</summary>",
            0,
        );
    }

    #[test]
    fn a10_param_close_then_whitespace_run_then_terminator_is_rejected() {
        assert_rejected(
            "</tags>\n\n   </invoke>",
            MarkupRule::TagAdjacency,
            "</tags>",
            0,
        );
    }

    #[test]
    fn a11_trailing_terminator_with_trailing_whitespace_is_rejected() {
        let text = "Notes on the storage layer rewrite.\n</invoke>\n\n";
        assert_rejected(
            text,
            MarkupRule::TrailingTerminator,
            "</invoke>",
            text.find("</invoke>").unwrap(),
        );
    }

    #[test]
    fn a12_generic_to_generic_adjacency_with_single_space_is_rejected() {
        assert_rejected(
            "</namespace> <supersedes>",
            MarkupRule::TagAdjacency,
            "</namespace>",
            0,
        );
    }

    #[test]
    fn a13_adjacency_follower_may_carry_attributes() {
        // `<summary type="text">` as a follower still counts: the
        // second signal is the parameter name, not the tag's shape.
        assert_rejected(
            "</entities>\n<summary type=\"text\">x</summary>",
            MarkupRule::TagAdjacency,
            "</entities>",
            0,
        );
    }

    // ═══════════════════════════════════════════════════════════════
    // B. Markup detector MUST ACCEPT (false-positive proof)
    // ═══════════════════════════════════════════════════════════════

    #[test]
    fn b1_csharp_doc_comment_is_accepted() {
        assert_accepted(
            "/// <summary>Returns the widget count.</summary>\n/// <returns>int</returns>",
        );
    }

    #[test]
    fn b2_html_details_summary_is_accepted() {
        assert_accepted("<details><summary>Stack trace</summary><pre>panic at...</pre></details>");
    }

    #[test]
    fn b3_atom_entry_is_accepted() {
        assert_accepted("<entry><title>Post</title><summary type=\"text\">Blurb</summary></entry>");
    }

    #[test]
    fn b4_docbook_parameter_element_is_accepted() {
        assert_accepted("The <parameter>timeout</parameter> element controls retry backoff.");
    }

    #[test]
    fn b5_bpel_invoke_mid_prose_is_accepted() {
        assert_accepted(
            "Legacy BPEL wrapped calls in <invoke partnerLink=\"svc\" operation=\"go\"/> \
             ... </invoke> inside a scope, which we replaced with gRPC.",
        );
    }

    #[test]
    fn b6_jsx_fragment_is_accepted() {
        assert_accepted("return <Fragment><TagList tags={tags} /></Fragment>;");
    }

    #[test]
    fn b7_self_referential_prose_about_these_very_parameters_is_accepted() {
        // The normal way to discuss the store schema. If this ever
        // fires, the detector is unusable for a memory store whose
        // users are engineers working on the memory store.
        assert_accepted(
            "The MCP store_memory tool takes {\"summary\": \"...\", \"fullText\": \"...\", \
             \"tags\": []}. A client bug spliced parameter markup into fullText, so tags \
             arrived empty.",
        );
    }

    #[test]
    fn b8_xml_config_with_a_namespace_element_is_accepted() {
        // `</namespace>` followed by `<name>` — adjacent tags, but the
        // follower is not one of our parameter names.
        assert_accepted("<namespace>prod</namespace>\n<name>ingest-svc</name>");
    }

    #[test]
    fn b9_rust_generics_and_comparisons_are_accepted() {
        assert_accepted("Vec<String>, HashMap<K, V>, impl Iterator<Item = &'a str>");
        assert_accepted("the guard is a < b && c > d, which held under load");
    }

    #[test]
    fn b10_maven_dependency_block_is_accepted() {
        assert_accepted("<dependency><groupId>x</groupId><artifactId>y</artifactId></dependency>");
    }

    #[test]
    fn b11_shell_redirection_is_accepted() {
        assert_accepted("cmd <input.txt >output.txt 2>&1");
    }

    #[test]
    fn b12_adjacency_whose_follower_is_not_a_param_tag_is_accepted() {
        assert_accepted("</summary></details>");
    }

    #[test]
    fn b13_plain_word_usage_is_accepted() {
        assert_accepted(
            "We invoke the parameter validator; the new note supersedes the old summary.",
        );
    }

    #[test]
    fn b14_generic_close_newline_non_param_follower_is_accepted() {
        assert_accepted("<summary>Overview</summary>\n<p>Body text</p>");
    }

    #[test]
    fn b15_empty_and_one_mib_inputs_are_accepted_quickly() {
        assert_accepted("");
        // A megabyte of markup-shaped prose. The rules are linear, so
        // this returns immediately; a quadratic scan would hang the
        // test suite, which is the point of the case.
        let para = "<p>Lorem ipsum dolor sit <em>amet</em>, consectetur adipiscing elit, \
                    sed do eiusmod tempor incididunt ut labore.</p>\n";
        let mut big = String::with_capacity(1_100_000);
        while big.len() < 1_048_576 {
            big.push_str(para);
        }
        assert!(big.len() >= 1_048_576);
        // Not `assert_accepted`: a failure there would dump a megabyte.
        assert!(scan_client_markup(&big).is_none());
    }

    #[test]
    fn b17_testng_suite_xml_is_accepted() {
        // The reported false positive. `<parameter name=... value=.../>` is
        // how every TestNG suite passes data to a test, so a bare
        // `<parameter name=` anchor rejects a whole class of legitimate
        // Java notes.
        assert_accepted(
            "<!DOCTYPE suite SYSTEM \"https://testng.org/testng-1.0.dtd\">\n\
             <suite name=\"Regression\" parallel=\"tests\">\n\
             \x20 <parameter name=\"browser\" value=\"chrome\"/>\n\
             \x20 <test name=\"smoke\">\n\
             \x20   <parameter name=\"baseUrl\" value=\"https://staging.example.com\"/>\n\
             \x20   <classes><class name=\"com.example.SmokeTest\"/></classes>\n\
             \x20 </test>\n\
             </suite>",
        );
    }

    #[test]
    fn b18_maven_and_spring_parameter_elements_are_accepted() {
        // Attribute-first `<parameter name=` with a text body, as Maven
        // plugin docs and Spring XML both spell it.
        assert_accepted(
            "The failsafe config sets <parameter name=\"timeout\">30</parameter> and Spring's \
             <bean class=\"Job\"><parameter name=\"retries\">3</parameter></bean> mirrors it.",
        );
    }

    #[test]
    fn b19_bare_invoke_with_a_name_attribute_is_accepted() {
        // A `<invoke name=...>` in prose is now judged on its context like
        // any other tag: nothing else here looks like a tool call.
        assert_accepted(
            "The old workflow engine used <invoke name=\"chargeCard\" retries=\"2\"/> for \
             every step, which we replaced with an idempotency key.",
        );
    }

    #[test]
    fn b16_param_close_far_from_its_follower_is_accepted() {
        // Whitespace only is the rule; intervening prose breaks it.
        assert_accepted("</summary> and then the <tags> element follows much later");
    }

    // ═══════════════════════════════════════════════════════════════
    // B'. Documented, deliberately accepted false positives
    // ═══════════════════════════════════════════════════════════════

    #[test]
    fn b_prime_1_note_ending_in_a_quoted_bpel_terminator_is_rejected() {
        // THE ONE KNOWINGLY-ACCEPTED FALSE POSITIVE. A note whose final
        // line happens to be a quoted `</invoke>` fires R5. R5 exists
        // to catch truncated corruption where the parameter close tag
        // was consumed but the terminator survived; that is worth more
        // than this rare shape, and R5 is the first rule to drop if a
        // real user is ever bitten. The message names the token and its
        // offset so the caller can see exactly why.
        let text = "The BPEL scope ended with:\n</invoke>";
        assert_rejected(
            text,
            MarkupRule::TrailingTerminator,
            "</invoke>",
            text.find("</invoke>").unwrap(),
        );
    }

    #[test]
    fn b_prime_2_generic_tags_never_fire_alone() {
        // Locks in the rule that a lowercase English name in tag
        // position is never a single sufficient signal.
        assert_accepted("<summary>x</summary>\n<remarks>y</remarks>");
    }

    // ═══════════════════════════════════════════════════════════════
    // C. Extractors
    // ═══════════════════════════════════════════════════════════════

    #[test]
    fn c_opt_string_array_absent_null_and_empty_all_yield_empty() {
        assert_eq!(
            opt_string_array(&json!({}), "tags").unwrap(),
            Vec::<String>::new()
        );
        assert_eq!(
            opt_string_array(&json!({ "tags": null }), "tags").unwrap(),
            Vec::<String>::new()
        );
        assert_eq!(
            opt_string_array(&json!({ "tags": [] }), "tags").unwrap(),
            Vec::<String>::new()
        );
    }

    #[test]
    fn c_opt_string_array_accepts_strings() {
        assert_eq!(
            opt_string_array(&json!({ "tags": ["a", "b"] }), "tags").unwrap(),
            vec!["a".to_string(), "b".to_string()]
        );
    }

    #[test]
    fn c_opt_string_array_rejects_a_bare_string() {
        // The exact shape that silently stored zero tags before.
        let err = opt_string_array(&json!({ "tags": "a,b" }), "tags").unwrap_err();
        assert_eq!(err.field, "tags");
        assert_eq!(
            err.message,
            "Parameter 'tags' must be an array of strings (got string)"
        );
    }

    #[test]
    fn c_opt_string_array_rejects_an_object() {
        let err = opt_string_array(&json!({ "tags": { "0": "a" } }), "tags").unwrap_err();
        assert_eq!(
            err.message,
            "Parameter 'tags' must be an array of strings (got object)"
        );
    }

    #[test]
    fn c_opt_string_array_names_the_bad_element_index() {
        let err = opt_string_array(&json!({ "tags": ["a", 3] }), "tags").unwrap_err();
        assert_eq!(
            err.message,
            "Parameter 'tags' must be an array of strings (element at index 1 is a number)"
        );

        let err = opt_string_array(&json!({ "tags": [null] }), "tags").unwrap_err();
        assert_eq!(
            err.message,
            "Parameter 'tags' must be an array of strings (element at index 0 is a null)"
        );

        let err = opt_string_array(&json!({ "tags": ["a", "b", 3] }), "tags").unwrap_err();
        assert!(
            err.message.contains("element at index 2 is a number"),
            "{err}"
        );
    }

    #[test]
    fn c_opt_str_treats_null_as_absent_and_keeps_empty_strings() {
        assert_eq!(opt_str(&json!({}), "fullText").unwrap(), None);
        assert_eq!(
            opt_str(&json!({ "fullText": null }), "fullText").unwrap(),
            None
        );
        assert_eq!(
            opt_str(&json!({ "fullText": "" }), "fullText").unwrap(),
            Some(String::new())
        );
    }

    #[test]
    fn c_opt_str_rejects_wrong_types() {
        let err = opt_str(&json!({ "fullText": 5 }), "fullText").unwrap_err();
        assert_eq!(
            err.message,
            "Parameter 'fullText' must be a string (got number)"
        );
        let err = opt_str(&json!({ "fullText": ["x"] }), "fullText").unwrap_err();
        assert_eq!(
            err.message,
            "Parameter 'fullText' must be a string (got array)"
        );
        assert_eq!(err.field, "fullText");
    }

    #[test]
    fn c_require_str_treats_null_as_missing() {
        let absent = require_str(&json!({}), "summary").unwrap_err();
        assert_eq!(absent.message, "Missing required parameter: summary");
        let null = require_str(&json!({ "summary": null }), "summary").unwrap_err();
        assert_eq!(null, absent);
    }

    #[test]
    fn c_require_str_reports_wrong_types() {
        let err = require_str(&json!({ "summary": 5 }), "summary").unwrap_err();
        assert_eq!(
            err.message,
            "Parameter 'summary' must be a string (got number)"
        );
    }

    #[test]
    fn c_opt_uuid_absent_and_null_yield_none() {
        assert_eq!(opt_uuid(&json!({}), "supersedes").unwrap(), None);
        assert_eq!(
            opt_uuid(&json!({ "supersedes": null }), "supersedes").unwrap(),
            None
        );
    }

    #[test]
    fn c_opt_uuid_accepts_the_lenient_forms_uuid_parse_str_allows() {
        let canonical = "a1b2c3d4-e5f6-7890-abcd-ef1234567890";
        let expected = uuid::Uuid::parse_str(canonical).unwrap();
        // Documents that `Uuid::parse_str` is lenient: all four
        // spellings are accepted and parse to the same value.
        for form in [
            canonical.to_string(),
            canonical.replace('-', ""),
            format!("{{{canonical}}}"),
            format!("urn:uuid:{canonical}"),
        ] {
            let got = opt_uuid(&json!({ "supersedes": form }), "supersedes")
                .unwrap_or_else(|e| panic!("{form} rejected: {e}"))
                .unwrap_or_else(|| panic!("{form} yielded None"));
            assert_eq!(got.into_inner(), expected, "{form}");
        }
    }

    #[test]
    fn c_opt_uuid_rejects_unparseable_and_wrong_typed_values() {
        let err = opt_uuid(&json!({ "supersedes": "mem-1234" }), "supersedes").unwrap_err();
        assert_eq!(err.field, "supersedes");
        assert_eq!(
            err.message,
            "Invalid UUID in parameter 'supersedes': \"mem-1234\""
        );

        let err = opt_uuid(&json!({ "supersedes": 123 }), "supersedes").unwrap_err();
        assert_eq!(
            err.message,
            "Parameter 'supersedes' must be a string containing a UUID (got number)"
        );

        let err = opt_uuid(&json!({ "supersedes": "" }), "supersedes").unwrap_err();
        assert!(
            err.message.starts_with("Invalid UUID in parameter"),
            "{err}"
        );
    }

    #[test]
    fn c_opt_uuid_elides_a_long_bad_value() {
        let long = "z".repeat(200);
        let err = opt_uuid(&json!({ "parentId": long }), "parentId").unwrap_err();
        assert!(err.message.ends_with("...\""), "{err}");
        assert!(err.message.len() < 140, "{err}");
    }

    #[test]
    fn c_require_uuid_reports_missing_and_bad_values() {
        let err = require_uuid(&json!({}), "id").unwrap_err();
        assert_eq!(err.message, "Missing required parameter: id");
        let err = require_uuid(&json!({ "id": null }), "id").unwrap_err();
        assert_eq!(err.message, "Missing required parameter: id");
        let err = require_uuid(&json!({ "id": "nope" }), "id").unwrap_err();
        assert!(err.message.starts_with("Invalid UUID"), "{err}");
        let id = "a1b2c3d4-e5f6-7890-abcd-ef1234567890";
        assert_eq!(
            require_uuid(&json!({ "id": id }), "id")
                .unwrap()
                .into_inner(),
            uuid::Uuid::parse_str(id).unwrap()
        );
    }

    #[test]
    fn c_elide_never_splits_a_character() {
        let text = "—".repeat(40); // 120 bytes, 3 bytes per character
        let out = elide(&text, 10);
        assert_eq!(out, "———...");
        assert!(elide("short", 64).ends_with('t'));
        assert_eq!(elide("short", 64), "short");
        assert_eq!(elide("", 4), "");
    }

    #[test]
    fn c_json_type_names() {
        assert_eq!(json_type_name(&json!(null)), "null");
        assert_eq!(json_type_name(&json!(true)), "boolean");
        assert_eq!(json_type_name(&json!(1)), "number");
        assert_eq!(json_type_name(&json!("s")), "string");
        assert_eq!(json_type_name(&json!([])), "array");
        assert_eq!(json_type_name(&json!({})), "object");
    }

    // ═══════════════════════════════════════════════════════════════
    // D. parse_store_input
    // ═══════════════════════════════════════════════════════════════

    const NS: &str = "work";

    #[test]
    fn d1_happy_path_populates_every_field() {
        let parent = "a1b2c3d4-e5f6-7890-abcd-ef1234567890";
        let older = "b2c3d4e5-f6a7-8901-bcde-f12345678901";
        let item = json!({
            "summary": "Storage layer rewritten",
            "fullText": "We replaced the four-file engine with a per-namespace layout.",
            "tags": ["type/decision", "tech/rust"],
            "entities": ["recalld"],
            "topics": ["storage"],
            "emotions": ["confident"],
            "namespace": "engineering",
            "parentId": parent,
            "supersedes": older,
        });
        let input = parse_store_input(&item, NS).unwrap();
        assert_eq!(input.summary, "Storage layer rewritten");
        assert_eq!(
            input.full_text.as_deref(),
            Some("We replaced the four-file engine with a per-namespace layout.")
        );
        assert_eq!(input.tags, vec!["type/decision", "tech/rust"]);
        assert_eq!(input.entities, vec!["recalld"]);
        assert_eq!(input.topics, vec!["storage"]);
        assert_eq!(input.emotions, vec!["confident"]);
        assert_eq!(input.namespace, "engineering");
        assert_eq!(
            input.parent_id.map(|id| id.into_inner()),
            Some(uuid::Uuid::parse_str(parent).unwrap())
        );
        assert_eq!(
            input.supersedes.map(|id| id.into_inner()),
            Some(uuid::Uuid::parse_str(older).unwrap())
        );
        assert!(input.embedding.is_none());
        assert!(input.initial_stability.is_none());
    }

    #[test]
    fn d2_minimal_input_uses_the_default_namespace_argument() {
        let input = parse_store_input(&json!({ "summary": "just this" }), NS).unwrap();
        assert_eq!(input.namespace, NS);
        assert!(input.full_text.is_none());
        assert!(input.tags.is_empty());
        assert!(input.entities.is_empty());
        assert!(input.topics.is_empty());
        assert!(input.emotions.is_empty());
        assert!(input.parent_id.is_none());
        assert!(input.supersedes.is_none());
    }

    #[test]
    fn d2b_explicit_nulls_behave_exactly_like_absent_keys() {
        // THE highest-risk compatibility detail: SDKs that serialize
        // `None` as `null` must keep working.
        let item = json!({
            "summary": "just this",
            "fullText": null,
            "tags": null,
            "entities": null,
            "topics": null,
            "emotions": null,
            "namespace": null,
            "parentId": null,
            "supersedes": null,
        });
        let from_nulls = parse_store_input(&item, NS).unwrap();
        let from_absent = parse_store_input(&json!({ "summary": "just this" }), NS).unwrap();
        assert_eq!(from_nulls.namespace, from_absent.namespace);
        assert_eq!(from_nulls.namespace, NS);
        assert_eq!(from_nulls.full_text, from_absent.full_text);
        assert_eq!(from_nulls.tags, from_absent.tags);
        assert_eq!(from_nulls.entities, from_absent.entities);
        assert_eq!(from_nulls.topics, from_absent.topics);
        assert_eq!(from_nulls.emotions, from_absent.emotions);
        assert_eq!(from_nulls.parent_id, from_absent.parent_id);
        assert_eq!(from_nulls.supersedes, from_absent.supersedes);
    }

    #[test]
    fn d3_wrong_typed_namespace_fails_instead_of_falling_back() {
        // Reproduces "namespace silently wrong": the old code took the
        // default here and stored the memory in the wrong partition.
        let err =
            parse_store_input(&json!({ "summary": "s", "namespace": ["a"] }), NS).unwrap_err();
        assert_eq!(err.field, "namespace");
        assert_eq!(
            err.message,
            "Parameter 'namespace' must be a string (got array)"
        );
        assert!(
            !err.message.contains(NS),
            "the default namespace must not appear in the rejection: {err}"
        );
    }

    #[test]
    fn d4_wrong_typed_tags_fail_instead_of_vanishing() {
        // Reproduces "tags empty".
        let err =
            parse_store_input(&json!({ "summary": "s", "tags": "topic/rust" }), NS).unwrap_err();
        assert_eq!(err.field, "tags");
        assert!(err.message.contains("must be an array of strings"), "{err}");
    }

    #[test]
    fn d5_unparseable_link_ids_fail_instead_of_being_dropped() {
        // Reproduces "supersedes ignored": previously the memory was
        // stored with no link at all.
        for field in ["supersedes", "parentId"] {
            let item = json!({ "summary": "s", field: "not-a-uuid" });
            let err = parse_store_input(&item, NS).unwrap_err();
            assert_eq!(err.field, field);
            assert_eq!(
                err.message,
                format!("Invalid UUID in parameter '{field}': \"not-a-uuid\"")
            );
        }
    }

    #[test]
    fn d6_markup_in_full_text_is_rejected_with_the_full_message() {
        let item = json!({ "summary": "Design notes", "fullText": report_artifact() });
        let err = parse_store_input(&item, NS).unwrap_err();
        assert_eq!(err.field, "fullText");
        let offset = report_artifact().find("</fullText>").unwrap();
        assert_eq!(
            err.message,
            format!(
                "Parameter 'fullText' contains literal tool-call markup \
                 (\"</fullText>\" at byte offset {offset}). {MARKUP_ADVICE}"
            )
        );
        assert!(err.message.contains("nothing was stored"), "{err}");
    }

    #[test]
    fn d6b_markup_in_summary_is_rejected() {
        let item = json!({ "summary": "notes</summary>\n<tags>[]</tags>" });
        let err = parse_store_input(&item, NS).unwrap_err();
        assert_eq!(err.field, "summary");
        assert!(
            err.message
                .starts_with("Parameter 'summary' contains literal tool-call markup")
        );
    }

    #[test]
    fn d6c_legitimate_technical_prose_still_stores() {
        let item = json!({
            "summary": "C# doc comments use <summary> elements",
            "fullText": "/// <summary>Returns the widget count.</summary>\n\
                         /// <returns>int</returns>\nThe DocBook <parameter>timeout</parameter> \
                         element is unrelated.",
        });
        assert!(parse_store_input(&item, NS).is_ok());
    }

    #[test]
    fn d7_failure_precedence_is_deterministic() {
        // Broken in five ways at once; peel them off one at a time and
        // the reported field walks the documented order.
        let mut item = json!({
            "fullText": ["x"],
            "tags": "a,b",
            "namespace": 7,
            "supersedes": "mem-1",
        });
        // summary missing -> summary wins over everything else.
        assert_eq!(parse_store_input(&item, NS).unwrap_err().field, "summary");

        item["summary"] = json!("ok");
        assert_eq!(parse_store_input(&item, NS).unwrap_err().field, "fullText");

        item["fullText"] = json!("fine");
        assert_eq!(parse_store_input(&item, NS).unwrap_err().field, "tags");

        item["tags"] = json!([]);
        assert_eq!(parse_store_input(&item, NS).unwrap_err().field, "namespace");

        item["namespace"] = json!("work");
        assert_eq!(
            parse_store_input(&item, NS).unwrap_err().field,
            "supersedes"
        );

        item["supersedes"] = json!(null);
        assert!(parse_store_input(&item, NS).is_ok());
    }

    #[test]
    fn d7b_markup_is_checked_before_the_length_limit() {
        // A 3000-byte summary that also carries markup reports the
        // markup: the type/markup boundary runs before the shared
        // content limits.
        let summary = format!("{}</summary>\n<tags>[]</tags>", "a".repeat(3000));
        let err = parse_store_input(&json!({ "summary": summary }), NS).unwrap_err();
        assert!(err.message.contains("tool-call markup"), "{err}");
    }

    #[test]
    fn d8_content_limits_still_fire_with_byte_based_messages() {
        // summary over the byte limit
        let err = parse_store_input(&json!({ "summary": "a".repeat(2001) }), NS).unwrap_err();
        assert_eq!(err.field, "summary");
        assert!(err.message.contains("2001 bytes"), "{err}");
        assert!(err.message.contains("2000-byte limit"), "{err}");
        assert!(
            !err.message.contains("maximum length of 2000 characters"),
            "the old character-based wording must be gone: {err}"
        );

        // 65 tags
        let tags: Vec<String> = (0..65).map(|i| format!("t{i}")).collect();
        let err = parse_store_input(&json!({ "summary": "s", "tags": tags }), NS).unwrap_err();
        assert_eq!(err.field, "tags");
        assert_eq!(
            err.message,
            "tags has 65 items, which exceeds the maximum of 64"
        );

        // 33 entities
        let entities: Vec<String> = (0..33).map(|i| format!("e{i}")).collect();
        let err =
            parse_store_input(&json!({ "summary": "s", "entities": entities }), NS).unwrap_err();
        assert_eq!(err.field, "entities");
        assert!(err.message.contains("exceeds the maximum of 32"), "{err}");

        // fullText over 1 MiB
        let big = "a".repeat(1_048_577);
        let err = parse_store_input(&json!({ "summary": "s", "fullText": big }), NS).unwrap_err();
        assert_eq!(err.field, "fullText");
        assert!(err.message.contains("1048576-byte limit"), "{err}");

        // empty summary
        let err = parse_store_input(&json!({ "summary": "" }), NS).unwrap_err();
        assert_eq!(err.field, "summary");
        assert_eq!(err.message, "summary must not be empty");
    }

    #[test]
    fn d8b_per_element_length_limits_reach_both_mcp_doors() {
        // `store_memory` and each item of `store_memories` come through
        // here, so this covers both MCP entry points.
        let long_tag = "a".repeat(129);
        let err = parse_store_input(&json!({ "summary": "s", "tags": ["ok", long_tag] }), NS)
            .unwrap_err();
        assert_eq!(err.field, "tags");
        assert!(err.message.contains("tags[1] is 129 bytes"), "{err}");
        assert!(err.message.contains("128-byte limit"), "{err}");

        // Labels are tighter than tags by the width of the prefix the
        // merge step prepends — 121 bytes for `entity/`.
        let long_entity = "a".repeat(122);
        let item = json!({ "summary": "s", "entities": [long_entity] });
        let err = parse_store_input(&item, NS).unwrap_err();
        assert_eq!(err.field, "entities");
        assert!(err.message.contains("121-byte limit"), "{err}");

        // The batch door reports the same failure.
        let err = parse_store_item(&item, 3, NS).unwrap_err();
        assert_eq!(err.field, "entities");

        // Exactly at the limit is accepted.
        let at_limit = json!({ "summary": "s", "entities": ["a".repeat(121)] });
        assert!(parse_store_input(&at_limit, NS).is_ok());
    }

    #[test]
    fn d9_non_object_items_are_rejected_by_shape() {
        for value in [
            json!("hello"),
            json!([]),
            json!(3),
            json!(null),
            json!(true),
        ] {
            let err = parse_store_input(&value, NS).unwrap_err();
            assert_eq!(err.field, "");
            assert!(
                err.message.starts_with("Item must be an object (got "),
                "{err}"
            );
        }
        assert_eq!(
            parse_store_input(&json!("hello"), NS).unwrap_err().message,
            "Item must be an object (got string)"
        );
    }

    #[test]
    fn d10_batch_items_name_their_index_when_not_an_object() {
        let err = parse_store_item(&json!("hello"), 3, NS).unwrap_err();
        assert_eq!(err.field, "");
        assert_eq!(
            err.message,
            "Item at index 3 must be an object (got string)"
        );

        // Everything else is byte-identical to the single-call path.
        let item = json!({ "summary": "s", "tags": "a,b" });
        assert_eq!(
            parse_store_item(&item, 0, NS).unwrap_err(),
            parse_store_input(&item, NS).unwrap_err()
        );
        assert_eq!(
            parse_store_item(&json!({ "summary": "s" }), 0, NS)
                .unwrap()
                .namespace,
            NS
        );
    }

    #[test]
    fn d11_error_display_is_exactly_the_message() {
        let err = parse_store_input(&json!({}), NS).unwrap_err();
        assert_eq!(err.to_string(), err.message);
    }
}
