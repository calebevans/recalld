//! Shared preparation for actual MCP/direct and daemon batch consumers.
//! Request identity is complete before resolving any mutable namespace policy.
use super::{args, bridge};
use crate::model::MemoryId;
use crate::storage::batch::{BatchError, BatchInput, BatchItemPlan, BatchKind};
use crate::storage::control::{self, MutationContext, MutationScope};
use crate::storage::insertion::InsertMemory;
use serde_json::Value;

pub(crate) struct PreparedItem {
    pub plan: BatchItemPlan,
    pub store: Option<bridge::StoreInput>,
    pub forget: Option<MemoryId>,
}

pub(crate) fn prepare(input: &BatchInput) -> Result<Vec<PreparedItem>, bridge::BridgeError> {
    if input.result_version != 1
        || input.items.is_empty()
        || input.items.len() > crate::model::constants::MAX_BATCH_MEMORIES
    {
        return Err(bridge::BridgeError::InvalidInput(
            "batch resultVersion must be 1 and items must be a bounded nonempty array".into(),
        ));
    }
    input
        .context()
        .validate()
        .map_err(|e| bridge::BridgeError::InvalidInput(e.to_string()))?;
    if input.batch_operation_id.is_some_and(|id| id.is_nil())
        || input.batch_operation_id.is_some()
            && (input.store_id.is_none() || input.idempotency_key.is_some())
    {
        return Err(bridge::BridgeError::InvalidInput(
            "batchOperationId resume requires storeId and no idempotencyKey".into(),
        ));
    }
    input
        .items
        .iter()
        .enumerate()
        .map(|(index, value)| match input.kind {
            BatchKind::Insert => {
                let parsed = parse_store(value, index, &input.default_namespace);
                let request_hash = item_hash(
                    input.kind,
                    value,
                    parsed.as_ref().ok(),
                    &input.default_namespace,
                )?;
                match parsed {
                    Ok(store) => {
                        let (_, record) = canonical_input(&store);
                        Ok(PreparedItem {
                            plan: BatchItemPlan::Insert {
                                request_hash,
                                namespace: store.namespace.clone(),
                                input: record,
                            },
                            store: Some(store),
                            forget: None,
                        })
                    }
                    Err(error) => Ok(PreparedItem {
                        plan: BatchItemPlan::Invalid {
                            request_hash,
                            error: BatchError {
                                code: "INVALID_INPUT".into(),
                                field: Some(error.field),
                            },
                        },
                        store: None,
                        forget: None,
                    }),
                }
            }
            BatchKind::Forget => {
                let request_hash = item_hash(input.kind, value, None, &input.default_namespace)?;
                let id = value
                    .as_str()
                    .and_then(|s| uuid::Uuid::parse_str(s).ok())
                    .map(MemoryId::from_uuid);
                Ok(match id {
                    Some(id) => PreparedItem {
                        plan: BatchItemPlan::Forget { request_hash, id },
                        store: None,
                        forget: Some(id),
                    },
                    None => PreparedItem {
                        plan: BatchItemPlan::Invalid {
                            request_hash,
                            error: BatchError {
                                code: "INVALID_INPUT".into(),
                                field: Some(format!("ids[{index}]")),
                            },
                        },
                        store: None,
                        forget: None,
                    },
                })
            }
        })
        .collect()
}

