//! Canonical namespace admission, checked inside the final write transaction.
use super::*;

pub(crate) fn require_namespace_active(
    tx: &redb::WriteTransaction,
    namespace: NamespaceId,
) -> Result<NamespaceConfig, StorageError> {
    purge::require_no_active_write(tx)?;
    let table = tx.open_table(NAMESPACE_TABLE)?;
    let bytes = table
        .get(namespace.get())?
        .ok_or(StorageError::NamespaceNotFound(namespace.get()))?;
    NamespaceConfig::decode(bytes.value()).map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn durable_namespace_guard_rejects_deleting_store_inside_final_transaction() {
        let fixture = crate::test_support::Fixture::new();
        let namespace = fixture.create_namespace("deleting");
        fixture.insert_memory_in(namespace, &[]).await;
        let mut engine = fixture.storage.write().unwrap();
        engine
            .begin_namespace_purge(&MutationContext::unkeyed(namespace), true)
            .unwrap();
        // Bypass only the fast flag by exercising the actual durable helper
        // inside a private test transaction. No canonical write is committed.
        let tx = engine.meta_store().db.begin_write().unwrap();
        assert!(require_namespace_active(&tx, namespace).is_err());
        assert!(require_namespace_active(&tx, NamespaceId::new(1)).is_err());
        tx.abort().unwrap();
    }
}
