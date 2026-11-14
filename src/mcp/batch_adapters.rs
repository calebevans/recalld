//! Actual ordered batch consumer. A transport outcome is never used as a
//! second journal: successful child outcomes live in their canonical commit.
use super::{McpStorageAdapter, StoreWriteError, bridge};
use crate::mcp::batches::{self, PreparedItem};
use crate::storage::StorageError;
use crate::storage::batch::*;
use crate::storage::control::{MutationOutcome, WriteReceipt};
use serde_json::json;

impl McpStorageAdapter {
    fn batch_completion(&self) -> crate::derived_completion::DerivedCompletion {
        crate::derived_completion::DerivedCompletion {
            storage: self.storage.clone(),
            cache: self.cache.clone(),
            graph: self.graph.clone(),
            vectors: self.vector_index.clone(),
            fts: self.fts_index.clone(),
            entities: self.entity_index.clone(),
        }
    }

    pub(crate) async fn run_batch(
        &self,
        input: BatchInput,
    ) -> Result<BatchOutcome, bridge::BridgeError> {
        // Prepare the entire ordered caller intent before policy/default reads.
        let prepared = batches::prepare(&input)?;
        let plans: Vec<_> = prepared.iter().map(|item| item.plan.clone()).collect();
        let ctx = input.context();
        let kind = input.kind;
        let parent = input.batch_operation_id;
        let storage = self.storage.clone();
        let reserved=tokio::task::spawn_blocking(move||{
            let storage=storage.read().map_err(|e|StorageError::RecoveryRequired(e.to_string()))?;
            let store_id=storage.meta_store().control_state()?.store_id;
            Ok::<_,StorageError>((store_id,storage.meta_store().reserve_or_resume_batch(&ctx,kind,&plans,parent)))
        }).await.map_err(|e|bridge::BridgeError::PartiallyApplied(format!("batch reservation worker outcome unknown; resubmit the same full request and key after recovery: {e}")))?
            .map_err(super::map_mutation_error)?;
        let (store_id, reserved) = reserved;
        let mut progress = match reserved {
            Ok(progress) => progress,
            Err(StorageError::CommitOutcomeUnknown { operation_id, .. }) => {
                // Do not reinterpret unsafe same-handle readback as confirmation.
                return Ok(BatchOutcome {
                    result_version: 1,
                    batch_operation_id: operation_id,
                    store_id,
                    reservation_state: BatchReservationState::Unknown,
                    reservation_receipt: None,
                    bookkeeping_receipts: vec![],
                    unknown_bookkeeping_operations: vec![],
                    items: (0..prepared.len())
                        .map(|index| BatchItemOutcome {
                            index: index as u32,
                            status: BatchStatus::Unknown,
                            operation_id: None,
                            namespace_id: None,
                            receipt: None,
                            error: Some(BatchError::new("COMMIT_OUTCOME_UNKNOWN")),
                            ready: false,
                            retryable: false,
                            result: None,
                        })
                        .collect(),
                    total: prepared.len(),
                    succeeded: 0,
                    failed: 0,
                    unresolved: prepared.len(),
                });
            }
            Err(e) => return Err(super::map_mutation_error(e)),
        };
        let mut outcome = outcome_from_progress(&progress, &prepared, self.timezone)?;
        // A previous attempt may have committed a child but failed publication.
        // Replay never calls a provider or allocates another payload/identity.
        if self.batch_completion().complete().await.is_err() {
            mark_unready(&mut outcome);
            return Ok(outcome);
        }
        for (index, prepared) in prepared.iter().enumerate() {
            if !matches!(progress.items[index].state, BatchItemState::NotAttempted) {
                continue;
            }
            let child = progress.child(index).ok_or_else(|| {
                bridge::BridgeError::Internal("pending batch item has no reservation".into())
            })?;
            let result = if let Some(input) = &prepared.store {
                let mut input = input.clone();
                input.namespace = progress.items[index]
                    .defaults
                    .as_ref()
                    .unwrap()
                    .namespace_name
                    .clone();
                self.store_memory_reserved(input, Some(child.clone()))
                    .await
                    .map(|(stored, receipt)| {
                        (
                            receipt,
                            serde_json::to_value(stored).expect("stored memory serialization"),
                        )
                    })
            } else if let Some(id) = prepared.forget {
                self.batch_completion()
                    .forget_reserved(child.clone(), id)
                    .await
                    .map(|receipt| {
                        let result = forget_result(id, &receipt);
                        (receipt, result)
                    })
                    .map_err(StoreWriteError::Canonical)
            } else {
                return Err(bridge::BridgeError::Internal(
                    "pending item has no prepared input".into(),
                ));
            };
            match result {
                Ok((receipt, result)) => {
                    outcome.items[index].status = BatchStatus::Ok;
                    outcome.items[index].receipt = Some(receipt);
                    outcome.items[index].result = Some(result);
                    outcome.items[index].retryable = false;
                }
                Err(StoreWriteError::Canonical(StorageError::CommittedMutation {
                    receipt,
                    ..
                })) => {
                    // Keep the actual current child, even if older items also
                    // committed. It is a success with readiness=false.
                    outcome.items[index].status = BatchStatus::Ok;
                    outcome.items[index].result = if let Some(input) = &prepared.store {
                        Some(
                            serde_json::to_value(batches::stored_from_receipt(
                                input,
                                &receipt,
                                self.timezone,
                            )?)
                            .map_err(|e| bridge::BridgeError::Internal(e.to_string()))?,
                        )
                    } else {
                        prepared.forget.map(|id| forget_result(id, &receipt))
                    };
                    outcome.items[index].receipt = Some(*receipt);
                    outcome.items[index].error = Some(BatchError::new("INDEX_NOT_READY"));
                    outcome.items[index].retryable = false;
                    mark_unready(&mut outcome);
                    return Ok(outcome);
                }
                Err(StoreWriteError::Canonical(StorageError::CommitOutcomeUnknown {
                    operation_id,
                    ..
                })) => {
                    outcome.items[index].status = BatchStatus::Unknown;
                    outcome.items[index].operation_id = Some(operation_id);
                    outcome.items[index].error = Some(BatchError::new("COMMIT_OUTCOME_UNKNOWN"));
                    outcome.items[index].retryable = false;
                    mark_unready(&mut outcome);
                    return Ok(outcome);
                }
                Err(error) => {
                    if let Some(final_error) = final_error(&error) {
                        let storage = self.storage.clone();
                        let finalized = tokio::task::spawn_blocking(move || {
                            storage
                                .read()
                                .map_err(|e| StorageError::RecoveryRequired(e.to_string()))?
                                .meta_store()
                                .finalize_batch_error(&child, final_error)
                        })
                        .await;
                        match finalized {
                            Ok(Ok(next)) => {
                                // A concurrent same-key attempt may already have
                                // committed this child. Its canonical result wins.
                                progress = next;
                                let mut current =
                                    item_outcome(&progress, index, prepared, self.timezone)?;
                                current.ready = false;
                                outcome.items[index] = current;
                                outcome.bookkeeping_receipts =
                                    progress.bookkeeping_receipts.clone();
                            }
                            Ok(Err(StorageError::CommitOutcomeUnknown {
                                operation_id, ..
                            })) => {
                                outcome.unknown_bookkeeping_operations.push(operation_id);
                                outcome.items[index].status = BatchStatus::Unknown;
                                outcome.items[index].error =
                                    Some(BatchError::new("BOOKKEEPING_OUTCOME_UNKNOWN"));
                                outcome.items[index].retryable = false;
                                mark_unready(&mut outcome);
                                return Ok(outcome);
                            }
                            _ => {
                                outcome.items[index].status = BatchStatus::Unknown;
                                outcome.items[index].error =
                                    Some(BatchError::new("RECONCILIATION_REQUIRED"));
                                outcome.items[index].retryable = false;
                                mark_unready(&mut outcome);
                                return Ok(outcome);
                            }
                        }
                    } else {
                        // Provider failure happened before canonical work; keep
                        // its reserved child pending. No provider detail is stored.
                        let unavailable = matches!(
                            error,
                            StoreWriteError::Bridge(bridge::BridgeError::Unavailable(_))
                                | StoreWriteError::Canonical(
                                    StorageError::ProviderVectorUnavailable(_)
                                )
                        );
                        outcome.items[index].status = if unavailable {
                            BatchStatus::NotAttempted
                        } else {
                            BatchStatus::Unknown
                        };
                        outcome.items[index].retryable = unavailable;
                        outcome.items[index].error = Some(BatchError::new(if unavailable {
                            "UNAVAILABLE"
                        } else {
                            "RECONCILIATION_REQUIRED"
                        }));
                        break;
                    }
                }
            }
        }
        if self.batch_completion().complete().await.is_ok()
            && super::effects_stamp(&self.storage).await.is_ok()
        {
            // Another matching caller may have committed while this caller's
            // advisory/read stamp changed. Reconcile only through an admitted
            // healthy handle, never after an explicit unknown-commit return.
            let storage = self.storage.clone();
            let parent = outcome.batch_operation_id;
            let store_id = outcome.store_id;
            let current = tokio::task::spawn_blocking(move || {
                storage
                    .read()
                    .map_err(|e| StorageError::RecoveryRequired(e.to_string()))?
                    .meta_store()
                    .lookup_batch(store_id, parent)
            })
            .await;
            match current {
                Ok(Ok(Some(current))) => {
                    for (index, prepared) in prepared.iter().enumerate() {
                        if matches!(
                            outcome.items[index].status,
                            BatchStatus::Unknown | BatchStatus::NotAttempted
                        ) && !matches!(current.items[index].state, BatchItemState::NotAttempted)
                        {
                            outcome.items[index] =
                                item_outcome(&current, index, prepared, self.timezone)?;
                        }
                    }
                    outcome.bookkeeping_receipts = current.bookkeeping_receipts;
                }
                _ => {
                    mark_unready(&mut outcome);
                    return Ok(outcome);
                }
            }
            for item in &mut outcome.items {
                item.ready = matches!(item.status, BatchStatus::Ok | BatchStatus::Error);
            }
        } else {
            mark_unready(&mut outcome);
        }
        outcome.recount();
        Ok(outcome)
    }
}

