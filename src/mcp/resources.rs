//! MCP resource definitions and handlers for Recalld.
//!
//! Defines 4 resources: 3 static (namespaces, health, session) and 1 template
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
            uri: "recalld://session".to_string(),
            name: "Session Context".to_string(),
            description: Some("Resolved connection default and current v1 limits; namespaces are organizational, not access controls.".to_string()),
            mime_type: Some("application/json".to_string()),
        },
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
        "recalld://session" => read_session(bridge).await,
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

/// Resolve the frozen bridge default without creating a missing namespace.
/// Resource reads are private and immediately stale at the server boundary.
async fn read_session(bridge: &McpBridge) -> Result<ResourceReadResult, String> {
    use crate::mcp::bridge::{DUPLICATE_NEIGHBOR_CAP, DUPLICATE_SAMPLE_CAP};
    use crate::model::constants::{MAX_BATCH_MEMORIES, MAX_LIST_LIMIT, MAX_TAG_LIMIT};

    let requested = bridge.default_namespace();
    let namespaces = bridge
        .namespaces
        .list_namespaces()
        .await
        .map_err(|e| format!("Failed to resolve session namespace: {e}"))?;
    let namespace = namespaces
        .iter()
        .find(|ns| ns.name.eq_ignore_ascii_case(requested));
    let context = serde_json::json!({
        "supportedContractVersions": [1],
        "defaultNamespace": {
            "requestedName": requested,
            // The bridge does not retain whether the value came from a file or fallback.
            "source": "bridge",
            "exists": namespace.is_some(),
            "resolved": namespace.map(|ns| serde_json::json!({
                "name": ns.name, "id": ns.id, "embeddingDim": ns.embedding_dim
            }))
        },
        "namespacesAreAccessControls": false,
        "limits": { "batchMemories": MAX_BATCH_MEMORIES, "listMemories": MAX_LIST_LIMIT, "tagsPerBucket": MAX_TAG_LIMIT },
        "duplicateScan": {
            "algorithm": "sampledNearestNeighbors",
            "sampleCap": DUPLICATE_SAMPLE_CAP,
            "neighborCap": DUPLICATE_NEIGHBOR_CAP,
            "complete": false
        }
    });
    Ok(ResourceReadResult {
        contents: vec![ResourceContent {
            uri: "recalld://session".into(),
            mime_type: Some("application/json".into()),
            text: Some(serde_json::to_string_pretty(&context).map_err(|e| e.to_string())?),
        }],
    })
}

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

    let text =
        serde_json::to_string_pretty(&health).map_err(|e| format!("Serialization error: {e}"))?;

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

    let text =
        serde_json::to_string_pretty(&stats).map_err(|e| format!("Serialization error: {e}"))?;

    Ok(ResourceReadResult {
        contents: vec![ResourceContent {
            uri: format!("recalld://namespaces/{name}/stats"),
            mime_type: Some("application/json".to_string()),
            text: Some(text),
        }],
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::protocol::LEGACY_PROTOCOL_VERSION;
    use crate::mcp::server::{
        McpServer,
        tests::{initialize_request, modern_request, plain_request},
    };
    use serde_json::{Value, json};
    use std::sync::Arc;

    #[tokio::test]
    async fn session_context_resolves_each_bridge_without_public_caching_or_invented_identity() {
        let f = crate::test_support::Fixture::new();
        let project = f.create_namespace("Project");
        for modern in [false, true] {
            for requested in ["default", "project", "missing"] {
                let mut bridge = crate::test_support::StubBridge::default().into_bridge();
                bridge.namespaces = Arc::new(f.namespace_adapter());
                bridge.default_namespace = requested.to_string();
                let mut server = McpServer::new(Arc::new(bridge));
                if !modern {
                    assert!(
                        server
                            .handle_message(initialize_request(1, LEGACY_PROTOCOL_VERSION))
                            .await
                            .unwrap()
                            .error
                            .is_none()
                    );
                }
                let params = json!({"uri": "recalld://session"});
                let request = if modern {
                    modern_request(2, "resources/read", params)
                } else {
                    plain_request(2, "resources/read", Some(params))
                };
                let response = server.handle_message(request).await.unwrap();
                assert!(response.error.is_none(), "{response:?}");
                let result = response.result.unwrap();
                assert_eq!(result["cacheScope"], "private");
                assert_eq!(result["ttlMs"], 0);
                let text = result["contents"][0]["text"].as_str().unwrap();
                let context: Value = serde_json::from_str(text).unwrap();
                assert_eq!(context["supportedContractVersions"], json!([1]));
                assert_eq!(context["defaultNamespace"]["requestedName"], requested);
                assert_eq!(context["duplicateScan"]["complete"], false);
                assert_eq!(
                    context["defaultNamespace"]["exists"],
                    requested != "missing"
                );
                match requested {
                    "project" => {
                        assert_eq!(context["defaultNamespace"]["resolved"]["name"], "Project");
                        assert_eq!(context["defaultNamespace"]["resolved"]["id"], project.get());
                    }
                    "missing" => assert!(context["defaultNamespace"]["resolved"].is_null()),
                    _ => assert_eq!(context["defaultNamespace"]["resolved"]["name"], "default"),
                }
                for unsupported in [
                    "storeId",
                    "generation",
                    "profileId",
                    "receipt",
                    "recordAccess",
                    "idempotency",
                ] {
                    assert!(!text.contains(unsupported), "{text}");
                }
                assert!(!text.contains(f.path().to_str().unwrap()));
            }
        }
        use crate::storage::StorageEngine as _;
        assert!(
            f.storage
                .read()
                .unwrap()
                .get_namespace_by_name("missing")
                .unwrap()
                .is_none()
        );
        assert!(
            resource_definitions()
                .iter()
                .any(|r| r.uri == "recalld://session")
        );
    }
}
