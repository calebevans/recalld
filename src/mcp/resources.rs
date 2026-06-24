//! MCP resource definitions and handlers for Recalld.
//!
//! Defines 3 resources: 2 static (namespaces, health) and 1 template
//! (namespace stats by name).

use crate::mcp::bridge::McpBridge;
use crate::mcp::protocol::{ResourceContent, ResourceInfo, ResourceReadResult, ResourceTemplate};

// ═══════════════════════════════════════════════════════════════════════
// Registry
// ═══════════════════════════════════════════════════════════════════════

/// Return static resource definitions for `resources/list`.
pub fn resource_definitions() -> Vec<ResourceInfo> {
    vec![
        ResourceInfo {
            uri: "recalld://namespaces".to_string(),
            name: "Memory Namespaces".to_string(),
            description: Some(
                "List of all memory namespaces with names, IDs, \
                 embedding dimensions, and memory counts."
                    .to_string(),
            ),
            mime_type: Some("application/json".to_string()),
        },
        ResourceInfo {
            uri: "recalld://health".to_string(),
            name: "System Health".to_string(),
            description: Some(
                "Current health status of the Recalld memory system \
                 including subsystem status and uptime."
                    .to_string(),
            ),
            mime_type: Some("application/json".to_string()),
        },
    ]
}

/// Return resource templates for `resources/templates/list`.
pub fn resource_template_definitions() -> Vec<ResourceTemplate> {
    vec![ResourceTemplate {
        uri_template: "recalld://namespaces/{name}/stats".to_string(),
        name: "Namespace Statistics".to_string(),
        description: Some(
            "Detailed statistics for a namespace: memory count, \
             phase distribution, permastore count, average strength."
                .to_string(),
        ),
        mime_type: Some("application/json".to_string()),
    }]
}

// ═══════════════════════════════════════════════════════════════════════
// Dispatch
// ═══════════════════════════════════════════════════════════════════════

/// Dispatch a resource read by URI.
pub async fn dispatch_resource(
    bridge: &McpBridge,
    uri: &str,
) -> Result<ResourceReadResult, String> {
    match uri {
        "recalld://namespaces" => read_namespaces(bridge).await,
        "recalld://health" => read_health(bridge).await,
        _ if uri.starts_with("recalld://namespaces/") && uri.ends_with("/stats") => {
            // Extract namespace name from URI: recalld://namespaces/{name}/stats
            let name = uri
                .strip_prefix("recalld://namespaces/")
                .and_then(|s| s.strip_suffix("/stats"))
                .ok_or_else(|| format!("Invalid resource URI: {uri}"))?;
            read_namespace_stats(bridge, name).await
        }
        _ => Err(format!("Unknown resource: {uri}")),
    }
}

// ═══════════════════════════════════════════════════════════════════════
// Handlers
// ═══════════════════════════════════════════════════════════════════════

/// Read all namespaces.
async fn read_namespaces(bridge: &McpBridge) -> Result<ResourceReadResult, String> {
    let namespaces = bridge
        .namespaces
        .list_namespaces()
        .await
        .map_err(|e| format!("Failed to list namespaces: {e}"))?;

    let text = serde_json::to_string_pretty(&namespaces)
        .map_err(|e| format!("Serialization error: {e}"))?;

    Ok(ResourceReadResult {
        contents: vec![ResourceContent {
            uri: "recalld://namespaces".to_string(),
            mime_type: Some("application/json".to_string()),
            text: Some(text),
        }],
    })
}

/// Read system health status.
async fn read_health(bridge: &McpBridge) -> Result<ResourceReadResult, String> {
    let health = bridge.health.check_health().await;

    let text = serde_json::to_string_pretty(&health)
        .map_err(|e| format!("Serialization error: {e}"))?;

    Ok(ResourceReadResult {
        contents: vec![ResourceContent {
            uri: "recalld://health".to_string(),
            mime_type: Some("application/json".to_string()),
            text: Some(text),
        }],
    })
}

/// Read statistics for a specific namespace.
async fn read_namespace_stats(
    bridge: &McpBridge,
    name: &str,
) -> Result<ResourceReadResult, String> {
    let stats = bridge
        .namespaces
        .namespace_stats(name)
        .await
        .map_err(|e| format!("Failed to get namespace stats: {e}"))?;

    let text = serde_json::to_string_pretty(&stats)
        .map_err(|e| format!("Serialization error: {e}"))?;

    Ok(ResourceReadResult {
        contents: vec![ResourceContent {
            uri: format!("recalld://namespaces/{name}/stats"),
            mime_type: Some("application/json".to_string()),
            text: Some(text),
        }],
    })
}
