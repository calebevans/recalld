//! MCP tool definitions and handlers for Recalld memory operations.
//!
//! Defines 14 tools: store_memory, store_memories, recall_memories, get_memory,
//! reinforce_memory, forget_memory, forget_memories, find_similar_memories,
//! create_namespace, delete_namespace, namespace_stats, list_memories,
//! list_namespaces, list_tags.

use serde_json::json;

use crate::mcp::args;
use crate::mcp::bridge::{McpBridge, StoredMemory};
use crate::mcp::protocol::{ToolAnnotations, ToolCallResult, ToolInfo};
use crate::model::constants::{
    DEFAULT_LIST_LIMIT, DEFAULT_TAG_LIMIT, DUPLICATE_SIMILARITY_THRESHOLD, EMOTION_MAX_BYTES,
    ENTITY_MAX_BYTES, FULL_TEXT_MAX_BYTES, MAX_BATCH_MEMORIES, MAX_EMOTIONS, MAX_ENTITIES,
    MAX_LIST_LIMIT, MAX_TAG_LIMIT, MAX_TAGS, MAX_TOPICS, SUMMARY_MAX_BYTES, TAG_MAX_BYTES,
    TOPIC_MAX_BYTES,
};
use crate::model::validation::validate_namespace_name;

/// Schema description for the `summary` field.
///
/// The limit is enforced in UTF-8 bytes; the previous wording said
/// "chars", which made a 1,900-character em-dash summary look like it
/// should fit when it is 5,700 bytes.
const SUMMARY_DESC: &str = "Short description of the memory. Limit is 2000 UTF-8 bytes, \
     which is 2000 ASCII characters but fewer for text containing em dashes, curly quotes, \
     accents, or emoji (2-4 bytes each).";

/// Schema description for the `fullText` field.
const FULL_TEXT_DESC: &str = "Detailed content. Removed as memory decays to ghost phase. \
     Limit is 1048576 UTF-8 bytes (1 MiB).";

// ═══════════════════════════════════════════════════════════════════════
// Registry and dispatch
// ═══════════════════════════════════════════════════════════════════════

/// Return all tool definitions for `tools/list`.
pub fn tool_definitions() -> Vec<ToolInfo> {
    vec![
        store_memory_def(),
        store_memories_def(),
        recall_memories_def(),
        get_memory_def(),
        reinforce_memory_def(),
        forget_memory_def(),
        forget_memories_def(),
        find_similar_memories_def(),
        create_namespace_def(),
        delete_namespace_def(),
        namespace_stats_def(),
        list_memories_def(),
        list_namespaces_def(),
        list_tags_def(),
    ]
}

/// Every tool name [`dispatch_tool`] has an arm for.
///
/// Kept next to the match so a definition added without a dispatch arm —
/// which advertises a tool that then answers "Unknown tool" — fails a
/// test rather than a caller.
#[cfg(test)]
const DISPATCHED: [&str; 14] = [
    "store_memory",
    "store_memories",
    "recall_memories",
    "get_memory",
    "reinforce_memory",
    "forget_memory",
    "forget_memories",
    "find_similar_memories",
    "create_namespace",
    "delete_namespace",
    "namespace_stats",
    "list_memories",
    "list_namespaces",
    "list_tags",
];

/// Dispatch a tool call by name to the appropriate handler.
pub async fn dispatch_tool(
    bridge: &McpBridge,
    name: &str,
    arguments: serde_json::Value,
) -> ToolCallResult {
    match name {
        "store_memory" => handle_store_memory(bridge, arguments).await,
        "store_memories" => handle_store_memories(bridge, arguments).await,
        "recall_memories" => handle_recall_memories(bridge, arguments).await,
        "get_memory" => handle_get_memory(bridge, arguments).await,
        "reinforce_memory" => handle_reinforce_memory(bridge, arguments).await,
        "forget_memory" => handle_forget_memory(bridge, arguments).await,
        "forget_memories" => handle_forget_memories(bridge, arguments).await,
        "find_similar_memories" => handle_find_similar_memories(bridge, arguments).await,
        "create_namespace" => handle_create_namespace(bridge, arguments).await,
        "delete_namespace" => handle_delete_namespace(bridge, arguments).await,
        "namespace_stats" => handle_namespace_stats(bridge, arguments).await,
        "list_memories" => handle_list_memories(bridge, arguments).await,
        "list_namespaces" => handle_list_namespaces(bridge, arguments).await,
        "list_tags" => handle_list_tags(bridge, arguments).await,
        _ => ToolCallResult::error(format!("Unknown tool: {name}")),
    }
}

// ═══════════════════════════════════════════════════════════════════════
// Tool 1: store_memory
// ═══════════════════════════════════════════════════════════════════════

fn store_memory_def() -> ToolInfo {
    ToolInfo {
        name: "store_memory".to_string(),
        title: Some("Store Memory".to_string()),
        // The near-duplicate half of this description teaches the
        // RECOVERY, not just the warning. The check is post-hoc by
        // construction — the memory is already stored when the report
        // arrives — so an agent told only "these look similar" has no
        // action available and will ignore it.
        description: "Store a new observation, fact, or piece of context as a memory. \
            The system automatically generates an embedding for semantic search. \
            Use tags to categorize (e.g., \"topic/rust\", \"project/recalld\"). \
            Memories decay naturally over time unless reinforced. \
            The memory is always stored, but the result may carry a `nearDuplicates` \
            object listing existing memories at cosine similarity 0.85 or higher. \
            Act on it rather than ignoring it: if the new memory restates one of them, \
            call `forget_memory` on the id you just created, then either \
            `reinforce_memory` on the existing memory (nothing new to say) or \
            `store_memory` again with `supersedes` set to the existing id (the fact \
            changed). Leave both only when they are genuinely distinct facts that \
            happen to read alike. Pass `checkDuplicates: false` to skip the check \
            during bulk ingestion."
            .to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "summary": {
                    "type": "string",
                    "description": SUMMARY_DESC,
                    // Counts code points, and chars <= bytes always, so
                    // this is strictly looser than the server's byte
                    // limit — a useful client-side guard that can never
                    // reject input the server would accept.
                    "maxLength": SUMMARY_MAX_BYTES
                },
                "fullText": {
                    "type": "string",
                    "description": FULL_TEXT_DESC,
                    "maxLength": FULL_TEXT_MAX_BYTES
                },
                "tags": {
                    "type": "array",
                    // Per-element `maxLength` counts code points, so like
                    // the `summary` guard above it is strictly looser
                    // than the server's byte limit.
                    "items": { "type": "string", "maxLength": TAG_MAX_BYTES },
                    "maxItems": MAX_TAGS,
                    "description": "Categorization tags, e.g. [\"topic/rust\", \"type/observation\"]"
                },
                "entities": {
                    "type": "array",
                    "items": { "type": "string", "maxLength": ENTITY_MAX_BYTES },
                    "maxItems": MAX_ENTITIES,
                    "description": "Named entities (people, places, orgs, titles) mentioned in this memory. Used for search indexing and graph linking."
                },
                "topics": {
                    "type": "array",
                    "items": { "type": "string", "maxLength": TOPIC_MAX_BYTES },
                    "maxItems": MAX_TOPICS,
                    "description": "Topic keywords describing what the memory is about, e.g. [\"rust\", \"cooking\", \"career\"]"
                },
                "emotions": {
                    "type": "array",
                    "items": { "type": "string", "maxLength": EMOTION_MAX_BYTES },
                    "maxItems": MAX_EMOTIONS,
                    "description": "Emotional tone if relevant, e.g. [\"happy\", \"anxious\", \"grateful\"]"
                },
                "namespace": {
                    "type": "string",
                    "description": "Memory partition (default: \"default\")",
                    "default": "default"
                },
                "parentId": {
                    "type": "string",
                    "description": "UUID of an existing memory in the same namespace, linked as this memory's parent. The store fails if the target does not exist or is in a different namespace."
                },
                "supersedes": {
                    "type": "string",
                    "description": "UUID of an existing memory in the same namespace that this one replaces. Recall returns this memory in place of the old one. The store fails if the target does not exist or is in a different namespace."
                },
                "checkDuplicates": {
                    "type": "boolean",
                    "description": "Report existing memories that closely resemble this one (default: true). Never blocks the store; it only adds a `nearDuplicates` object to the result. Set false for bulk ingestion, where one similarity scan per memory is not worth paying.",
                    "default": true
                }
            },
            "required": ["summary"]
        }),
        annotations: Some(ToolAnnotations {
            read_only_hint: Some(false),
            destructive_hint: Some(false),
            idempotent_hint: Some(false),
            open_world_hint: Some(false),
        }),
    }
}

async fn handle_store_memory(bridge: &McpBridge, arguments: serde_json::Value) -> ToolCallResult {
    // Extraction, type strictness, markup detection, and the shared
    // content limits all live in `args`, so this handler and the batch
    // one below cannot drift apart.
    let input = match args::parse_store_input(&arguments, bridge.default_namespace()) {
        Ok(input) => input,
        Err(e) => {
            // Logged, never silently defaulted: a client whose
            // serializer mangles arguments shows up in the operator's
            // logs on the first call, not after ten bad writes. The
            // offending text body is deliberately not logged.
            tracing::warn!(
                tool = "store_memory",
                field = %e.field,
                error = %e.message,
                "Rejected malformed MCP tool arguments"
            );
            return ToolCallResult::error(e.to_string());
        }
    };

    match bridge.storage.store_memory(input).await {
        Ok(stored) => match ToolCallResult::json(&stored) {
            Ok(r) => r,
            Err(e) => ToolCallResult::error(format!("Serialization error: {e}")),
        },
        Err(e) => ToolCallResult::error(format!("Failed to store memory: {e}")),
    }
}

// ═══════════════════════════════════════════════════════════════════════
// Tool 1b: store_memories (batch)
// ═══════════════════════════════════════════════════════════════════════