fn mark_unready(outcome: &mut BatchOutcome) {
    for item in &mut outcome.items {
        item.ready = false;
        if item.status == BatchStatus::Ok {
            item.retryable = false;
            item.error = Some(BatchError::new("INDEX_NOT_READY"));
        }
    }
    outcome.recount();
}

fn outcome_from_progress(
    progress: &BatchProgress,
    prepared: &[PreparedItem],
    timezone: chrono_tz::Tz,
) -> Result<BatchOutcome, bridge::BridgeError> {
    let mut outcome = BatchOutcome {
        result_version: 1,
        batch_operation_id: progress.reservation_receipt.operation_id,
        store_id: progress.reservation_receipt.store_id,
        reservation_state: BatchReservationState::Committed,
        reservation_receipt: Some(progress.reservation_receipt.clone()),
        bookkeeping_receipts: progress.bookkeeping_receipts.clone(),
        unknown_bookkeeping_operations: vec![],
        items: prepared
            .iter()
            .enumerate()
            .map(|(index, item)| item_outcome(progress, index, item, timezone))
            .collect::<Result<_, _>>()?,
        total: 0,
        succeeded: 0,
        failed: 0,
        unresolved: 0,
    };
    outcome.recount();
    Ok(outcome)
}

fn item_outcome(
    progress: &BatchProgress,
    index: usize,
    prepared: &PreparedItem,
    timezone: chrono_tz::Tz,
) -> Result<BatchItemOutcome, bridge::BridgeError> {
    let item = &progress.items[index];
    let mut outcome = BatchItemOutcome {
        index: item.index,
        status: BatchStatus::NotAttempted,
        operation_id: Some(item.operation_id),
        namespace_id: item.defaults.as_ref().map(|d| d.namespace_id),
        receipt: None,
        error: None,
        ready: false,
        retryable: true,
        result: None,
    };
    match &item.state {
        BatchItemState::NotAttempted => {}
        BatchItemState::Error { error } => {
            outcome.status = BatchStatus::Error;
            outcome.error = Some(error.clone());
            outcome.retryable = false;
        }
        BatchItemState::Ok { receipt } => {
            let mut receipt = receipt.clone();
            receipt.replayed = true;
            outcome.status = BatchStatus::Ok;
            outcome.retryable = false;
            outcome.result = if let Some(input) = &prepared.store {
                let mut input = input.clone();
                input.namespace = item.defaults.as_ref().unwrap().namespace_name.clone();
                Some(
                    serde_json::to_value(batches::stored_from_receipt(&input, &receipt, timezone)?)
                        .map_err(|e| bridge::BridgeError::Internal(e.to_string()))?,
                )
            } else {
                prepared.forget.map(|id| forget_result(id, &receipt))
            };
            outcome.receipt = Some(receipt);
        }
    }
    Ok(outcome)
}

