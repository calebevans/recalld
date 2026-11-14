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