fn store_memories_def() -> ToolInfo {
    ToolInfo {
        name: "store_memories".to_string(),
        title: Some("Store Memories (Batch)".to_string()),
        description: "Store multiple memories in a single call. Each item has the \
            same schema as store_memory. Returns an array of results, one per \
            input memory. Saves round trips for bulk ingestion. \
            Items are stored in order and each is indexed before the next is \
            checked, so a result entry may carry a `nearDuplicates` object naming \
            an earlier item of the same batch as well as pre-existing memories. \
            The recovery is the same as for store_memory: forget the new id and \
            either reinforce the existing memory or re-store it with `supersedes`. \
            Set `checkDuplicates: false` on an item to skip its check."
            .to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "memories": {
                    "type": "array",
                    "description": "Array of memories to store (max 100 per call)",
                    "maxItems": MAX_BATCH_MEMORIES,
                    "items": {
                        "type": "object",
                        "properties": {
                            "summary": {
                                "type": "string",
                                "description": SUMMARY_DESC,
                                "maxLength": SUMMARY_MAX_BYTES
                            },
                            "fullText": {
                                "type": "string",
                                "description": FULL_TEXT_DESC,
                                "maxLength": FULL_TEXT_MAX_BYTES
                            },
                            "tags": {
                                "type": "array",
                                "items": { "type": "string", "maxLength": TAG_MAX_BYTES },
                                "maxItems": MAX_TAGS,
                                "description": "Categorization tags, e.g. [\"topic/rust\", \"type/observation\"]"
                            },
                            "entities": {
                                "type": "array",
                                "items": { "type": "string", "maxLength": ENTITY_MAX_BYTES },
                                "maxItems": MAX_ENTITIES,
                                "description": "Named entities (people, places, orgs, titles) mentioned in this memory."
                            },
                            "topics": {
                                "type": "array",
                                "items": { "type": "string", "maxLength": TOPIC_MAX_BYTES },
                                "maxItems": MAX_TOPICS,
                                "description": "Topic keywords describing what the memory is about"
                            },
                            "emotions": {
                                "type": "array",
                                "items": { "type": "string", "maxLength": EMOTION_MAX_BYTES },
                                "maxItems": MAX_EMOTIONS,
                                "description": "Emotional tone if relevant"
                            },
                            "namespace": {
                                "type": "string",
                                "description": "Memory partition (default: \"default\")",
                                "default": "default"
                            },
                            "parentId": {
                                "type": "string",
                                "description": "UUID of an existing memory in the same namespace, linked as this memory's parent. The store fails if the target does not exist or is in a different namespace."
                            },
                            "supersedes": {
                                "type": "string",
                                "description": "UUID of an existing memory in the same namespace that this one replaces. The store fails if the target does not exist or is in a different namespace."
                            },
                            "checkDuplicates": {
                                "type": "boolean",
                                "description": "Report existing memories that closely resemble this one (default: true). Never blocks the store; it only adds a `nearDuplicates` object to this item's result. Set false for bulk ingestion, where one similarity scan per memory is not worth paying.",
                                "default": true
                            }
                        },
                        "required": ["summary"]
                    }
                }
            },
            "required": ["memories"]
        }),
        annotations: Some(ToolAnnotations {
            read_only_hint: Some(false),
            destructive_hint: Some(false),
            idempotent_hint: Some(false),
            open_world_hint: Some(false),
        }),
    }
}

/// Build one `store_memories` result entry.
///
/// Hand-built rather than `serde_json::to_value(stored)` because a batch
/// entry additionally carries its `index`, and existing consumers key off
/// that shape.
///
/// Extracted into a pure function so the hand-built JSON can be tested
/// against `StoredMemory`'s own serialization. `json!` ignores
/// `skip_serializing_if`, so every optional field on `StoredMemory` has to
/// be inserted conditionally here or it is silently missing from batch
/// results while the single-memory tool emits it. That is a class of bug,
/// not one instance — `supersedes` was the first, `nearDuplicates` the
/// second, and the test is what stops the third.
fn store_result_entry(index: usize, stored: &StoredMemory) -> serde_json::Value {
    let mut entry = json!({
        "index": index,
        "id": stored.id,
        "namespace": stored.namespace,
        "phase": stored.phase,
        "strength": stored.strength,
        "stability": stored.stability,
        "createdAt": stored.created_at,
    });

    let optional: [(&str, Option<serde_json::Value>); 2] = [
        (
            "supersedes",
            stored
                .supersedes
                .as_ref()
                .and_then(|v| serde_json::to_value(v).ok()),
        ),
        (
            "nearDuplicates",
            stored
                .near_duplicates
                .as_ref()
                .and_then(|v| serde_json::to_value(v).ok()),
        ),
    ];
    if let Some(obj) = entry.as_object_mut() {
        for (key, value) in optional {
            if let Some(value) = value {
                obj.insert(key.to_string(), value);
            }
        }
    }

    entry
}

async fn handle_store_memories(bridge: &McpBridge, arguments: serde_json::Value) -> ToolCallResult {
    let memories_val = match arguments.get("memories") {
        Some(v) => v,
        None => return ToolCallResult::error("Missing required parameter: memories"),
    };

    let memories_arr = match memories_val.as_array() {
        Some(arr) => arr,
        None => return ToolCallResult::error("Parameter 'memories' must be an array"),
    };

    if memories_arr.is_empty() {
        return ToolCallResult::error("Parameter 'memories' must not be empty");
    }

    if memories_arr.len() > MAX_BATCH_MEMORIES {
        return ToolCallResult::error(format!(
            "Too many memories (maximum {MAX_BATCH_MEMORIES} per call)"
        ));
    }

    let mut results: Vec<serde_json::Value> = Vec::with_capacity(memories_arr.len());

    for (index, item) in memories_arr.iter().enumerate() {
        let input = match args::parse_store_item(item, index, bridge.default_namespace()) {
            Ok(input) => input,
            Err(e) => {
                tracing::warn!(
                    tool = "store_memories",
                    index,
                    field = %e.field,
                    error = %e.message,
                    "Rejected malformed MCP tool arguments"
                );
                // `field` is additive; existing consumers already
                // handle a per-item `error`.
                results.push(json!({
                    "index": index,
                    "error": e.to_string(),
                    "field": e.field,
                }));
                continue;
            }
        };

        match bridge.storage.store_memory(input).await {
            Ok(stored) => results.push(store_result_entry(index, &stored)),
            Err(e) => {
                results.push(json!({
                    "index": index,
                    "error": format!("Failed to store memory: {e}")
                }));
            }
        }
    }

    let stored_count = results.iter().filter(|r| r.get("id").is_some()).count();
    let error_count = results.iter().filter(|r| r.get("error").is_some()).count();
    let response = json!({
        "results": results,
        "total": results.len(),
        "stored": stored_count,
        "errors": error_count,
    });
    match ToolCallResult::json(&response) {
        Ok(r) => r,
        Err(e) => ToolCallResult::error(format!("Serialization error: {e}")),
    }
}

// ═══════════════════════════════════════════════════════════════════════
// Tool 2: recall_memories
// ═══════════════════════════════════════════════════════════════════════

fn recall_memories_def() -> ToolInfo {
    ToolInfo {
        name: "recall_memories".to_string(),
        title: Some("Recall Memories".to_string()),
        description: "Search for relevant memories using a natural language query. \
            Returns memories ranked by semantic similarity combined with memory \
            strength (well-reinforced memories rank higher). This is the primary \
            way to retrieve context from the memory system. Use compact=true \
            (the default) for token-efficient results containing only id, \
            summary, fullText, entities, and topics. Set compact=false to \
            include full metadata, graph edges, and neighbor context."
            .to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Natural language search query"
                },
                "limit": {
                    "type": "integer",
                    "description": "Maximum results (default: 10, max: 100)",
                    "default": 10,
                    "minimum": 1,
                    "maximum": 100
                },
                "namespace": {
                    "type": "string",
                    "description": "Which namespace to search (default: \"default\")",
                    "default": "default"
                },
                "tags": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Only return memories with ALL of these tags"
                },
                "entities": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Filter to memories mentioning these entities (people, places, proper nouns)"
                },
                "topics": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Filter to memories about these topics (e.g., 'adoption', 'cooking')"
                },
                "emotions": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Filter to memories with these emotional tones (e.g., 'happy', 'anxious')"
                },
                "minStrength": {
                    "type": "number",
                    "description": "Minimum memory strength threshold (0.0-1.0)",
                    "minimum": 0.0,
                    "maximum": 1.0
                },
                "depth": {
                    "type": "integer",
                    "description": "Graph hops to include related memories (default: 0, max: 3)",
                    "default": 0,
                    "minimum": 0,
                    "maximum": 3
                },
                "timeRangeStart": {
                    "description": "Lower bound timestamp: either milliseconds since Unix epoch (integer) or ISO 8601 string (e.g. \"2024-06-24T00:00:00Z\"). Memories created at or after this time get a relevance boost.",
                    "oneOf": [
                        { "type": "integer" },
                        { "type": "string" }
                    ]
                },
                "timeRangeEnd": {
                    "description": "Upper bound timestamp: either milliseconds since Unix epoch (integer) or ISO 8601 string (e.g. \"2024-06-24T00:00:00Z\"). Memories created at or before this time get a relevance boost.",
                    "oneOf": [
                        { "type": "integer" },
                        { "type": "string" }
                    ]
                },
                "compact": {
                    "type": "boolean",
                    "description": "If true (default), returns only id, summary, fullText, entities, and topics per memory for token efficiency. Set to false to include full metadata (tags, score, phase, strength, timestamps, related edges) and graph neighbor context.",
                    "default": true
                }
            },
            "required": ["query"]
        }),
        annotations: Some(ToolAnnotations {
            read_only_hint: Some(true),
            destructive_hint: Some(false),
            idempotent_hint: Some(true),
            open_world_hint: Some(false),
        }),
    }
}