fn forget_result(id: crate::model::MemoryId, receipt: &WriteReceipt) -> serde_json::Value {
    json!({"id":id.to_string(),"deleted":matches!(receipt.outcome,MutationOutcome::MemoryTombstoned{status:crate::storage::tombstone::TombstoneStatus::Tombstoned,..})})
}

fn final_error(error: &StoreWriteError) -> Option<BatchError> {
    let code = match error {
        StoreWriteError::Bridge(bridge::BridgeError::InvalidInput(_))
        | StoreWriteError::Canonical(StorageError::InvalidMutation(_)) => "INVALID_INPUT",
        StoreWriteError::Bridge(bridge::BridgeError::NotFound(_))
        | StoreWriteError::Canonical(StorageError::NotFound(_)) => "NOT_FOUND",
        StoreWriteError::Canonical(StorageError::NamespaceNotFound(_)) => "NAMESPACE_NOT_FOUND",
        StoreWriteError::Canonical(StorageError::MutationNamespaceMismatch) => "NAMESPACE_MISMATCH",
        StoreWriteError::Canonical(StorageError::EdgeConflict(_)) => "EDGE_CONFLICT",
        StoreWriteError::Canonical(StorageError::IdempotencyConflict) => "CONFLICT",
        _ => return None,
    };
    Some(BatchError::new(code))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::bridge::StorageEngine as _;
    use crate::storage::StorageEngine as _;
    use crate::test_support::Fixture;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    fn request(f: &Fixture, key: &str, items: Vec<serde_json::Value>) -> BatchInput {
        BatchInput {
            result_version: 1,
            kind: BatchKind::Insert,
            items,
            default_namespace: "default".into(),
            store_id: Some(f.storage.read().unwrap().control_state().unwrap().store_id),
            idempotency_key: Some(key.into()),
            batch_operation_id: None,
        }
    }
    fn item() -> serde_json::Value {
        json!({"summary":"batch fixture","embedding":[1.,0.,0.,0.],"checkDuplicates":false})
    }
    fn generation(f: &Fixture) -> u64 {
        f.storage
            .read()
            .unwrap()
            .control_state()
            .unwrap()
            .committed_generation
            .get()
    }

    #[tokio::test]
    async fn actual_batch_concurrent_identical_requests_share_children_and_mutations() {
        let f = Fixture::new();
        let adapter = f.storage_adapter();
        let input = request(&f, "concurrent", vec![item()]);
        let before = generation(&f);
        let (one, two) = tokio::join!(
            adapter.mutate_batch(input.clone()),
            adapter.mutate_batch(input)
        );
        let one = one.unwrap();
        let two = two.unwrap();
        assert!(one.all_ready());
        assert!(two.all_ready());
        assert_eq!(one.batch_operation_id, two.batch_operation_id);
        assert_eq!(
            one.items[0].receipt.as_ref().unwrap().operation_id,
            two.items[0].receipt.as_ref().unwrap().operation_id
        );
        assert_eq!(generation(&f), before + 2);
        assert_eq!(f.storage.read().unwrap().scan_all().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn supplied_batch_semantic_fields_validate_before_provider_or_payload_staging() {
        struct NeverProvider;
        #[async_trait::async_trait]
        impl crate::embedding::EmbeddingProvider for NeverProvider {
            async fn embed(&self, _: &str) -> Result<Vec<f32>, crate::embedding::EmbeddingError> {
                panic!("provider must not run for supplied or invalid semantic inputs")
            }
            fn dimensions(&self) -> usize {
                4
            }
            fn model_name(&self) -> &str {
                "never"
            }
        }
        let f = Fixture::new();
        let mut adapter = f.storage_adapter();
        adapter.embedding = Arc::new(NeverProvider);
        let invalid = vec![
            json!({"summary":"s","embedding":"wrong"}),
            json!({"summary":"s","initialStability":false}),
            json!({"summary":"s","initialStability":0}),
            json!({"summary":"s","initialStability":-0.0}),
            json!({"summary":"s","embedding":[0.,0.,0.,0.]}),
            json!({"summary":"s","id":uuid::Uuid::new_v4()}),
        ];
        let before = generation(&f);
        let outcome = adapter
            .mutate_batch(request(&f, "invalid semantic", invalid))
            .await
            .unwrap();
        assert_eq!(
            (outcome.succeeded, outcome.failed, outcome.unresolved),
            (0, 6, 0)
        );
        assert!(f.storage.read().unwrap().scan_all().unwrap().is_empty());
        assert_eq!(generation(&f), before + 2); // reservation plus zero-vector final rejection
        let supplied = request(
            &f,
            "supplied",
            vec![
                json!({"summary":"s","embedding":[2.,0.,0.,0.],"initialStability":7.25,"checkDuplicates":false}),
            ],
        );
        let success = adapter.mutate_batch(supplied.clone()).await.unwrap();
        assert!(success.all_ready());
        assert!(
            matches!(success.items[0].receipt.as_ref().unwrap().outcome,MutationOutcome::MemoryInserted{initial_stability_bits,..} if initial_stability_bits==7.25f32.to_bits())
        );
        let before = generation(&f);
        let mut changed = supplied;
        changed.items[0]
            .as_object_mut()
            .unwrap()
            .remove("initialStability");
        assert!(adapter.mutate_batch(changed).await.is_err());
        assert_eq!(generation(&f), before);
    }

    #[tokio::test]
    async fn actual_batch_mixed_results_and_changed_default_or_original_bits_conflict() {
        let f = Fixture::new();
        f.create_namespace("other");
        let mut adapter = f.storage_adapter();
        let mut config = crate::config::RecalldConfig::default();
        config.graph.autolink_enabled = false;
        adapter.config = Arc::new(config);
        let input = request(
            &f,
            "mixed",
            vec![
                json!(false),
                item(),
                json!({"summary":"invalid zero","embedding":[0.,0.,0.,0.]}),
                item(),
            ],
        );
        let before = generation(&f);
        let outcome = adapter.mutate_batch(input.clone()).await.unwrap();
        assert_eq!(
            (
                outcome.total,
                outcome.succeeded,
                outcome.failed,
                outcome.unresolved
            ),
            (4, 2, 2, 0)
        );
        assert_eq!(outcome.bookkeeping_receipts.len(), 1);
        assert_eq!(generation(&f), before + 4); // parent, 2 children, 1 final rejection
        assert!(outcome.items.iter().all(|i| i.ready));
        assert!(!outcome.all_ready());
        assert_ne!(
            outcome.items[1].receipt.as_ref().unwrap().operation_id,
            outcome.items[3].receipt.as_ref().unwrap().operation_id
        );
        let replay = adapter.mutate_batch(input.clone()).await.unwrap();
        assert_eq!(generation(&f), before + 4);
        assert!(replay.reservation_receipt.unwrap().replayed);
        assert_eq!(
            replay.items[1].receipt.as_ref().unwrap().operation_id,
            outcome.items[1].receipt.as_ref().unwrap().operation_id
        );
        for changed in [
            "namespace",
            "scaled",
            "signedzero",
            "order",
            "stability",
            "unknown",
        ] {
            let mut retry = input.clone();
            match changed {
                "namespace" => retry.default_namespace = "other".into(),
                "scaled" => retry.items[1]["embedding"] = json!([2., 0., 0., 0.]),
                "signedzero" => retry.items[1]["embedding"] = json!([1., -0.0, 0., 0.]),
                "stability" => {
                    retry.items[1]["initialStability"] = json!(
                        f.storage
                            .read()
                            .unwrap()
                            .get_namespace(crate::model::NamespaceId::new(1))
                            .unwrap()
                            .unwrap()
                            .initial_stability
                    )
                }
                "unknown" => retry.items[1]["callerExtension"] = json!({"future":"intent"}),
                _ => retry.items.swap(0, 1),
            }
            assert!(
                adapter
                    .mutate_batch(retry)
                    .await
                    .unwrap_err()
                    .to_string()
                    .contains("IDEMPOTENCY_CONFLICT")
            );
            assert_eq!(generation(&f), before + 4);
        }
    }

    #[tokio::test]
    async fn committed_unready_child_keeps_current_receipt_and_replays_after_online_repair() {
        let f = Fixture::new();
        let adapter = f.storage_adapter();
        *f.vector_index.write().await =
            crate::search::FlatVectorIndex::new(crate::test_support::DIM + 1);
        let input = request(&f, "publication", vec![item(), item()]);
        let outcome = adapter.mutate_batch(input.clone()).await.unwrap();
        assert_eq!(
            (outcome.succeeded, outcome.failed, outcome.unresolved),
            (1, 0, 1)
        );
        let current = outcome.items[0].receipt.as_ref().unwrap();
        assert_eq!(Some(current.operation_id), outcome.items[0].operation_id);
        assert_ne!(current.operation_id, outcome.batch_operation_id);
        assert!(!outcome.items[0].ready);
        assert!(!outcome.items[0].retryable);
        let pending = f
            .storage
            .read()
            .unwrap()
            .meta_store()
            .pending_effects()
            .unwrap();
        assert_eq!(pending[0].receipt.operation_id, current.operation_id);
        *f.vector_index.write().await =
            crate::search::FlatVectorIndex::new(crate::test_support::DIM);
        let replay = adapter.mutate_batch(input).await.unwrap();
        assert!(replay.all_ready());
        assert_eq!(replay.succeeded, 2);
        assert!(replay.items[0].receipt.as_ref().unwrap().replayed);
        assert_eq!(
            replay.items[0].receipt.as_ref().unwrap().operation_id,
            current.operation_id
        );
        assert_eq!(f.storage.read().unwrap().scan_all().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn actual_adapter_distinguishes_unknown_reservation_child_and_error_bookkeeping() {
        for stage in [1, 2, 3] {
            let f = Fixture::new();
            let adapter = f.storage_adapter();
            f.storage
                .read()
                .unwrap()
                .meta_store()
                .fail_batch_commit
                .store(stage, Ordering::SeqCst);
            let mut value = item();
            if stage == 2 {
                value["embedding"] = json!([0., 0., 0., 0.]);
            }
            let input = request(&f, "unknown", vec![value, item()]);
            let outcome = adapter.mutate_batch(input.clone()).await.unwrap();
            assert_eq!(outcome.items[0].status, BatchStatus::Unknown);
            assert!(!outcome.items[0].ready);
            assert!(!outcome.items[0].retryable);
            let parent = outcome.batch_operation_id;
            let store_id = outcome.store_id;
            if stage == 1 {
                assert_eq!(outcome.reservation_state, BatchReservationState::Unknown);
                assert!(outcome.reservation_receipt.is_none());
                assert!(outcome.items[0].operation_id.is_none());
            } else {
                assert_eq!(outcome.reservation_state, BatchReservationState::Committed);
                assert_ne!(outcome.items[0].operation_id, Some(parent));
                assert!(outcome.items[0].receipt.is_none());
                assert_eq!(
                    outcome.unknown_bookkeeping_operations.len(),
                    usize::from(stage == 2)
                );
            }
            drop(adapter);
            let dir = f.close();
            let engine = crate::storage::RedbStorageEngine::open(dir.path()).unwrap();
            let progress = engine
                .meta_store()
                .lookup_batch(store_id, parent)
                .unwrap()
                .unwrap();
            if stage == 3 {
                assert!(
                    matches!(&progress.items[0].state,BatchItemState::Ok{receipt} if Some(receipt.operation_id)==outcome.items[0].operation_id)
                );
            }
            if stage == 2 {
                assert_eq!(
                    progress.bookkeeping_receipts[0].operation_id,
                    outcome.unknown_bookkeeping_operations[0]
                );
                assert!(matches!(
                    progress.items[0].state,
                    BatchItemState::Error { .. }
                ));
            }
            if stage == 1 {
                assert!(
                    progress
                        .items
                        .iter()
                        .all(|i| matches!(i.state, BatchItemState::NotAttempted))
                );
            }
        }
    }

    #[tokio::test]
    async fn provider_failure_stays_pending_and_committed_replay_skips_provider() {
        struct Provider {
            calls: Arc<AtomicUsize>,
        }
        #[async_trait::async_trait]
        impl crate::embedding::EmbeddingProvider for Provider {
            async fn embed(&self, _: &str) -> Result<Vec<f32>, crate::embedding::EmbeddingError> {
                Ok(if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                    vec![0.; 4]
                } else {
                    vec![1., 0., 0., 0.]
                })
            }
            fn dimensions(&self) -> usize {
                4
            }
            fn model_name(&self) -> &str {
                "synthetic-batch"
            }
        }
        let f = Fixture::new();
        let mut adapter = f.storage_adapter();
        let calls = Arc::new(AtomicUsize::new(0));
        adapter.embedding = Arc::new(Provider {
            calls: calls.clone(),
        });
        let input = request(
            &f,
            "provider",
            vec![json!({"summary":"generated","checkDuplicates":false})],
        );
        let before = generation(&f);
        let first = adapter.mutate_batch(input.clone()).await.unwrap();
        assert_eq!(first.items[0].status, BatchStatus::NotAttempted);
        assert_eq!(generation(&f), before + 1);
        assert!(f.storage.read().unwrap().scan_all().unwrap().is_empty());
        assert!(first.items[0].retryable);
        let complete = adapter.mutate_batch(input.clone()).await.unwrap();
        assert!(complete.all_ready());
        let replay = adapter.mutate_batch(input).await.unwrap();
        assert!(replay.all_ready());
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(generation(&f), before + 2);
    }

    #[tokio::test]
    async fn actual_mcp_retains_every_index_and_sets_error_on_committed_not_ready() {
        let f = Fixture::new();
        let mut bridge = crate::test_support::StubBridge::default().into_bridge();
        bridge.storage = Arc::new(f.storage_adapter());
        *f.vector_index.write().await =
            crate::search::FlatVectorIndex::new(crate::test_support::DIM + 1);
        let result = crate::mcp::tools::dispatch_tool(
            &bridge,
            "store_memories",
            json!({"memories":[false,item(),item()]}),
        )
        .await;
        assert_eq!(result.is_error, Some(true));
        let crate::mcp::protocol::ContentBlock::Text { text } = &result.content[0];
        let response: serde_json::Value = serde_json::from_str(text).unwrap();
        let outcome: BatchOutcome = serde_json::from_value(response["batch"].clone()).unwrap();
        assert_eq!(
            (
                outcome.total,
                outcome.succeeded,
                outcome.failed,
                outcome.unresolved
            ),
            (3, 1, 1, 1)
        );
        assert_eq!(
            outcome.items[1].receipt.as_ref().map(|r| r.operation_id),
            outcome.items[1].operation_id
        );
        assert_ne!(
            outcome.items[1].operation_id,
            Some(outcome.batch_operation_id)
        );
        assert_eq!(response["results"].as_array().unwrap().len(), 3);
        assert_eq!(response["stored"], 1);
        assert_eq!(response["results"][1]["ready"], false);
    }
}
