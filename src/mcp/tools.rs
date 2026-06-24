//! MCP tool definitions and handlers for Recalld memory operations.
//!
//! Defines 7 tools: store_memory, recall_memories, get_memory,
//! reinforce_memory, forget_memory, find_similar_memories, create_namespace.

use serde_json::json;

use crate::mcp::bridge::McpBridge;
use crate::mcp::protocol::{ToolAnnotations, ToolCallResult, ToolInfo};

// ═══════════════════════════════════════════════════════════════════════
// Registry and dispatch
// ═══════════════════════════════════════════════════════════════════════

/// Return all tool definitions for `tools/list`.
pub fn tool_definitions() -> Vec<ToolInfo> {
    vec![
        store_memory_def(),
        recall_memories_def(),
        get_memory_def(),
        reinforce_memory_def(),
        forget_memory_def(),
        find_similar_memories_def(),
        create_namespace_def(),
    ]
}

/// Dispatch a tool call by name to the appropriate handler.
pub async fn dispatch_tool(
    bridge: &McpBridge,
    name: &str,
    arguments: serde_json::Value,
) -> ToolCallResult {
    match name {
        "store_memory" => handle_store_memory(bridge, arguments).await,
        "recall_memories" => handle_recall_memories(bridge, arguments).await,
        "get_memory" => handle_get_memory(bridge, arguments).await,
        "reinforce_memory" => handle_reinforce_memory(bridge, arguments).await,
        "forget_memory" => handle_forget_memory(bridge, arguments).await,
        "find_similar_memories" => handle_find_similar_memories(bridge, arguments).await,
        "create_namespace" => handle_create_namespace(bridge, arguments).await,
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
        description: "Store a new observation, fact, or piece of context as a memory. \
            The system automatically generates an embedding for semantic search. \
            Use tags to categorize (e.g., \"topic/rust\", \"project/recalld\"). \
            Memories decay naturally over time unless reinforced."
            .to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "summary": {
                    "type": "string",
                    "description": "Short description of the memory (max 2000 chars)",
                    "maxLength": 2000
                },
                "fullText": {
                    "type": "string",
                    "description": "Detailed content. Removed as memory decays to ghost phase."
                },
                "tags": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Categorization tags, e.g. [\"topic/rust\", \"type/observation\"]"
                },
                "entities": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Named entities (people, places, orgs, titles) mentioned in this memory. Used for search indexing and graph linking."
                },
                "topics": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Topic keywords describing what the memory is about, e.g. [\"rust\", \"cooking\", \"career\"]"
                },
                "emotions": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Emotional tone if relevant, e.g. [\"happy\", \"anxious\", \"grateful\"]"
                },
                "namespace": {
                    "type": "string",
                    "description": "Memory partition (default: \"default\")",
                    "default": "default"
                },
                "parentId": {
                    "type": "string",
                    "description": "UUID of parent memory to create a hierarchical link"
                },
                "supersedes": {
                    "type": "string",
                    "description": "UUID of an older memory this one replaces. The old memory will be deprioritized in search results in favor of this one."
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
    let summary = match arguments.get("summary").and_then(|v| v.as_str()) {
        Some(s) => s.to_string(),
        None => return ToolCallResult::error("Missing required parameter: summary"),
    };

    // Issue 2: Enforce summary length limit
    if summary.len() > 2000 {
        return ToolCallResult::error("Summary exceeds maximum length of 2000 characters");
    }

    let full_text = arguments
        .get("fullText")
        .and_then(|v| v.as_str())
        .map(String::from);

    // Issue 2: Enforce full_text length limit (1 MB)
    const MAX_FULL_TEXT_BYTES: usize = 1_048_576;
    if let Some(ref ft) = full_text {
        if ft.len() > MAX_FULL_TEXT_BYTES {
            return ToolCallResult::error("fullText exceeds maximum length of 1 MB");
        }
    }

    let tags: Vec<String> = arguments
        .get("tags")
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default();
    let entities: Vec<String> = arguments
        .get("entities")
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default();
    let topics: Vec<String> = arguments
        .get("topics")
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default();
    let emotions: Vec<String> = arguments
        .get("emotions")
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default();

    // Issue 3: Enforce array size limits
    if tags.len() > 64 {
        return ToolCallResult::error("Too many tags (maximum 64)");
    }
    if entities.len() > 32 {
        return ToolCallResult::error("Too many entities (maximum 32)");
    }
    if topics.len() > 32 {
        return ToolCallResult::error("Too many topics (maximum 32)");
    }
    if emotions.len() > 32 {
        return ToolCallResult::error("Too many emotions (maximum 32)");
    }
    let namespace = arguments
        .get("namespace")
        .and_then(|v| v.as_str())
        .map(String::from)
        .unwrap_or_else(|| bridge.default_namespace().to_string());
    let parent_id = arguments
        .get("parentId")
        .and_then(|v| v.as_str())
        .and_then(|s| uuid::Uuid::parse_str(s).ok())
        .map(crate::model::MemoryId::from_uuid);
    let supersedes = arguments
        .get("supersedes")
        .and_then(|v| v.as_str())
        .and_then(|s| uuid::Uuid::parse_str(s).ok())
        .map(crate::model::MemoryId::from_uuid);

    let input = crate::mcp::bridge::StoreInput {
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
// Tool 2: recall_memories
// ═══════════════════════════════════════════════════════════════════════

fn recall_memories_def() -> ToolInfo {
    ToolInfo {
        name: "recall_memories".to_string(),
        title: Some("Recall Memories".to_string()),
        description: "Search for relevant memories using a natural language query. \
            Returns memories ranked by semantic similarity combined with memory \
            strength (well-reinforced memories rank higher). This is the primary \
            way to retrieve context from the memory system."
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
    let query = match arguments.get("query").and_then(|v| v.as_str()) {
        Some(s) => s.to_string(),
        None => return ToolCallResult::error("Missing required parameter: query"),
    };

    let limit = arguments
        .get("limit")
        .and_then(|v| v.as_u64())
        .unwrap_or(10) as usize;
    let namespace = arguments
        .get("namespace")
        .and_then(|v| v.as_str())
        .map(String::from)
        .unwrap_or_else(|| bridge.default_namespace().to_string());
    let tags: Vec<String> = arguments
        .get("tags")
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default();
    let entities: Vec<String> = arguments
        .get("entities")
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default();
    let topics: Vec<String> = arguments
        .get("topics")
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default();
    let emotions: Vec<String> = arguments
        .get("emotions")
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default();
    let min_strength = arguments
        .get("minStrength")
        .and_then(|v| v.as_f64())
        .map(|f| f as f32);
    let depth = arguments.get("depth").and_then(|v| v.as_u64()).unwrap_or(0) as u32;

    // Parse time range values: accept either integer (millis) or ISO 8601 string.
    let time_range_start = match arguments.get("timeRangeStart") {
        Some(v) => match crate::time::parse_time_value(v) {
            Some(Ok(ms)) => Some(ms),
            Some(Err(e)) => return ToolCallResult::error(format!("Invalid timeRangeStart: {e}")),
            None => None,
        },
        None => None,
    };
    let time_range_end = match arguments.get("timeRangeEnd") {
        Some(v) => match crate::time::parse_time_value(v) {
            Some(Ok(ms)) => Some(ms),
            Some(Err(e)) => return ToolCallResult::error(format!("Invalid timeRangeEnd: {e}")),
            None => None,
        },
        None => None,
    };

    let input = crate::mcp::bridge::SearchInput {
        query,
        namespace,
        limit: limit.min(100),
        tags,
        entities,
        topics,
        emotions,
        min_strength,
        depth,
        time_range_start,
        time_range_end,
    };

    match bridge.search.search(input).await {
        Ok(search_response) => {
            let hit_count = search_response.hits.len();
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
        description: "Permanently delete a memory. Use for incorrect, outdated, \
            or harmful information that should be immediately removed rather \
            than allowed to decay naturally."
            .to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "id": {
                    "type": "string",
                    "description": "Memory UUID to delete"
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
// Tool 6: find_similar_memories
// ═══════════════════════════════════════════════════════════════════════

fn find_similar_memories_def() -> ToolInfo {
    ToolInfo {
        name: "find_similar_memories".to_string(),
        title: Some("Find Similar Memories".to_string()),
        description: "Find memories semantically similar to a specific existing \
            memory. Useful for exploring related context, finding potential \
            contradictions, or discovering duplicates."
            .to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "id": {
                    "type": "string",
                    "description": "Source memory UUID"
                },
                "limit": {
                    "type": "integer",
                    "description": "Maximum results (default: 10, max: 100)",
                    "default": 10,
                    "minimum": 1,
                    "maximum": 100
                },
                "minScore": {
                    "type": "number",
                    "description": "Minimum similarity threshold (0.0-1.0)"
                },
                "sameNamespace": {
                    "type": "boolean",
                    "description": "Restrict to same namespace (default: true)",
                    "default": true
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

async fn handle_find_similar_memories(
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

    let limit = arguments
        .get("limit")
        .and_then(|v| v.as_u64())
        .unwrap_or(10) as usize;
    let min_score = arguments
        .get("minScore")
        .and_then(|v| v.as_f64())
        .map(|f| f as f32);
    let same_namespace = arguments
        .get("sameNamespace")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);

    match bridge
        .search
        .find_similar(id, limit.min(100), min_score, same_namespace)
        .await
    {
        Ok(hits) => {
            let response = json!({
                "sourceId": id_str,
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

    // Issue 1: Server-side validation of namespace name
    if name.is_empty()
        || name.len() > 64
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return ToolCallResult::error(
            "Invalid namespace name: must be 1-64 characters, alphanumeric, hyphens, or underscores only",
        );
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