async fn handle_recall_memories(
    bridge: &McpBridge,
    arguments: serde_json::Value,
) -> ToolCallResult {
    // Every wrong-typed or out-of-range argument is rejected in `args`.
    // Only `namespace` used to be: `{"tags": "topic/rust"}` became `[]`
    // and ran UNFILTERED over the whole namespace, `limit: 1000` was
    // clamped to 100, and `compact: "false"` became `true`.
    let args::RecallArgs { input, compact } =
        match args::parse_recall_input(&arguments, bridge.default_namespace()) {
            Ok(parsed) => parsed,
            Err(e) => {
                tracing::warn!(
                    tool = "recall_memories",
                    field = %e.field,
                    error = %e.message,
                    "Rejected malformed MCP tool arguments"
                );
                return ToolCallResult::error(e.to_string());
            }
        };

    match bridge.search.search(input).await {
        Ok(search_response) => {
            let hit_count = search_response.hits.len();
            if compact {
                // Compact mode: return only id, summary, fullText, entities, topics
                // to minimize token usage.
                let compact_hits: Vec<serde_json::Value> = search_response
                    .hits
                    .iter()
                    .map(|hit| {
                        let mut obj = json!({
                            "id": hit.id,
                            "summary": hit.summary,
                        });
                        if let Some(ref ft) = hit.full_text {
                            obj["fullText"] = json!(ft);
                        }
                        if !hit.entities.is_empty() {
                            obj["entities"] = json!(hit.entities);
                        }
                        if !hit.topics.is_empty() {
                            obj["topics"] = json!(hit.topics);
                        }
                        obj
                    })
                    .collect();
                let response = json!({
                    "memories": compact_hits,
                    "count": hit_count,
                });
                match ToolCallResult::json(&response) {
                    Ok(r) => r,
                    Err(e) => ToolCallResult::error(format!("Serialization error: {e}")),
                }
            } else {
                // Full mode: include all metadata, graph edges, and neighbor context.
                let mut response = json!({
                    "memories": search_response.hits,
                    "count": hit_count,
                });
                if !search_response.neighbors.is_empty() {
                    response["graphContext"] = json!({
                        "neighbors": search_response.neighbors,
                        "neighborCount": search_response.neighbors.len(),
                    });
                }
                match ToolCallResult::json(&response) {
                    Ok(r) => r,
                    Err(e) => ToolCallResult::error(format!("Serialization error: {e}")),
                }
            }
        }
        Err(e) => ToolCallResult::error(format!("Search failed: {e}")),
    }
}

// ═══════════════════════════════════════════════════════════════════════
// Tool 3: get_memory
// ═══════════════════════════════════════════════════════════════════════

fn get_memory_def() -> ToolInfo {
    ToolInfo {
        name: "get_memory".to_string(),
        title: Some("Get Memory".to_string()),
        description: "Retrieve a specific memory by its ID.".to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "id": {
                    "type": "string",
                    "description": "Memory UUID"
                }
            },
            "required": ["id"]
        }),
        annotations: Some(ToolAnnotations {
            read_only_hint: Some(true),
            destructive_hint: Some(false),
            idempotent_hint: Some(true),
            open_world_hint: Some(false),
        }),
    }
}

async fn handle_get_memory(bridge: &McpBridge, arguments: serde_json::Value) -> ToolCallResult {
    let id_str = match arguments.get("id").and_then(|v| v.as_str()) {
        Some(s) => s,
        None => return ToolCallResult::error("Missing required parameter: id"),
    };

    let uuid = match uuid::Uuid::parse_str(id_str) {
        Ok(u) => u,
        Err(_) => return ToolCallResult::error(format!("Invalid UUID: {id_str}")),
    };
    let id = crate::model::MemoryId::from_uuid(uuid);

    match bridge.storage.get_memory(id).await {
        Ok(Some(record)) => match ToolCallResult::json(&record) {
            Ok(r) => r,
            Err(e) => ToolCallResult::error(format!("Serialization error: {e}")),
        },
        Ok(None) => ToolCallResult::error(format!("Memory not found: {id_str}")),
        Err(e) => ToolCallResult::error(format!("Failed to get memory: {e}")),
    }
}

// ═══════════════════════════════════════════════════════════════════════
// Tool 4: reinforce_memory
// ═══════════════════════════════════════════════════════════════════════

fn reinforce_memory_def() -> ToolInfo {
    ToolInfo {
        name: "reinforce_memory".to_string(),
        title: Some("Reinforce Memory".to_string()),
        description: "Strengthen a memory that proved useful. Increases its \
            stability so it decays more slowly. Use after retrieving and \
            benefiting from a memory. Quality ratings: 1=forgot (weakens), \
            2=hard, 3=good (default), 4=easy (strongest reinforcement)."
            .to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "id": {
                    "type": "string",
                    "description": "Memory UUID to reinforce"
                },
                "quality": {
                    "type": "integer",
                    "description": "Rating 1-4: 1=forgot, 2=hard, 3=good, 4=easy (default: 3)",
                    "default": 3,
                    "minimum": 1,
                    "maximum": 4
                }
            },
            "required": ["id"]
        }),
        annotations: Some(ToolAnnotations {
            read_only_hint: Some(false),
            destructive_hint: Some(false),
            idempotent_hint: Some(true),
            open_world_hint: Some(false),
        }),
    }
}

async fn handle_reinforce_memory(
    bridge: &McpBridge,
    arguments: serde_json::Value,
) -> ToolCallResult {
    let id_str = match arguments.get("id").and_then(|v| v.as_str()) {
        Some(s) => s,
        None => return ToolCallResult::error("Missing required parameter: id"),
    };

    let uuid = match uuid::Uuid::parse_str(id_str) {
        Ok(u) => u,
        Err(_) => return ToolCallResult::error(format!("Invalid UUID: {id_str}")),
    };
    let id = crate::model::MemoryId::from_uuid(uuid);

    // Issue 4: Validate the u64 value BEFORE casting to u8 to prevent truncation
    let quality_u64 = arguments
        .get("quality")
        .and_then(|v| v.as_u64())
        .unwrap_or(3);

    if !(1..=4).contains(&quality_u64) {
        return ToolCallResult::error("Quality must be 1-4");
    }
    let quality = quality_u64 as u8;

    match bridge.storage.reinforce_memory(id, quality).await {
        Ok(result) => match ToolCallResult::json(&result) {
            Ok(r) => r,
            Err(e) => ToolCallResult::error(format!("Serialization error: {e}")),
        },
        Err(e) => ToolCallResult::error(format!("Failed to reinforce memory: {e}")),
    }
}

// ═══════════════════════════════════════════════════════════════════════
// Tool 5: forget_memory
// ═══════════════════════════════════════════════════════════════════════

fn forget_memory_def() -> ToolInfo {
    ToolInfo {
        name: "forget_memory".to_string(),
        title: Some("Forget Memory".to_string()),
        // "Permanently delete" was wrong in both directions: the record
        // survives (only its content is erased), and the tombstone it
        // leaves behind is never reclaimed. An agent told "permanent"
        // reasonably assumes the row is gone and the space is back.
        description: "Delete a memory's content. The memory moves to the Tombstone \
            phase: its summary, full text and tags are erased and it stops appearing \
            in recall, but its record and its graph edges are kept so relationship \
            chains stay intact, and its UUID is never reused. Tombstoned memories are \
            never reclaimed by the decay sweep, so they persist for the life of the \
            database. Use for incorrect, outdated or harmful information that must \
            stop being recalled immediately. To correct a memory, prefer storing the \
            correction with `supersedes`."
            .to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "id": {
                    "type": "string",
                    "description": "Memory UUID to forget. The memory moves to the \
                        Tombstone phase; its graph edges are kept and its UUID is \
                        never reused."
                }
            },
            "required": ["id"]
        }),
        annotations: Some(ToolAnnotations {
            read_only_hint: Some(false),
            destructive_hint: Some(true),
            idempotent_hint: Some(true),
            open_world_hint: Some(false),
        }),
    }
}

async fn handle_forget_memory(bridge: &McpBridge, arguments: serde_json::Value) -> ToolCallResult {
    let id_str = match arguments.get("id").and_then(|v| v.as_str()) {
        Some(s) => s,
        None => return ToolCallResult::error("Missing required parameter: id"),
    };

    let uuid = match uuid::Uuid::parse_str(id_str) {
        Ok(u) => u,
        Err(_) => return ToolCallResult::error(format!("Invalid UUID: {id_str}")),
    };
    let id = crate::model::MemoryId::from_uuid(uuid);

    match bridge.storage.delete_memory(id).await {
        Ok(deleted) => {
            let response = json!({
                "id": id_str,
                "deleted": deleted,
            });
            match ToolCallResult::json(&response) {
                Ok(r) => r,
                Err(e) => ToolCallResult::error(format!("Serialization error: {e}")),
            }
        }
        Err(e) => ToolCallResult::error(format!("Failed to delete memory: {e}")),
    }
}

// ═══════════════════════════════════════════════════════════════════════
// Tool 5b: forget_memories (batch)
// ═══════════════════════════════════════════════════════════════════════

fn forget_memories_def() -> ToolInfo {
    ToolInfo {
        name: "forget_memories".to_string(),
        title: Some("Forget Memories (Batch)".to_string()),
        description: "Delete several memories in a single call. Each moves to the \
            Tombstone phase exactly as forget_memory describes: content erased, \
            record and graph edges kept, UUID never reused, never reclaimed by the \
            decay sweep. Returns an array of results, one per input id, in the order \
            given. A `deleted: false` result means the id was unknown or the memory \
            had already been forgotten."
            .to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "ids": {
                    "type": "array",
                    "minItems": 1,
                    "maxItems": MAX_BATCH_MEMORIES,
                    "items": { "type": "string", "format": "uuid" },
                    "description": "Memory UUIDs to forget. Each moves to the Tombstone \
                        phase; its graph edges are kept and its UUID is never reused."
                }
            },
            "required": ["ids"]
        }),
        annotations: Some(ToolAnnotations {
            read_only_hint: Some(false),
            destructive_hint: Some(true),
            idempotent_hint: Some(true),
            open_world_hint: Some(false),
        }),
    }
}