fn parse_store(
    value: &Value,
    index: usize,
    default_namespace: &str,
) -> Result<bridge::StoreInput, args::ArgError> {
    let mut input = args::parse_store_item(value, index, default_namespace)?;
    for field in [
        "id",
        "createdAt",
        "namespaceId",
        "storeId",
        "idempotencyKey",
        "batchOperationId",
        "expectedGeneration",
        "contractVersion",
    ] {
        if value.get(field).is_some() {
            return Err(args::ArgError {
                field: format!("memories[{index}].{field}"),
                message:
                    "canonical identity/version fields are not accepted on a legacy batch item"
                        .into(),
            });
        }
    }
    // The original MCP-only parser requests provider generation. The shared
    // daemon/direct batch boundary also accepts explicit StoreInput fields and
    // must not silently erase them before hashing or staging.
    if let Some(value) = value.get("embedding").filter(|v| !v.is_null()) {
        input.embedding =
            Some(
                serde_json::from_value(value.clone()).map_err(|_| args::ArgError {
                    field: format!("memories[{index}].embedding"),
                    message: "embedding must be an array of numbers".into(),
                })?,
            );
    }
    if let Some(value) = value.get("initialStability").filter(|v| !v.is_null()) {
        input.initial_stability =
            Some(
                serde_json::from_value(value.clone()).map_err(|_| args::ArgError {
                    field: format!("memories[{index}].initialStability"),
                    message: "initialStability must be a number".into(),
                })?,
            );
    }
    Ok(input)
}

/// Reuse the single-write normalization for structured tags and canonical input.
/// Original supplied float bits stay in InsertMemory; working normalization is
/// performed later on a separate copy by the shared storage validator.
pub(crate) fn canonical_input(input: &bridge::StoreInput) -> (Vec<String>, InsertMemory) {
    use crate::model::constants::{EMOTION_TAG_PREFIX, ENTITY_TAG_PREFIX, TOPIC_TAG_PREFIX};
    let mut tags = input.tags.clone();
    for (values, prefix) in [
        (&input.entities, ENTITY_TAG_PREFIX),
        (&input.topics, TOPIC_TAG_PREFIX),
        (&input.emotions, EMOTION_TAG_PREFIX),
    ] {
        for value in values {
            let tag = format!("{prefix}{}", value.to_lowercase());
            if !tags.contains(&tag) {
                tags.push(tag);
            }
        }
    }
    let parsed = crate::model::parse_tags_lossy("store_memory.tags", &tags);
    let record = InsertMemory {
        id: None,
        summary: input.summary.clone(),
        full_text: input.full_text.clone(),
        tags: parsed,
        initial_stability: input.initial_stability,
        created_at: None,
        embedding: input.embedding.clone(),
        parent_id: input.parent_id,
        supersedes: input.supersedes,
    };
    (tags, record)
}

fn item_hash(
    kind: BatchKind,
    original: &Value,
    parsed: Option<&bridge::StoreInput>,
    default_namespace: &str,
) -> Result<[u8; 32], bridge::BridgeError> {
    let encoded = (|| -> Result<Vec<u8>, crate::storage::StorageError> {
        if let Some(input) = parsed {
            // Fixed semantic defaults expand before hashing. Mutable policy
            // defaults stay explicit None until the manifest freezes them.
            let scope = match original.get("namespace") {
                None | Some(Value::Null) => ("defaulted", default_namespace),
                _ => ("explicit", input.namespace.as_str()),
            };
            control::encode(&(
                kind,
                scope,
                &input.summary,
                &input.full_text,
                &input.tags,
                &input.entities,
                &input.topics,
                &input.emotions,
                input
                    .embedding
                    .as_ref()
                    .map(|v| v.iter().map(|x| x.to_bits()).collect::<Vec<_>>()),
                input.initial_stability.map(f32::to_bits),
                input.parent_id,
                input.supersedes,
                input.check_duplicates.unwrap_or(true),
                unknown_fields(original),
            ))
        } else {
            // Invalid items still have immutable caller identity. No invalid
            // payload is retained; only this fingerprint and a bounded code.
            let scope = if kind == BatchKind::Insert
                && original.get("namespace").is_none_or(Value::is_null)
            {
                Some(("defaulted", default_namespace))
            } else {
                None
            };
            control::encode(&(kind, scope, canonical_json(original)))
        }
    })()
    .map_err(|e| bridge::BridgeError::InvalidInput(e.to_string()))?;
    let ctx = MutationContext {
        scope: MutationScope::Catalog,
        ..MutationContext::unkeyed(crate::model::NamespaceId::new(1))
    };
    Ok(control::request_hash(
        &ctx,
        "batch_item_intent_v1",
        &[&encoded],
    ))
}

