//! Executes the public offline CLI against synthetic stores only.
use recalld::model::schema::LifecycleVersion;
use recalld::model::{MemoryId, NamespaceConfig};
use recalld::storage::lifecycle_migration::LifecycleMigrationOptions;
use recalld::storage::schema_migration::{MigrationOptions, inspect_schema, migrate_schema};
use recalld::storage::{RedbStorageEngine, StorageEngine};
use std::process::Command;

#[test]
fn cli_preview_migrate_retry_and_rollback_use_real_offline_writer() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("legacy");
    let mut engine = RedbStorageEngine::open(&source).unwrap();
    let mut ns = NamespaceConfig::default_namespace(0);
    ns.embedding_dim = 4;
    let ns = engine.create_namespace(&ns).unwrap();
    let id = MemoryId::new();
    engine
        .insert_memory_with_receipt(
            &recalld::storage::control::MutationContext::unkeyed(ns),
            &recalld::storage::insertion::InsertMemory {
                id: Some(id),
                summary: "kept".into(),
                full_text: Some("retained payload".into()),
                tags: vec![],
                embedding: None,
                initial_stability: Some(1.),
                created_at: Some(0),
                parent_id: None,
                supersedes: None,
            },
            &[1., 0., 0., 0.],
            0,
        )
        .unwrap();
    engine.sync().unwrap();
    drop(engine);
    let canonical = root.path().join("canonical");
    migrate_schema(
        &source,
        &canonical,
        &MigrationOptions {
            migration_now: 42,
            expected_source: inspect_schema(&source).unwrap().control.generation_ref(),
            attestations: vec![],
        },
    )
    .unwrap();
    let opts = LifecycleMigrationOptions {
        expected_source: inspect_schema(&canonical).unwrap().control.generation_ref(),
        namespace_id: ns,
        expected_policy_revision: Default::default(),
        target: LifecycleVersion::ExplicitFeedbackV2,
        idempotency_key: "cli-test".into(),
    };
    let request = root.path().join("request.json");
    std::fs::write(&request, serde_json::to_vec(&opts).unwrap()).unwrap();
    let config = root.path().join("recalld.toml");
    std::fs::write(&config, "").unwrap();
    let run = |source: &std::path::Path, mode: &str, argument: &std::ffi::OsStr| {
        let out = Command::new(env!("CARGO_BIN_EXE_recalld"))
            .arg("--config")
            .arg(&config)
            .arg("lifecycle")
            .arg("--source")
            .arg(source)
            .arg("--request")
            .arg(&request)
            .arg(mode)
            .arg(argument)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice::<serde_json::Value>(&out.stdout).unwrap()
    };
    let before = std::fs::read(canonical.join("meta.db")).unwrap();
    let preview = run(
        &canonical,
        "--preview-at",
        std::ffi::OsStr::new("3456000000000"),
    );
    assert_eq!(preview["fullToSummary"], 1);
    assert_eq!(std::fs::read(canonical.join("meta.db")).unwrap(), before);
    let dest = root.path().join("v2");
    let receipt = run(&canonical, "--destination", dest.as_os_str());
    assert_eq!(receipt["replayed"], false);
    let retry = run(&canonical, "--destination", dest.as_os_str());
    assert_eq!(retry["operationId"], receipt["operationId"]);
    assert_eq!(retry["replayed"], true);
    let inspection = inspect_schema(&dest).unwrap();
    let mut rollback = opts;
    rollback.expected_source = inspection.control.generation_ref();
    rollback.expected_policy_revision = inspection.namespaces[0]
        .schema
        .as_ref()
        .unwrap()
        .policy_revision;
    rollback.target = LifecycleVersion::LegacyAccessV1;
    rollback.idempotency_key = "cli-back".into();
    std::fs::write(&request, serde_json::to_vec(&rollback).unwrap()).unwrap();
    let back = root.path().join("back");
    run(&dest, "--destination", back.as_os_str());
    assert!(RedbStorageEngine::open(&back).is_err());
    assert_eq!(std::fs::read(canonical.join("meta.db")).unwrap(), before);
}