async fn handle_forget_memories(
    bridge: &McpBridge,
    arguments: serde_json::Value,
) -> ToolCallResult {
    // Same guard order as store_memories: missing key, wrong type,
    // empty, over cap.
    let ids_val = match arguments.get("ids") {
        Some(v) => v,
        None => return ToolCallResult::error("Missing required parameter: ids"),
    };

    let ids_arr = match ids_val.as_array() {
        Some(arr) => arr,
        None => return ToolCallResult::error("Parameter 'ids' must be an array"),
    };

    if ids_arr.is_empty() {
        return ToolCallResult::error("Parameter 'ids' must not be empty");
    }

    if ids_arr.len() > MAX_BATCH_MEMORIES {
        return ToolCallResult::error(format!(
            "Too many ids (maximum {MAX_BATCH_MEMORIES} per call)"
        ));
    }

    // UUIDs are parsed here, not at the bridge, so one malformed id
    // becomes a per-item error instead of failing the whole batch. Each
    // valid id keeps its original index so the results can be merged
    // back in caller order.
    let mut results: Vec<serde_json::Value> = vec![serde_json::Value::Null; ids_arr.len()];
    let mut parsed: Vec<(usize, crate::model::MemoryId)> = Vec::with_capacity(ids_arr.len());

    for (index, item) in ids_arr.iter().enumerate() {
        let Some(id_str) = item.as_str() else {
            results[index] = json!({
                "index": index,
                "error": format!("Parameter 'ids[{index}]' must be a string"),
                "field": format!("ids[{index}]"),
            });
            continue;
        };
        match uuid::Uuid::parse_str(id_str) {
            Ok(uuid) => parsed.push((index, crate::model::MemoryId::from_uuid(uuid))),
            Err(_) => {
                results[index] = json!({
                    "index": index,
                    "error": format!("Invalid UUID: {id_str}"),
                    "field": format!("ids[{index}]"),
                });
            }
        }
    }

    if !parsed.is_empty() {
        let ids: Vec<crate::model::MemoryId> = parsed.iter().map(|(_, id)| *id).collect();
        match bridge.storage.delete_memories(&ids).await {
            Ok(flags) => {
                for ((index, id), deleted) in parsed.iter().zip(flags) {
                    results[*index] = json!({
                        "index": index,
                        "id": id.to_string(),
                        "deleted": deleted,
                    });
                }
            }
            Err(e) => {
                for (index, id) in &parsed {
                    results[*index] = json!({
                        "index": index,
                        "id": id.to_string(),
                        "error": format!("Failed to delete memory: {e}"),
                    });
                }
            }
        }
    }

    let deleted_count = results
        .iter()
        .filter(|r| r.get("deleted").and_then(|v| v.as_bool()) == Some(true))
        .count();
    let error_count = results.iter().filter(|r| r.get("error").is_some()).count();
    let response = json!({
        "results": results,
        "total": results.len(),
        "deleted": deleted_count,
        "errors": error_count,
    });
    match ToolCallResult::json(&response) {
        Ok(r) => r,
        Err(e) => ToolCallResult::error(format!("Serialization error: {e}")),
    }
}

// ═══════════════════════════════════════════════════════════════════════
// Tool 6: find_similar_memories
// ═══════════════════════════════════════════════════════════════════════

fn find_similar_memories_def() -> ToolInfo {
    ToolInfo {
        name: "find_similar_memories".to_string(),
        title: Some("Find Similar Memories".to_string()),
        description: "Find memories semantically similar to a specific existing \
            memory, or scan an entire namespace for clusters of near-duplicate \
            memories. Two modes: \"single\" (default) requires an id and finds \
            similar memories to it; \"scan\" requires a namespace and detects \
            clusters of near-duplicates across the namespace."
            .to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "mode": {
                    "type": "string",
                    "enum": ["single", "scan"],
                    "description": "Operation mode: \"single\" (default) finds memories similar to a given id; \"scan\" detects duplicate clusters across a namespace",
                    "default": "single"
                },
                "id": {
                    "type": "string",
                    "description": "Source memory UUID (required for \"single\" mode)"
                },
                "namespace": {
                    "type": "string",
                    "description": "Namespace to scan for duplicates (required for \"scan\" mode, defaults to session default for \"single\" mode)"
                },
                "limit": {
                    "type": "integer",
                    "description": "Maximum results per source memory in \"single\" mode (default: 10, max: 100)",
                    "default": 10,
                    "minimum": 1,
                    "maximum": 100
                },
                "minScore": {
                    "type": "number",
                    "description": "Minimum similarity threshold for \"single\" mode (0.0-1.0)"
                },
                "threshold": {
                    "type": "number",
                    "description": "Similarity threshold for \"scan\" mode duplicate detection (0.0-1.0, default: 0.85)",
                    // `f64`, so this serializes as `0.85` and not as the
                    // widened `0.8500000238418579` an `f32` would produce.
                    "default": DUPLICATE_SIMILARITY_THRESHOLD,
                    "minimum": 0.0,
                    "maximum": 1.0
                },
                "sameNamespace": {
                    "type": "boolean",
                    "description": "Restrict to same namespace in \"single\" mode (default: true). Ignored in \"scan\" mode (scan always operates within the specified namespace).",
                    "default": true
                }
            }
        }),
        annotations: Some(ToolAnnotations {
            read_only_hint: Some(true),
            destructive_hint: Some(false),
            idempotent_hint: Some(true),
            open_world_hint: Some(false),
        }),
    }
}

async fn handle_find_similar_memories(
    bridge: &McpBridge,
    arguments: serde_json::Value,
) -> ToolCallResult {
    // A wrong-typed `mode` is an error, never a silent "single": those
    // are two different operations, and running the wrong one is not a
    // degradation the caller can detect.
    let mode = match args::opt_enum(&arguments, "mode", &["single", "scan"]) {
        Ok(m) => m.unwrap_or_else(|| "single".to_string()),
        Err(e) => {
            tracing::warn!(
                tool = "find_similar_memories",
                field = %e.field,
                error = %e.message,
                "Rejected malformed MCP tool arguments"
            );
            return ToolCallResult::error(e.to_string());
        }
    };

    match mode.as_str() {
        "single" => handle_find_similar_single(bridge, &arguments).await,
        "scan" => handle_find_similar_scan(bridge, &arguments).await,
        // Unreachable: `opt_enum` already rejected anything else.
        other => ToolCallResult::error(format!(
            "Invalid mode: \"{other}\". Must be \"single\" or \"scan\"."
        )),
    }
}

/// Handle "single" mode: find memories similar to a given ID (original behavior).
async fn handle_find_similar_single(
    bridge: &McpBridge,
    arguments: &serde_json::Value,
) -> ToolCallResult {
    let args::FindSimilarArgs {
        id,
        limit,
        min_score,
        same_namespace,
    } = match args::parse_find_similar_args(arguments) {
        Ok(parsed) => parsed,
        Err(e) => {
            tracing::warn!(
                tool = "find_similar_memories",
                field = %e.field,
                error = %e.message,
                "Rejected malformed MCP tool arguments"
            );
            return ToolCallResult::error(e.to_string());
        }
    };

    match bridge
        .search
        .find_similar(id, limit, min_score, same_namespace)
        .await
    {
        Ok(hits) => {
            let response = json!({
                "sourceId": id.to_string(),
                "memories": hits,
                "count": hits.len(),
            });
            match ToolCallResult::json(&response) {
                Ok(r) => r,
                Err(e) => ToolCallResult::error(format!("Serialization error: {e}")),
            }
        }
        Err(e) => ToolCallResult::error(format!("Find similar failed: {e}")),
    }
}

/// Handle "scan" mode: detect clusters of near-duplicate memories in a namespace.
async fn handle_find_similar_scan(
    bridge: &McpBridge,
    arguments: &serde_json::Value,
) -> ToolCallResult {
    let args::DuplicateScanArgs {
        namespace,
        threshold,
    } = match args::parse_duplicate_scan_args(
        arguments,
        bridge.default_namespace(),
        DUPLICATE_SIMILARITY_THRESHOLD,
    ) {
        Ok(parsed) => parsed,
        Err(e) => {
            tracing::warn!(
                tool = "find_similar_memories",
                field = %e.field,
                error = %e.message,
                "Rejected malformed MCP tool arguments"
            );
            return ToolCallResult::error(e.to_string());
        }
    };

    // Max memories to sample from the namespace (bounded to 200).
    const MAX_SCAN_MEMORIES: usize = 200;

    match bridge
        .search
        .scan_duplicates(&namespace, threshold, MAX_SCAN_MEMORIES)
        .await
    {
        Ok(clusters) => {
            let response = json!({
                "namespace": namespace,
                "threshold": threshold,
                "clusters": clusters,
                "clusterCount": clusters.len(),
            });
            match ToolCallResult::json(&response) {
                Ok(r) => r,
                Err(e) => ToolCallResult::error(format!("Serialization error: {e}")),
            }
        }
        Err(e) => ToolCallResult::error(format!("Duplicate scan failed: {e}")),
    }
}

// ═══════════════════════════════════════════════════════════════════════
// Tool 7: create_namespace
// ═══════════════════════════════════════════════════════════════════════

fn create_namespace_def() -> ToolInfo {
    ToolInfo {
        name: "create_namespace".to_string(),
        title: Some("Create Namespace".to_string()),
        description: "Create a new memory namespace for organizing memories by \
            domain or project. Each namespace has its own embedding space and \
            decay configuration. You can set a custom decay rate multiplier to \
            make memories in this namespace decay faster, slower, or not at all. \
            Embedding dimensions are fixed after creation."
            .to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "Namespace name (alphanumeric, hyphens, underscores; 1-64 chars)",
                    "pattern": "^[a-zA-Z0-9_-]{1,64}$"
                },
                "embeddingDim": {
                    "type": "integer",
                    "description": "Embedding dimensions, fixed after creation. Defaults to the same dimensions as the 'default' namespace."
                },
                "initialStability": {
                    "type": "number",
                    "description": "Starting stability in days for new memories (default: 3.7145)",
                    "default": 3.7145
                },
                "desiredRetention": {
                    "type": "number",
                    "description": "Target retention rate 0.0-1.0 (default: 0.9)",
                    "default": 0.9,
                    "minimum": 0.0,
                    "maximum": 1.0
                },
                "decayRateMultiplier": {
                    "type": "number",
                    "description": "Decay rate multiplier for this namespace. 1.0 = normal (default), 2.0 = 2x slower decay, 0.5 = 2x faster decay, 0.0 = decay disabled. Omit to inherit global setting.",
                    "minimum": 0.0
                }
            },
            "required": ["name"]
        }),
        annotations: Some(ToolAnnotations {
            read_only_hint: Some(false),
            destructive_hint: Some(false),
            idempotent_hint: Some(false),
            open_world_hint: Some(false),
        }),
    }
}