fn unknown_fields(value: &Value) -> Value {
    const KNOWN: &[&str] = &[
        "summary",
        "fullText",
        "tags",
        "entities",
        "topics",
        "emotions",
        "namespace",
        "embedding",
        "initialStability",
        "parentId",
        "supersedes",
        "checkDuplicates",
    ];
    let fields = value.as_object().expect("successfully parsed store item");
    canonical_json(&Value::Object(
        fields
            .iter()
            .filter(|(key, _)| !KNOWN.contains(&key.as_str()))
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect(),
    ))
}

fn canonical_json(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut entries: Vec<_> = map.iter().collect();
            entries.sort_by_key(|(key, _)| *key);
            Value::Object(
                entries
                    .into_iter()
                    .map(|(key, value)| (key.clone(), canonical_json(value)))
                    .collect(),
            )
        }
        Value::Array(values) => Value::Array(values.iter().map(canonical_json).collect()),
        _ => value.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn request(namespace: &str, value: Value) -> BatchInput {
        BatchInput {
            result_version: 1,
            kind: BatchKind::Insert,
            items: vec![value],
            default_namespace: namespace.into(),
            store_id: None,
            idempotency_key: None,
            batch_operation_id: None,
        }
    }
    #[test]
    fn caller_identity_includes_effective_default_scope_but_never_rebinds_explicit_scope() {
        let one = request(
            "one",
            serde_json::json!({"summary":"s","embedding":[1.0,0.0]}),
        );
        let two = request("two", one.items[0].clone());
        assert_ne!(
            prepare(&one).unwrap()[0].plan.request_hash(),
            prepare(&two).unwrap()[0].plan.request_hash()
        );
        let one = request(
            "one",
            serde_json::json!({"summary":"s","namespace":"explicit","embedding":[1.0,0.0]}),
        );
        let two = request("two", one.items[0].clone());
        assert_eq!(
            prepare(&one).unwrap()[0].plan.request_hash(),
            prepare(&two).unwrap()[0].plan.request_hash()
        );
    }
    #[test]
    fn caller_hash_preserves_supplied_float_bits_and_expands_fixed_defaults() {
        let baseline = request(
            "one",
            serde_json::json!({"summary":"s","embedding":[1.0,0.0]}),
        );
        let hash = prepare(&baseline).unwrap()[0].plan.request_hash();
        for embedding in [vec![2.0, 0.0], vec![1.0, -0.0]] {
            let changed = request(
                "one",
                serde_json::json!({"summary":"s","embedding":embedding}),
            );
            assert_ne!(hash, prepare(&changed).unwrap()[0].plan.request_hash());
        }
        let expanded = request(
            "one",
            serde_json::json!({"summary":"s","embedding":[1.0,0.0],"tags":[],"entities":[],"topics":[],"emotions":[],"checkDuplicates":true}),
        );
        assert_eq!(hash, prepare(&expanded).unwrap()[0].plan.request_hash());
    }
}

pub(crate) fn stored_from_receipt(
    input: &bridge::StoreInput,
    receipt: &control::WriteReceipt,
    timezone: chrono_tz::Tz,
) -> Result<bridge::StoredMemory, bridge::BridgeError> {
    let control::MutationOutcome::MemoryInserted {
        memory_id,
        created_at,
        initial_stability_bits,
        ..
    } = receipt.outcome
    else {
        return Err(bridge::BridgeError::Internal(
            "wrong batch insertion receipt".into(),
        ));
    };
    Ok(bridge::StoredMemory {
        id: memory_id.to_string(),
        namespace: input.namespace.clone(),
        phase: "Full".into(),
        strength: 1.0,
        stability: f32::from_bits(initial_stability_bits),
        created_at: crate::time::format_timestamp(created_at, timezone),
        supersedes: input.supersedes.map(|id| {
            crate::model::SupersedesOutcome::new(id, crate::model::SupersedesStatus::Applied)
        }),
        near_duplicates: None,
    })
}
