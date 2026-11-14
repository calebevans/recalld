use recalld::storage::{RedbStorageEngine, StorageEngine};
use zerocopy::{FromBytes, IntoBytes};

#[test]
fn offline_text_migration_cli_publishes_only_fresh_gated_destination() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("source");
    let target = root.path().join("target");
    std::fs::create_dir(&source).unwrap();
    // Historical empty layout assembled only in this integration test. No
    // public raw-store constructor is needed by production callers.
    let db = redb::Database::create(source.join("meta.db")).unwrap();
    let tx = db.begin_write().unwrap();
    tx.open_table(redb::TableDefinition::<&[u8], &[u8]>::new("memories")).unwrap();
    tx.open_table(redb::TableDefinition::<u32, &[u8]>::new("namespaces")).unwrap();
    tx.commit().unwrap();
    drop(db);
    let db = redb::Database::create(source.join("edges.db")).unwrap();
    let tx = db.begin_write().unwrap();
    tx.open_table(redb::TableDefinition::<&[u8], &[u8]>::new("edges_forward")).unwrap();
    tx.open_table(redb::TableDefinition::<&[u8], ()>::new("edges_reverse")).unwrap();
    tx.commit().unwrap();
    drop(db);
    std::fs::write(source.join("fulltext.dat"), recalld::storage::text::TextLogHeader::new().as_bytes()).unwrap();
    let engine = RedbStorageEngine::open(&source).unwrap();
    let lineage = engine.control_state().unwrap().store_id;
    drop(engine);
    let before = std::fs::read(source.join("meta.db")).unwrap();
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_recalld-text-migrate"))
        .args([&source, &target])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["storeId"], lineage.to_string());
    assert_eq!(std::fs::read(source.join("meta.db")).unwrap(), before);
    let gate = std::fs::read(target.join("fulltext.dat")).unwrap();
    assert!(recalld::storage::text::TextLogHeader::read_from_bytes(&gate[..16]).unwrap().validate().is_err());
    assert_eq!(
        RedbStorageEngine::open(&target).unwrap().count().unwrap(),
        0
    );
    let repeated = std::process::Command::new(env!("CARGO_BIN_EXE_recalld-text-migrate"))
        .args([&source, &target])
        .output()
        .unwrap();
    assert!(!repeated.status.success());
    let engine = RedbStorageEngine::open(&source).unwrap();
    let other = root.path().join("other");
    let busy = std::process::Command::new(env!("CARGO_BIN_EXE_recalld-text-migrate"))
        .args([&source, &other])
        .output()
        .unwrap();
    assert!(!busy.status.success());
    assert!(!other.exists());
    drop(engine);
}