async fn handle_create_namespace(
    bridge: &McpBridge,
    arguments: serde_json::Value,
) -> ToolCallResult {
    let name = match arguments.get("name").and_then(|v| v.as_str()) {
        Some(s) => s.to_string(),
        None => return ToolCallResult::error("Missing required parameter: name"),
    };

    // Server-side validation of namespace name, shared with the HTTP API.
    if let Err(e) = validate_namespace_name(&name) {
        return ToolCallResult::error(e.to_string());
    }

    let embedding_dim = arguments
        .get("embeddingDim")
        .and_then(|v| v.as_u64())
        .map(|v| v as u16);
    let initial_stability = arguments
        .get("initialStability")
        .and_then(|v| v.as_f64())
        .map(|f| f as f32);
    let desired_retention = arguments
        .get("desiredRetention")
        .and_then(|v| v.as_f64())
        .map(|f| f as f32);
    let decay_rate_multiplier = arguments
        .get("decayRateMultiplier")
        .and_then(|v| v.as_f64())
        .map(|f| f as f32);

    let input = crate::mcp::bridge::CreateNamespaceInput {
        name,
        embedding_dim,
        initial_stability,
        desired_retention,
        decay_rate_multiplier,
    };

    match bridge.namespaces.create_namespace(input).await {
        Ok(info) => match ToolCallResult::json(&info) {
            Ok(r) => r,
            Err(e) => ToolCallResult::error(format!("Serialization error: {e}")),
        },
        Err(e) => ToolCallResult::error(format!("Failed to create namespace: {e}")),
    }
}

// ═══════════════════════════════════════════════════════════════════════
// Tool 8b: delete_namespace
// ═══════════════════════════════════════════════════════════════════════

fn delete_namespace_def() -> ToolInfo {
    ToolInfo {
        name: "delete_namespace".to_string(),
        title: Some("Delete Namespace".to_string()),
        description: "Destroy a namespace and everything in it: every memory, every \
            graph edge touching one, its search index entries, and its vector file on \
            disk. There is no undo and no backup. A namespace that still holds \
            memories is refused unless you pass force: true. \
            This is also the only operation that reclaims tombstones — memories \
            deleted with forget_memory keep a record for the life of the database, and \
            nothing else ever removes it. \
            The 'default' namespace cannot be deleted."
            .to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "minLength": 1,
                    "maxLength": 64,
                    "description": "Namespace to delete. The 'default' namespace cannot be deleted."
                },
                "force": {
                    "type": "boolean",
                    "default": false,
                    "description": "Delete the namespace even if it still holds memories. \
                        Without this, a non-empty namespace is refused. Forced deletion \
                        permanently destroys every memory in the namespace and removes \
                        its vector file from disk. There is no undo and no backup."
                }
            },
            "required": ["name"]
        }),
        annotations: Some(ToolAnnotations {
            read_only_hint: Some(false),
            destructive_hint: Some(true),
            // Deliberately NOT idempotent, unlike forget_memory. This
            // removes a directory from disk and the namespace id is
            // never reused, so a blind retry after an unknown outcome
            // could destroy a same-named namespace the user recreated in
            // between.
            idempotent_hint: Some(false),
            open_world_hint: Some(false),
        }),
    }
}

async fn handle_delete_namespace(
    bridge: &McpBridge,
    arguments: serde_json::Value,
) -> ToolCallResult {
    use crate::model::constants::DEFAULT_NAMESPACE_NAME;

    let name = match arguments.get("name").and_then(|v| v.as_str()) {
        Some(s) => s.to_string(),
        None => return ToolCallResult::error("Missing required parameter: name"),
    };

    // The name becomes a directory name that `remove_dir_all` is
    // pointed at, so it is checked before anything is done with it.
    // Refused here as well as in the adapter; the adapter's copy is the
    // load-bearing one, because the socket is its own entry point.
    if let Err(e) = validate_namespace_name(&name) {
        return ToolCallResult::error(e.to_string());
    }

    // Refused here as well as in the adapter. This copy saves a daemon
    // round trip; the adapter's copy is the load-bearing one, because
    // the socket is its own entry point.
    // Case-insensitive: the guard protects a directory, and a
    // case-insensitive filesystem resolves `Default/` to `default/`.
    if name.eq_ignore_ascii_case(DEFAULT_NAMESPACE_NAME) {
        return ToolCallResult::error(format!(
            "The '{DEFAULT_NAMESPACE_NAME}' namespace cannot be deleted. It is recreated \
             on every startup under the same directory name, so deleting it would leave \
             a stale vectors.dat that a new namespace could silently reuse."
        ));
    }

    // A wrong-typed `force` is an error naming the field, never a silent
    // false: quietly downgrading to the safe path would make a caller
    // with a broken serializer see a refusal they cannot explain.
    let force = match args::opt_bool(&arguments, "force") {
        Ok(v) => v.unwrap_or(false),
        Err(e) => return ToolCallResult::error(e.to_string()),
    };

    let input = crate::mcp::bridge::DeleteNamespaceInput { name, force };

    match bridge.namespaces.delete_namespace(input).await {
        Ok(result) => match ToolCallResult::json(&result) {
            Ok(r) => r,
            Err(e) => ToolCallResult::error(format!("Serialization error: {e}")),
        },
        // A partial purge is not a failed delete. Prefixing it with
        // "Failed to delete namespace" would tell the agent nothing
        // happened, when in fact memories were destroyed and the job
        // needs finishing — so the message passes through unwrapped.
        Err(crate::mcp::bridge::BridgeError::PartiallyApplied(msg)) => ToolCallResult::error(msg),
        Err(e) => ToolCallResult::error(format!("Failed to delete namespace: {e}")),
    }
}

// ═══════════════════════════════════════════════════════════════════════
// Tool 9: namespace_stats
// ═══════════════════════════════════════════════════════════════════════

fn namespace_stats_def() -> ToolInfo {
    ToolInfo {
        name: "namespace_stats".to_string(),
        title: Some("Namespace Stats".to_string()),
        description: "Get statistics for a memory namespace including total memory count, \
            phase breakdown (full/summary/ghost), permastore count, average strength, \
            edge count, and vector storage size. Use this to check how many memories \
            exist or monitor namespace health."
            .to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "namespace": {
                    "type": "string",
                    "description": "Namespace name to get stats for (default: \"default\")",
                    "default": "default"
                }
            }
        }),
        annotations: Some(ToolAnnotations {
            read_only_hint: Some(true),
            destructive_hint: Some(false),
            idempotent_hint: Some(true),
            open_world_hint: Some(false),
        }),
    }
}

async fn handle_namespace_stats(
    bridge: &McpBridge,
    arguments: serde_json::Value,
) -> ToolCallResult {
    let namespace = match args::namespace_or(&arguments, bridge.default_namespace()) {
        Ok(ns) => ns,
        Err(e) => {
            tracing::warn!(
                tool = "namespace_stats",
                field = %e.field,
                error = %e.message,
                "Rejected malformed MCP tool arguments"
            );
            return ToolCallResult::error(e.to_string());
        }
    };

    match bridge.namespaces.namespace_stats(&namespace).await {
        Ok(stats) => match ToolCallResult::json(&stats) {
            Ok(r) => r,
            Err(e) => ToolCallResult::error(format!("Serialization error: {e}")),
        },
        Err(e) => ToolCallResult::error(format!("Failed to get namespace stats: {e}")),
    }
}

// ═══════════════════════════════════════════════════════════════════════
// Tool 10: list_memories
// ═══════════════════════════════════════════════════════════════════════

fn list_memories_def() -> ToolInfo {
    ToolInfo {
        name: "list_memories".to_string(),
        title: Some("List Memories".to_string()),
        description: "List all memories in a namespace with pagination and optional \
            filters. Unlike recall_memories, this does not require a search query \
            or embedding lookup — it returns memories sorted by creation date \
            (newest first). Use this for browsing, auditing, or enumerating \
            memories when you don't have a specific search query."
            .to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "namespace": {
                    "type": "string",
                    "description": "Which namespace to list from (default: \"default\")",
                    "default": "default"
                },
                "limit": {
                    "type": "integer",
                    "description": format!(
                        "Maximum results per page (default: {DEFAULT_LIST_LIMIT}, \
                         max: {MAX_LIST_LIMIT}). A larger value is rejected, not clamped."
                    ),
                    "default": DEFAULT_LIST_LIMIT,
                    "minimum": 1,
                    "maximum": MAX_LIST_LIMIT
                },
                "offset": {
                    "type": "integer",
                    "description": "Number of results to skip for pagination (default: 0)",
                    "default": 0,
                    "minimum": 0
                },
                "tags": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Only return memories with ALL of these tags (AND semantics)"
                },
                "entities": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Only return memories mentioning ALL of these entities"
                },
                "timeRangeStart": {
                    "description": "Lower bound timestamp: either milliseconds since Unix epoch (integer) or ISO 8601 string. Only memories created at or after this time are returned.",
                    "oneOf": [
                        { "type": "integer" },
                        { "type": "string" }
                    ]
                },
                "timeRangeEnd": {
                    "description": "Upper bound timestamp: either milliseconds since Unix epoch (integer) or ISO 8601 string. Only memories created at or before this time are returned.",
                    "oneOf": [
                        { "type": "integer" },
                        { "type": "string" }
                    ]
                }
            }
        }),
        annotations: Some(ToolAnnotations {
            read_only_hint: Some(true),
            destructive_hint: Some(false),
            idempotent_hint: Some(true),
            open_world_hint: Some(false),
        }),
    }
}

async fn handle_list_memories(bridge: &McpBridge, arguments: serde_json::Value) -> ToolCallResult {
    // Every wrong-typed or out-of-range argument is rejected in `args`,
    // so this handler cannot silently list the default namespace or
    // silently return a smaller page than was asked for.
    let input = match args::parse_list_memories_input(&arguments, bridge.default_namespace()) {
        Ok(input) => input,
        Err(e) => {
            tracing::warn!(
                tool = "list_memories",
                field = %e.field,
                error = %e.message,
                "Rejected malformed MCP tool arguments"
            );
            return ToolCallResult::error(e.to_string());
        }
    };

    match bridge.storage.list_memories(input).await {
        Ok(response) => match ToolCallResult::json(&response) {
            Ok(r) => r,
            Err(e) => ToolCallResult::error(format!("Serialization error: {e}")),
        },
        Err(e) => ToolCallResult::error(format!("List memories failed: {e}")),
    }
}

