use super::*;
use crate::model::{CachedRecord, DecayPhase};
use crate::storage::control::{MutationOutcome, MutationScope};
use crate::test_support::Fixture;

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
async fn namespace_batches_deduplicate_direct_wins_publish_and_survive_reopen() {
    let f = Fixture::new();
    let a = f.insert_memory().await;
    let other = f.create_namespace("other");
    let b = f.insert_memory_in(other, &[]).await;
    for id in [a, b] {
        f.cache
            .insert(id, CachedRecord::from(&f.record(id).unwrap()))
            .await;
    }
    let before = generation(&f);
    let receipts = record_exposures(
        f.storage.clone(),
        f.cache.clone(),
        vec![
            (b, AccessKind::AssociativeRetrieval),
            (a, AccessKind::AssociativeRetrieval),
            (a, AccessKind::DirectRetrieval),
            (b, AccessKind::AssociativeRetrieval),
        ],
        100,
    )
    .await
    .unwrap();
    assert_eq!(receipts.len(), 2);
    assert_eq!(generation(&f), before + 2);
    for (index, id, kind) in [
        (0, a, AccessKind::DirectRetrieval),
        (1, b, AccessKind::AssociativeRetrieval),
    ] {
        let MutationOutcome::ExposuresRecorded { accesses } = &receipts[index].outcome else {
            panic!()
        };
        assert_eq!(accesses.len(), 1);
        assert_eq!((accesses[0].memory_id, accesses[0].kind), (id, kind));
        assert_eq!(f.record(id).unwrap().access_history.len(), 1);
        assert_eq!(f.record(id).unwrap().access_history[0].kind, kind);
        assert!(f.cache.get(id).await.is_none());
    }
    assert!(
        record_exposures(f.storage.clone(), f.cache.clone(), vec![], i64::MAX)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(generation(&f), before + 2);
    let dir = f.close();
    let reopened = RedbStorageEngine::open(dir.path()).unwrap();
    for (receipt, ns) in receipts.iter().zip([NamespaceId::new(1), other]) {
        let persisted = reopened
            .meta_store()
            .lookup_operation(
                receipt.store_id,
                MutationScope::Namespace(ns),
                receipt.operation_id,
            )
            .unwrap()
            .unwrap();
        assert_eq!(&persisted, receipt);
    }
    assert_eq!(
        reopened
            .get_record(a)
            .unwrap()
            .unwrap()
            .access_history
            .len(),
        1
    );
}

#[tokio::test]
async fn later_namespace_failure_reports_prior_receipts_and_publishes_them() {
    let f = Fixture::new();
    let a = f.insert_memory().await;
    let ns = f.create_namespace("later");
    let b = f.insert_memory_in(ns, &[]).await;
    // Old scalar API deliberately retains its old accepted timestamp range.
    f.storage
        .write()
        .unwrap()
        .update_access(b, i64::MAX, AccessKind::DirectRetrieval)
        .unwrap();
    let b_before = f.record(b).unwrap().to_bytes();
    f.cache
        .insert(a, CachedRecord::from(&f.record(a).unwrap()))
        .await;
    let before = generation(&f);
    let error = record_exposures(
        f.storage.clone(),
        f.cache.clone(),
        vec![
            (b, AccessKind::DirectRetrieval),
            (a, AccessKind::DirectRetrieval),
        ],
        100,
    )
    .await
    .unwrap_err();
    assert_eq!(error.commit_state, "committed");
    assert!(!error.ready);
    assert_eq!(error.receipts.len(), 1);
    assert_eq!(generation(&f), before + 1);
    assert_eq!(f.record(a).unwrap().access_history.len(), 1);
    assert_eq!(f.record(b).unwrap().to_bytes(), b_before);
    assert!(f.cache.get(a).await.is_none());
    let payload = serde_json::to_value(&error).unwrap();
    assert_eq!(payload["receipts"].as_array().unwrap().len(), 1);
    assert!(!payload.to_string().contains("fixture"));
    use axum::response::IntoResponse;
    let response = crate::api::AppError::Observation(error).into_response();
    assert_eq!(
        response.status(),
        axum::http::StatusCode::SERVICE_UNAVAILABLE
    );
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body["observation"]["commitState"], "committed");
    assert_eq!(body["observation"]["receipts"].as_array().unwrap().len(), 1);
    assert!(body.get("hits").is_none());
}

