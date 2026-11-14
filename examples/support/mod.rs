//! Internal diagnostics use the real canonical writer and production startup.
//! Caller-supplied fixture IDs are supported InsertMemory inputs, not raw records.
use anyhow::{Result, ensure};
use recalld::{
    config::{EmbeddingProvider, RecalldConfig},
    model::{MemoryId, NamespaceId},
    storage::{
        RedbStorageEngine,
        control::{MutationContext, WriteReceipt},
        insertion::{InsertMemory, inserted_id},
    },
    system::Recalld,
    time::ManualClock,
};
use std::{path::Path, sync::Arc};

pub fn insert(
    storage: &mut RedbStorageEngine,
    id: MemoryId,
    namespace: NamespaceId,
    summary: String,
    vector: &[f32],
    now: i64,
) -> Result<WriteReceipt> {
    let receipt = storage.insert_memory_with_receipt(
        &MutationContext::unkeyed(namespace),
        &InsertMemory {
            id: Some(id),
            summary,
            full_text: None,
            tags: vec![],
            initial_stability: Some(1.0),
            created_at: Some(now),
            embedding: Some(vector.to_vec()),
            parent_id: None,
            supersedes: None,
        },
        &[],
        now,
    )?;
    ensure!(
        inserted_id(&receipt)? == id,
        "canonical writer changed supplied fixture identity"
    );
    Ok(receipt)
}

pub async fn open_ready(path: &Path, dimensions: usize, now: i64) -> Result<Recalld> {
    // Do not load ambient config, credentials, providers or daemon sockets.
    let mut config = RecalldConfig::default();
    config.storage.data_dir = path
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("non-UTF8 fixture path"))?
        .into();
    config.embedding.provider = EmbeddingProvider::Passthrough;
    config.embedding.dimensions = dimensions;
    config.decay.disable_sweep = true;
    config.rif.enabled = false;
    let system = Recalld::with_clock(config, Arc::new(ManualClock::new(now))).await?;
    system.complete_pending_effects().await?;
    Ok(system)
}

#[cfg(test)]
mod tests {
    use super::*;
    use recalld::{model::NamespaceConfig, storage::StorageEngine};

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn canonical_seed_rejects_bad_input_and_startup_makes_it_queryable() {
        let root = tempfile::tempdir().unwrap();
        let mut storage = RedbStorageEngine::open(root.path()).unwrap();
        let mut ns = NamespaceConfig::default_namespace(123);
        ns.name = "fixture".into();
        ns.embedding_dim = 4;
        let ns = storage.create_namespace(&ns).unwrap();
        let id = MemoryId::from_uuid(uuid::Uuid::from_u128(1));
        let before = storage.control_state().unwrap();
        assert!(insert(&mut storage, id, ns, "fixture".into(), &[f32::NAN; 4], 123).is_err());
        assert_eq!(storage.control_state().unwrap(), before);
        let receipt = insert(
            &mut storage,
            id,
            ns,
            "fixture".into(),
            &[1., 0., 0., 0.],
            123,
        )
        .unwrap();
        assert_eq!(inserted_id(&receipt).unwrap(), id);
        let committed = storage.control_state().unwrap();
        assert!(
            insert(
                &mut storage,
                id,
                ns,
                "duplicate".into(),
                &[1., 0., 0., 0.],
                123
            )
            .is_err()
        );
        assert_eq!(storage.control_state().unwrap(), committed);
        drop(storage);
        let system = open_ready(root.path(), 4, 123).await.unwrap();
        let ready = system.storage().read().unwrap().control_state().unwrap();
        let query = serde_json::from_value(serde_json::json!({
            "namespace":"fixture", "text":"fixture", "queryEmbedding":[1.,0.,0.,0.],
            "recordAccess":false, "graphDepth":0, "limit":1
        }))
        .unwrap();
        let response = system.query_engine().search(query).await.unwrap();
        assert_eq!(response.results[0].memory_id, id);
        assert_eq!(
            system.storage().read().unwrap().control_state().unwrap(),
            ready
        );
    }
}
