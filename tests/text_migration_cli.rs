use recalld::storage::{EdgeStore, MetadataStore, RedbStorageEngine, StorageEngine, TextStore};

#[test]
fn offline_text_migration_cli_publishes_only_fresh_gated_destination() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("source");
    let target = root.path().join("target");
    std::fs::create_dir(&source).unwrap();
    let metadata = MetadataStore::open(&source.join("meta.db")).unwrap();
    let lineage = metadata.control_state().unwrap().store_id;
    drop(metadata);
    drop(EdgeStore::open(&source.join("edges.db")).unwrap());
    drop(TextStore::open(&source.join("fulltext.dat")).unwrap());
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
    assert!(TextStore::open(&target.join("fulltext.dat")).is_err());
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