// ═══════════════════════════════════════════════════════════════════════
// Tool 11: list_namespaces
// ═══════════════════════════════════════════════════════════════════════

fn list_namespaces_def() -> ToolInfo {
    ToolInfo {
        name: "list_namespaces".to_string(),
        title: Some("List Namespaces".to_string()),
        description: "List every memory namespace with its ID, embedding dimensions, live \
            memory count, and creation date. Call this first when you do not know which \
            namespaces exist, before targeting one with list_memories, list_tags, or \
            namespace_stats. Counts exclude deleted (tombstoned) memories."
            .to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {}
        }),
        annotations: Some(ToolAnnotations {
            read_only_hint: Some(true),
            destructive_hint: Some(false),
            idempotent_hint: Some(true),
            open_world_hint: Some(false),
        }),
    }
}

/// The response is the bare serialization of `Vec<NamespaceInfo>`, not a
/// `{"namespaces": [...], "total": n}` envelope. Reusing the type
/// unchanged makes this tool and the `recalld://namespaces` resource
/// provably the same payload, and `total` is the array length.
async fn handle_list_namespaces(
    bridge: &McpBridge,
    _arguments: serde_json::Value,
) -> ToolCallResult {
    match bridge.namespaces.list_namespaces().await {
        Ok(namespaces) => match ToolCallResult::json(&namespaces) {
            Ok(r) => r,
            Err(e) => ToolCallResult::error(format!("Serialization error: {e}")),
        },
        Err(e) => ToolCallResult::error(format!("List namespaces failed: {e}")),
    }
}

// ═══════════════════════════════════════════════════════════════════════
// Tool 12: list_tags
// ═══════════════════════════════════════════════════════════════════════

fn list_tags_def() -> ToolInfo {
    ToolInfo {
        name: "list_tags".to_string(),
        title: Some("List Tags".to_string()),
        description: "List the tag vocabulary with per-label memory counts, most common \
            first. Use this to discover what labels already exist before filtering \
            list_memories or recall_memories, and before storing — so you reuse an existing \
            tag instead of inventing a near-duplicate. Results come in four buckets: plain \
            tags, plus the entity, topic, and emotion labels derived from those fields, \
            returned without their prefixes so they can be passed straight back to \
            store_memory or list_memories. Counts cover live memories only; deleted \
            memories are excluded."
            .to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "namespace": {
                    "type": "string",
                    "description": "Which namespace to count within (default: \"default\"). \
                        Reads that namespace's memory records, costing about one \
                        list_memories call.",
                    "default": "default"
                },
                "allNamespaces": {
                    "type": "boolean",
                    "description": "Count across every namespace instead of one. \
                        Cannot be combined with an explicit namespace.",
                    "default": false
                },
                "limit": {
                    "type": "integer",
                    "description": format!(
                        "Maximum labels returned per bucket (default: {DEFAULT_TAG_LIMIT}, \
                         max: {MAX_TAG_LIMIT}). Each bucket also reports its untruncated \
                         total."
                    ),
                    "default": DEFAULT_TAG_LIMIT,
                    "minimum": 1,
                    "maximum": MAX_TAG_LIMIT
                }
            }
        }),
        annotations: Some(ToolAnnotations {
            read_only_hint: Some(true),
            destructive_hint: Some(false),
            idempotent_hint: Some(true),
            open_world_hint: Some(false),
        }),
    }
}

async fn handle_list_tags(bridge: &McpBridge, arguments: serde_json::Value) -> ToolCallResult {
    let input = match args::parse_list_tags_input(&arguments, bridge.default_namespace()) {
        Ok(input) => input,
        Err(e) => {
            tracing::warn!(
                tool = "list_tags",
                field = %e.field,
                error = %e.message,
                "Rejected malformed MCP tool arguments"
            );
            return ToolCallResult::error(e.to_string());
        }
    };

    match bridge.storage.list_tags(input).await {
        Ok(response) => match ToolCallResult::json(&response) {
            Ok(r) => r,
            Err(e) => ToolCallResult::error(format!("Serialization error: {e}")),
        },
        Err(e) => ToolCallResult::error(format!("List tags failed: {e}")),
    }
}