#[tokio::test]
async fn same_namespace_failure_is_atomic_and_no_success_receipt() {
    let f = Fixture::new();
    let a = f.insert_memory().await;
    let b = f.insert_memory().await;
    f.storage.write().unwrap().tombstone_memory(b).unwrap();
    let before = generation(&f);
    let error = record_exposures(
        f.storage.clone(),
        f.cache.clone(),
        vec![
            (a, AccessKind::DirectRetrieval),
            (b, AccessKind::DirectRetrieval),
        ],
        100,
    )
    .await
    .unwrap_err();
    assert_eq!(error.commit_state, "notCommitted");
    assert!(error.receipts.is_empty());
    assert_eq!(generation(&f), before);
    assert!(f.record(a).unwrap().access_history.is_empty());
}

#[tokio::test]
async fn publication_panic_is_committed_not_ready_and_reopen_retains_receipt() {
    let f = Fixture::new();
    let id = f.insert_memory().await;
    let receipts = record_locked(
        &f.storage.write().unwrap(),
        &[(id, AccessKind::DirectRetrieval)],
        100,
    )
    .unwrap();
    // Synthetic publication failure after a real committed transaction.
    let error = publish(&f.storage.read().unwrap(), &receipts, || {
        panic!("publication probe")
    })
    .unwrap_err();
    assert_eq!(error.commit_state, "committed");
    assert!(!error.ready);
    assert_eq!(error.receipts, receipts);
    assert!(error.message.contains("INDEX_NOT_READY"));
    assert!(f.storage.read().unwrap().get_record(id).is_err());
    let dir = f.close();
    let reopened = RedbStorageEngine::open(dir.path()).unwrap();
    assert_eq!(
        reopened
            .get_record(id)
            .unwrap()
            .unwrap()
            .access_history
            .len(),
        1
    );
    assert!(
        reopened
            .meta_store()
            .lookup_operation(
                receipts[0].store_id,
                MutationScope::Namespace(NamespaceId::new(1)),
                receipts[0].operation_id
            )
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn actual_feedback_consumer_commits_one_receipt_and_invalidates_before_return() {
    let f = Fixture::new();
    let id = f.insert_memory().await;
    f.cache
        .insert(id, CachedRecord::from(&f.record(id).unwrap()))
        .await;
    let before = generation(&f);
    let plan = super::super::storage_adapter::reinforce(
        f.storage.clone(),
        f.cache.clone(),
        id,
        3,
        1.0,
        100,
        None,
    )
    .await
    .unwrap();
    assert_eq!(generation(&f), before + 1);
    let record = f.record(id).unwrap();
    assert_eq!(record.access_history.len(), 1);
    assert_eq!(
        record.access_history[0].kind,
        AccessKind::ManualReinforcement
    );
    assert_eq!(record.phase, DecayPhase::Full);
    assert_eq!(record.stability, plan.stability);
    assert!(f.cache.get(id).await.is_none());
    let dir = f.close();
    use redb::ReadableTable;
    let db = redb::Database::open(dir.path().join("meta.db")).unwrap();
    let tx = db.begin_read().unwrap();
    let table = tx.open_table(crate::storage::control::RECEIPTS).unwrap();
    let receipts: Vec<_> = table
        .iter()
        .unwrap()
        .map(|entry| {
            let (_, value) = entry.unwrap();
            crate::storage::control::validate_receipt(value.value())
                .unwrap()
                .receipt
        })
        .filter(|receipt| {
            matches!(
                receipt.outcome,
                MutationOutcome::LegacyFeedbackRecorded { .. }
            )
        })
        .collect();
    assert_eq!(receipts.len(), 1);
    assert!(
        matches!(receipts[0].outcome, MutationOutcome::LegacyFeedbackRecorded {memory_id, quality:3, ..} if memory_id == id)
    );
}