// ═══════════════════════════════════════════════════════════════════════
// Tests
// ═══════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;

    /// The `summary` sub-schema of each store tool, keyed by tool name.
    fn summary_schemas() -> Vec<(&'static str, serde_json::Value)> {
        let single = store_memory_def().input_schema["properties"]["summary"].clone();
        let batch = store_memories_def().input_schema["properties"]["memories"]["items"]
            ["properties"]["summary"]
            .clone();
        vec![("store_memory", single), ("store_memories", batch)]
    }

    #[test]
    fn summary_schema_describes_the_limit_in_bytes_not_chars() {
        for (tool, schema) in summary_schemas() {
            let desc = schema["description"]
                .as_str()
                .unwrap_or_else(|| panic!("{tool}: summary has no description"));
            assert!(desc.contains("bytes"), "{tool}: {desc}");
            assert!(!desc.contains("chars"), "{tool}: {desc}");
        }
    }

    #[test]
    fn summary_schema_keeps_max_length_guard() {
        // maxLength counts code points and chars <= bytes always, so
        // 2000 is strictly looser than the server's byte limit; it can
        // never reject something the server would accept.
        for (tool, schema) in summary_schemas() {
            assert_eq!(schema["maxLength"].as_u64(), Some(2000), "{tool}");
        }
    }

    #[test]
    fn array_fields_declare_max_items() {
        let single = store_memory_def().input_schema;
        let batch = store_memories_def().input_schema;
        let batch_item = &batch["properties"]["memories"]["items"];
        for props in [&single["properties"], &batch_item["properties"]] {
            assert_eq!(props["tags"]["maxItems"].as_u64(), Some(MAX_TAGS as u64));
            assert_eq!(
                props["entities"]["maxItems"].as_u64(),
                Some(MAX_ENTITIES as u64)
            );
            assert_eq!(
                props["topics"]["maxItems"].as_u64(),
                Some(MAX_TOPICS as u64)
            );
            assert_eq!(
                props["emotions"]["maxItems"].as_u64(),
                Some(MAX_EMOTIONS as u64)
            );
            assert_eq!(
                props["fullText"]["maxLength"].as_u64(),
                Some(FULL_TEXT_MAX_BYTES as u64)
            );
            // Per-element limits: an over-long label would otherwise be
            // merged into an over-long derived tag and dropped.
            for (field, max) in [
                ("tags", TAG_MAX_BYTES),
                ("entities", ENTITY_MAX_BYTES),
                ("topics", TOPIC_MAX_BYTES),
                ("emotions", EMOTION_MAX_BYTES),
            ] {
                assert_eq!(
                    props[field]["items"]["maxLength"].as_u64(),
                    Some(max as u64),
                    "{field}"
                );
            }
        }
        assert_eq!(
            batch["properties"]["memories"]["maxItems"].as_u64(),
            Some(MAX_BATCH_MEMORIES as u64)
        );
    }

    // ── near-duplicate advisory ──────────────────────────────────────

    /// The `checkDuplicates` sub-schema of each store tool, keyed by tool
    /// name — same shape as [`summary_schemas`], and for the same reason:
    /// the two schemas are written out separately and drift silently.
    fn check_duplicates_schemas() -> Vec<(&'static str, serde_json::Value)> {
        let single = store_memory_def().input_schema["properties"]["checkDuplicates"].clone();
        let batch = store_memories_def().input_schema["properties"]["memories"]["items"]
            ["properties"]["checkDuplicates"]
            .clone();
        vec![("store_memory", single), ("store_memories", batch)]
    }

    #[test]
    fn d21_both_store_tools_declare_check_duplicates_defaulting_to_true() {
        for (tool, schema) in check_duplicates_schemas() {
            assert_eq!(schema["type"], json!("boolean"), "{tool}");
            assert_eq!(
                schema["default"],
                json!(true),
                "{tool}: the opt-out must advertise itself as on by default"
            );
            assert!(
                schema["description"]
                    .as_str()
                    .is_some_and(|d| !d.is_empty()),
                "{tool}: checkDuplicates has no description"
            );
        }
    }

    /// The warning is post-hoc, so the description has to name both the
    /// field the caller will see and the recovery that acts on it. A
    /// description that mentions neither leaves the agent with a report it
    /// has no instruction to use.
    #[test]
    fn d22_store_memory_description_names_the_report_and_the_recovery() {
        let description = store_memory_def().description;
        for needle in [
            "nearDuplicates",
            "supersedes",
            "forget_memory",
            "reinforce_memory",
            "checkDuplicates",
        ] {
            assert!(
                description.contains(needle),
                "store_memory description does not mention {needle}: {description}"
            );
        }
    }

    /// `json!` widens an `f32` to `f64` on the way into a `Value`, so a
    /// threshold declared `0.85f32` publishes as `0.8500000238418579` —
    /// a schema change nobody made and nobody would notice. The constant
    /// is `f64` for exactly this reason; this test is what keeps it that
    /// way.
    #[test]
    fn d23_scan_mode_threshold_default_serializes_as_a_clean_decimal() {
        let schema = find_similar_memories_def().input_schema;
        assert_eq!(
            schema["properties"]["threshold"]["default"],
            json!(0.85),
            "threshold default is not a clean decimal"
        );
    }

    // ── The batch trap ───────────────────────────────────────────────

    fn stored_memory(
        supersedes: Option<crate::model::SupersedesOutcome>,
        near_duplicates: Option<crate::model::NearDuplicateReport>,
    ) -> StoredMemory {
        StoredMemory {
            id: "0195e2c0-0000-7000-8000-000000000001".to_string(),
            namespace: "default".to_string(),
            phase: "Full".to_string(),
            strength: 1.0,
            stability: 3.7145,
            created_at: "2026-08-11T12:00:00Z".to_string(),
            supersedes,
            near_duplicates,
        }
    }

    /// `json!` ignores `skip_serializing_if`, so a batch entry is built by
    /// hand and every optional field has to be re-remembered there. This
    /// compares the hand-built entry against `StoredMemory`'s own
    /// serialization: every key the single-memory tool emits must appear
    /// in the batch entry with an equal value.
    ///
    /// It would have caught the original `supersedes` omission, and it
    /// catches the next field somebody adds to `StoredMemory` without
    /// touching `store_result_entry`.
    #[test]
    fn e24_batch_entry_carries_every_field_the_single_store_serializes() {
        let stored = stored_memory(
            Some(crate::model::SupersedesOutcome::new(
                crate::model::MemoryId::new(),
                crate::model::SupersedesStatus::Applied,
            )),
            crate::model::NearDuplicateReport::from_matches(
                0.85,
                vec![crate::model::NearDuplicateMatch {
                    id: crate::model::MemoryId::new(),
                    score: 0.91,
                    summary: "an older phrasing".to_string(),
                }],
            ),
        );

        let direct = serde_json::to_value(&stored).expect("StoredMemory serializes");
        let batch = store_result_entry(7, &stored);

        for (key, value) in direct.as_object().expect("direct output is an object") {
            // `createdAt` is the one renamed key: `StoredMemory` and the
            // batch entry agree on it, so nothing to special-case.
            assert_eq!(
                batch.get(key),
                Some(value),
                "batch entry is missing or disagrees about '{key}'"
            );
        }
        assert_eq!(batch["index"], json!(7));
    }

    /// The other half: an absent optional field must be ABSENT, not
    /// `null`, so the batch shape matches what `store_memory` emits for
    /// the same store.
    #[test]
    fn e25_batch_entry_omits_absent_optional_fields() {
        let batch = store_result_entry(0, &stored_memory(None, None));
        let obj = batch.as_object().expect("entry is an object");

        assert!(!obj.contains_key("supersedes"), "{batch}");
        assert!(!obj.contains_key("nearDuplicates"), "{batch}");
    }

    // ── Batch forget ─────────────────────────────────────────────────

    /// The published bound and the enforced bound are the same constant,
    /// so a client that trusts the schema cannot be told its request was
    /// out of range. `minItems` matters too: an empty array is an error,
    /// not a no-op, and the schema has to say so.
    #[test]
    fn w37_forget_memories_ids_bounds_match_the_batch_constant() {
        let ids = forget_memories_def().input_schema["properties"]["ids"].clone();
        assert_eq!(ids["type"], json!("array"));
        assert_eq!(ids["maxItems"].as_u64(), Some(MAX_BATCH_MEMORIES as u64));
        assert_eq!(ids["minItems"].as_u64(), Some(1));
        assert_eq!(ids["items"]["type"], json!("string"));
        assert_eq!(ids["items"]["format"], json!("uuid"));
        assert_eq!(
            forget_memories_def().input_schema["required"],
            json!(["ids"])
        );
    }

    // ── delete_namespace ─────────────────────────────────────────────

    /// `force` has to advertise its default. A caller that reads the
    /// schema and omits the key must be able to predict which way the
    /// call goes, because the two answers are "refused" and "everything
    /// is destroyed".
    #[test]
    fn w38_delete_namespace_declares_a_required_name_and_force_defaulting_to_false() {
        let schema = delete_namespace_def().input_schema;
        assert_eq!(schema["required"], json!(["name"]));

        let force = &schema["properties"]["force"];
        assert_eq!(force["type"], json!("boolean"));
        assert_eq!(force["default"], json!(false));
        assert!(
            force["description"]
                .as_str()
                .is_some_and(|d| d.contains("no undo")),
            "force must say there is no undo: {force}"
        );

        let name = &schema["properties"]["name"];
        assert_eq!(name["type"], json!("string"));
        assert_eq!(name["minLength"].as_u64(), Some(1));
        assert_eq!(name["maxLength"].as_u64(), Some(64));
    }

    // ── Destructive-tool descriptions ────────────────────────────────

    /// `forget_memory` used to promise a permanent delete. It does not
    /// perform one: the record stays in meta.db, only its content is
    /// stripped, and no code path ever hard-deletes a tombstone. An agent
    /// reading "permanently delete" will believe the row and its disk
    /// space are gone, and will not reach for the tool that actually
    /// reclaims them.
    #[test]
    fn w39_forget_memory_description_does_not_promise_a_permanent_delete() {
        let description = forget_memory_def().description;
        assert!(
            !description.to_lowercase().contains("permanent"),
            "forget_memory still claims a permanent delete: {description}"
        );
        assert!(
            description.contains("Tombstone"),
            "forget_memory does not name the phase the memory moves to: {description}"
        );
        assert!(
            description.contains("never reclaimed"),
            "forget_memory does not say tombstones are never reclaimed: {description}"
        );
    }

    /// The annotation an MCP client uses to decide whether to prompt.
    /// Getting it wrong on these three means a destructive call goes
    /// through unconfirmed.
    #[test]
    fn w41_every_destructive_tool_declares_the_destructive_hint() {
        for def in [
            forget_memory_def(),
            forget_memories_def(),
            delete_namespace_def(),
        ] {
            let annotations = def
                .annotations
                .as_ref()
                .unwrap_or_else(|| panic!("{} has no annotations", def.name));
            assert_eq!(
                annotations.destructive_hint,
                Some(true),
                "{} does not declare itself destructive",
                def.name
            );
            assert_eq!(annotations.read_only_hint, Some(false), "{}", def.name);
        }

        // Deliberately not idempotent, unlike the two forget tools: this
        // removes a directory from disk and the namespace id is never
        // reused, so a blind retry could destroy a same-named namespace
        // recreated in between.
        assert_eq!(
            delete_namespace_def()
                .annotations
                .expect("annotations")
                .idempotent_hint,
            Some(false)
        );
    }

    // ── Registry shape ───────────────────────────────────────────────

    #[test]
    fn t34_every_tool_definition_has_a_unique_name() {
        let mut names: Vec<String> = tool_definitions().into_iter().map(|t| t.name).collect();
        let before = names.len();
        names.sort();
        names.dedup();
        assert_eq!(
            names.len(),
            before,
            "duplicate tool name in tool_definitions"
        );
    }

    /// The count the module doc, the README and the guide all repeat. If
    /// it moves, they have to move with it.
    #[test]
    fn t35_declares_fourteen_tools() {
        assert_eq!(tool_definitions().len(), 14);
    }

    /// A bridge whose every method fails. The point is to reach
    /// [`dispatch_tool`]'s match arms without standing up a real system:
    /// what a handler does after it finds its arm is somebody else's
    /// test, but *finding* the arm is this one's.
    struct Unroutable;

    macro_rules! refuse {
        () => {
            Err(crate::mcp::bridge::BridgeError::Internal(
                "stub".to_string(),
            ))
        };
    }

    #[async_trait::async_trait]
    impl crate::mcp::bridge::SearchPipeline for Unroutable {
        async fn search(
            &self,
            _: crate::mcp::bridge::SearchInput,
        ) -> Result<crate::mcp::bridge::SearchResponse, crate::mcp::bridge::BridgeError> {
            refuse!()
        }
        async fn find_similar(
            &self,
            _: crate::model::MemoryId,
            _: usize,
            _: Option<f32>,
            _: bool,
        ) -> Result<Vec<crate::mcp::bridge::SearchHit>, crate::mcp::bridge::BridgeError> {
            refuse!()
        }
        async fn scan_duplicates(
            &self,
            _: &str,
            _: f32,
            _: usize,
        ) -> Result<Vec<crate::mcp::bridge::DuplicateCluster>, crate::mcp::bridge::BridgeError>
        {
            refuse!()
        }
    }

    #[async_trait::async_trait]
    impl crate::mcp::bridge::StorageEngine for Unroutable {
        async fn store_memory(
            &self,
            _: crate::mcp::bridge::StoreInput,
        ) -> Result<StoredMemory, crate::mcp::bridge::BridgeError> {
            refuse!()
        }
        async fn get_memory(
            &self,
            _: crate::model::MemoryId,
        ) -> Result<Option<crate::mcp::bridge::MemoryRecord>, crate::mcp::bridge::BridgeError>
        {
            refuse!()
        }
        async fn delete_memory(
            &self,
            _: crate::model::MemoryId,
        ) -> Result<bool, crate::mcp::bridge::BridgeError> {
            refuse!()
        }
        async fn delete_memories(
            &self,
            _: &[crate::model::MemoryId],
        ) -> Result<Vec<bool>, crate::mcp::bridge::BridgeError> {
            refuse!()
        }
        async fn reinforce_memory(
            &self,
            _: crate::model::MemoryId,
            _: u8,
        ) -> Result<crate::mcp::bridge::ReinforceResult, crate::mcp::bridge::BridgeError> {
            refuse!()
        }
        async fn list_memories(
            &self,
            _: crate::mcp::bridge::ListMemoriesInput,
        ) -> Result<crate::mcp::bridge::ListMemoriesResponse, crate::mcp::bridge::BridgeError>
        {
            refuse!()
        }
        async fn list_tags(
            &self,
            _: crate::mcp::bridge::ListTagsInput,
        ) -> Result<crate::mcp::bridge::ListTagsResponse, crate::mcp::bridge::BridgeError> {
            refuse!()
        }
    }

    #[async_trait::async_trait]
    impl crate::mcp::bridge::NamespaceRegistry for Unroutable {
        async fn list_namespaces(
            &self,
        ) -> Result<Vec<crate::mcp::bridge::NamespaceInfo>, crate::mcp::bridge::BridgeError>
        {
            refuse!()
        }
        async fn create_namespace(
            &self,
            _: crate::mcp::bridge::CreateNamespaceInput,
        ) -> Result<crate::mcp::bridge::NamespaceInfo, crate::mcp::bridge::BridgeError> {
            refuse!()
        }
        async fn namespace_stats(
            &self,
            _: &str,
        ) -> Result<crate::mcp::bridge::NamespaceStats, crate::mcp::bridge::BridgeError> {
            refuse!()
        }
        async fn delete_namespace(
            &self,
            _: crate::mcp::bridge::DeleteNamespaceInput,
        ) -> Result<crate::mcp::bridge::DeleteNamespaceResult, crate::mcp::bridge::BridgeError>
        {
            refuse!()
        }
    }

    #[async_trait::async_trait]
    impl crate::mcp::bridge::HealthChecker for Unroutable {
        async fn check_health(&self) -> crate::mcp::bridge::HealthStatus {
            unreachable!("no tool reaches the health checker")
        }
    }

    fn unroutable_bridge() -> McpBridge {
        let stub = std::sync::Arc::new(Unroutable);
        McpBridge {
            search: stub.clone(),
            storage: stub.clone(),
            namespaces: stub.clone(),
            health: stub,
            default_namespace: "default".to_string(),
            timezone: chrono_tz::UTC,
        }
    }

    /// [`DISPATCHED`] is hand-maintained next to the match, so it can
    /// drift from the match itself. This one goes through the real
    /// [`dispatch_tool`]: a declared tool with no arm answers "Unknown
    /// tool", and the point of declaring it was that it would not.
    #[tokio::test]
    async fn w40_dispatch_tool_routes_every_declared_tool() {
        let bridge = unroutable_bridge();

        for tool in tool_definitions() {
            let result = dispatch_tool(&bridge, &tool.name, json!({})).await;
            let rendered = serde_json::to_string(&result).expect("result serializes");
            assert!(
                !rendered.contains("Unknown tool"),
                "{} is declared but dispatch_tool does not route it",
                tool.name
            );
        }

        let unknown = dispatch_tool(&bridge, "no_such_tool", json!({})).await;
        assert!(
            serde_json::to_string(&unknown)
                .expect("result serializes")
                .contains("Unknown tool"),
            "the unknown-tool path stopped reporting unknown tools"
        );
    }

    /// A definition without a dispatch arm advertises a tool that answers
    /// "Unknown tool". Cheap to catch here, expensive to catch in the
    /// field.
    #[test]
    fn t36_every_declared_tool_has_a_dispatch_arm() {
        for tool in tool_definitions() {
            assert!(
                DISPATCHED.contains(&tool.name.as_str()),
                "{} is declared but has no dispatch arm",
                tool.name
            );
        }
        assert_eq!(DISPATCHED.len(), tool_definitions().len());
    }

    /// The published bound and the enforced bound are the same constant,
    /// so a client that trusts the schema cannot be told its request was
    /// out of range.
    #[test]
    fn t37_listing_schema_limits_match_their_constants() {
        let list = list_memories_def().input_schema;
        assert_eq!(
            list["properties"]["limit"]["maximum"].as_u64(),
            Some(MAX_LIST_LIMIT as u64)
        );
        assert_eq!(
            list["properties"]["limit"]["default"].as_u64(),
            Some(DEFAULT_LIST_LIMIT as u64)
        );
        assert_eq!(list["properties"]["limit"]["minimum"].as_u64(), Some(1));

        let tags = list_tags_def().input_schema;
        assert_eq!(
            tags["properties"]["limit"]["maximum"].as_u64(),
            Some(MAX_TAG_LIMIT as u64)
        );
        assert_eq!(
            tags["properties"]["limit"]["default"].as_u64(),
            Some(DEFAULT_TAG_LIMIT as u64)
        );
        assert_eq!(tags["properties"]["limit"]["minimum"].as_u64(), Some(1));
    }

    #[test]
    fn t38_read_only_tools_declare_the_read_only_hint() {
        const READ_ONLY: [&str; 7] = [
            "recall_memories",
            "get_memory",
            "find_similar_memories",
            "namespace_stats",
            "list_memories",
            "list_namespaces",
            "list_tags",
        ];
        for tool in tool_definitions() {
            let Some(annotations) = tool.annotations else {
                panic!("{} has no annotations", tool.name);
            };
            let expected = READ_ONLY.contains(&tool.name.as_str());
            assert_eq!(
                annotations.read_only_hint,
                Some(expected),
                "{} read_only_hint",
                tool.name
            );
        }
    }

    #[test]
    fn t39_list_namespaces_takes_no_parameters() {
        let schema = list_namespaces_def().input_schema;
        assert_eq!(schema["type"].as_str(), Some("object"));
        assert_eq!(
            schema["properties"].as_object().map(|o| o.len()),
            Some(0),
            "list_namespaces must not require the caller to know anything first"
        );
    }

    /// Both scope switches must be discoverable from the schema, or a
    /// caller cannot know that global scope exists -- nor that the two
    /// cannot be combined.
    #[test]
    fn t40_list_tags_declares_both_scope_switches() {
        let props = list_tags_def().input_schema["properties"].clone();
        assert_eq!(props["namespace"]["type"].as_str(), Some("string"));
        assert_eq!(props["allNamespaces"]["type"].as_str(), Some("boolean"));
        assert_eq!(props["allNamespaces"]["default"].as_bool(), Some(false));
        assert!(
            props["allNamespaces"]["description"]
                .as_str()
                .unwrap_or_default()
                .contains("Cannot be combined"),
            "the mutual exclusion has to be in the schema, not just the error"
        );
    }

    // ── partial namespace purge ──────────────────────────────────────

    /// A purge that destroyed 2,000 memories and then failed is not a
    /// delete that failed. Rendering it as "Failed to delete namespace"
    /// tells the agent nothing happened, so it moves on and the
    /// half-purged namespace is never finished — and the memories are
    /// gone either way.
    #[tokio::test]
    async fn w52_a_partial_purge_says_partially_destroyed_not_failed() {
        let bridge = crate::test_support::StubBridge {
            delete_namespace_error: Some(crate::mcp::bridge::BridgeError::PartiallyApplied(
                "Namespace 'scratch' was PARTIALLY destroyed: 2000 memories are \
                 permanently gone. Re-run delete_namespace to finish it."
                    .to_string(),
            )),
        }
        .into_bridge();

        let result =
            handle_delete_namespace(&bridge, json!({ "name": "scratch", "force": true })).await;

        assert_eq!(result.is_error, Some(true));
        let text = tool_text(&result);
        assert!(
            text.contains("PARTIALLY destroyed") && text.contains("Re-run"),
            "{text}"
        );
        assert!(
            !text.contains("Failed to delete namespace"),
            "a partial purge must not be reported as a plain failure: {text}"
        );
    }

    /// The other direction: an ordinary failure keeps the prefix that
    /// says nothing was destroyed.
    #[tokio::test]
    async fn w53_an_ordinary_delete_failure_keeps_the_failed_prefix() {
        let bridge = crate::test_support::StubBridge {
            delete_namespace_error: Some(crate::mcp::bridge::BridgeError::Storage(
                "disk on fire".to_string(),
            )),
        }
        .into_bridge();

        let result =
            handle_delete_namespace(&bridge, json!({ "name": "scratch", "force": true })).await;

        assert_eq!(result.is_error, Some(true));
        assert!(
            tool_text(&result).contains("Failed to delete namespace"),
            "{:?}",
            tool_text(&result)
        );
    }

    // ── strict argument checking on the search tools ─────────────────

    /// `docs/mcp.md` claims `recall_memories` validates strictly; only
    /// its `namespace` was retrofitted. Everything here used to be a
    /// silent default, and the flagship case is the first one: a
    /// comma-joined `tags` string became `[]` and the recall ran
    /// UNFILTERED over the whole namespace, which is a *wider* answer
    /// than was asked for, returned as a success.
    #[tokio::test]
    async fn w64_recall_memories_rejects_every_malformed_argument() {
        let bridge = crate::test_support::StubBridge::default().into_bridge();

        let cases = [
            ("tags", json!({ "query": "q", "tags": "topic/rust" })),
            ("entities", json!({ "query": "q", "entities": "Sarah" })),
            ("topics", json!({ "query": "q", "topics": "rust" })),
            ("emotions", json!({ "query": "q", "emotions": "happy" })),
            ("limit", json!({ "query": "q", "limit": 1000 })),
            ("limit", json!({ "query": "q", "limit": "10" })),
            ("depth", json!({ "query": "q", "depth": 5 })),
            ("compact", json!({ "query": "q", "compact": "false" })),
            ("minStrength", json!({ "query": "q", "minStrength": 5 })),
            (
                "timeRangeStart",
                json!({ "query": "q", "timeRangeStart": ["x"] }),
            ),
            ("query", json!({})),
        ];

        for (field, arguments) in cases {
            // The stub's `search` is `unreachable!()`, so reaching the
            // bridge at all is the failure: it means the handler ran the
            // query instead of rejecting the arguments.
            let result = handle_recall_memories(&bridge, arguments.clone()).await;
            assert_eq!(
                result.is_error,
                Some(true),
                "{field} was accepted: {arguments}"
            );
            assert!(
                tool_text(&result).contains(field),
                "the error must name the field: {}",
                tool_text(&result)
            );
        }
    }

    #[tokio::test]
    async fn w65_find_similar_memories_rejects_every_malformed_argument() {
        let bridge = crate::test_support::StubBridge::default().into_bridge();
        let id = crate::model::MemoryId::new().to_string();

        let cases = [
            ("mode", json!({ "mode": 2, "id": id })),
            ("mode", json!({ "mode": "sngle", "id": id })),
            ("id", json!({ "mode": "single" })),
            ("id", json!({ "mode": "single", "id": "not-a-uuid" })),
            (
                "limit",
                json!({ "mode": "single", "id": id, "limit": 1000 }),
            ),
            (
                "minScore",
                json!({ "mode": "single", "id": id, "minScore": 5 }),
            ),
            (
                "sameNamespace",
                json!({ "mode": "single", "id": id, "sameNamespace": "true" }),
            ),
            ("threshold", json!({ "mode": "scan", "threshold": 5 })),
            ("namespace", json!({ "mode": "scan", "namespace": ["a"] })),
        ];

        for (field, arguments) in cases {
            let result = handle_find_similar_memories(&bridge, arguments.clone()).await;
            assert_eq!(
                result.is_error,
                Some(true),
                "{field} was accepted: {arguments}"
            );
            assert!(
                tool_text(&result).contains(field),
                "the error must name the field: {}",
                tool_text(&result)
            );
        }
    }

    /// The text of a tool result, joined across content blocks.
    fn tool_text(result: &ToolCallResult) -> String {
        result
            .content
            .iter()
            .map(|c| match c {
                crate::mcp::protocol::ContentBlock::Text { text } => text.as_str(),
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}
