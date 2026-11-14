use super::*;
use crate::backup::{
    RestoreOptions, archive::create_archive, manifest::generate_manifest, restore_from_backup,
    run_backup,
};
use crate::config::RecalldConfig;
use crate::model::EdgeType;
use crate::storage::{
    StorageEngine,
    schema_migration::{MigrationOptions, inspect_schema, migrate_schema},
};
use crate::test_support::Fixture;

async fn fixture() -> tempfile::TempDir {
    let fixture = Fixture::new();
    let a = fixture.insert_memory().await;
    let b = fixture.insert_memory().await;
    {
        let mut engine = fixture.storage.write().unwrap();
        engine
            .add_edge(a, b, EdgeType::Causal, 0.5, false, 123)
            .unwrap();
        engine.sync().unwrap();
    }
    let legacy = fixture.close();
    let target = tempfile::tempdir().unwrap();
    let source = inspect_schema(legacy.path())
        .unwrap()
        .control
        .generation_ref();
    migrate_schema(
        legacy.path(),
        &target.path().join("store"),
        &MigrationOptions {
            migration_now: 123456,
            expected_source: source,
            attestations: vec![],
        },
    )
    .unwrap();
    target
}

fn bytes(source: &Path) -> Vec<(std::path::PathBuf, Vec<u8>)> {
    validation::canonical_files(source)
        .unwrap()
        .into_iter()
        .map(|p| {
            let b = std::fs::read(&p).unwrap();
            (p, b)
        })
        .collect()
}

#[tokio::test]
async fn actual_v2_backup_captures_canonical_identity_configuration_counts_and_hashes() {
    let root = fixture().await;
    let source = root.path().join("store");
    let before = bytes(&source);
    let output = root.path().join("backup.zip");
    run_backup(&RecalldConfig::default(), &output, Some(&source), false)
        .await
        .unwrap();
    let mut archive = zip::ZipArchive::new(std::fs::File::open(&output).unwrap()).unwrap();
    let mut wire = String::new();
    archive
        .by_name("backup_manifest.json")
        .unwrap()
        .read_to_string(&mut wire)
        .unwrap();
    let manifest: BackupManifest = serde_json::from_str(&wire).unwrap();
    assert_eq!(manifest.backup_format_version, 2);
    assert_eq!(manifest.canonical_schema_version, Some(2));
    assert!(manifest.files.iter().all(|f| f.sha256.is_some()));
    let snapshot = manifest.snapshot.as_ref().unwrap();
    assert_eq!(snapshot.control, inspect_schema(&source).unwrap().control);
    assert_eq!(
        snapshot.schema,
        inspect_schema(&source).unwrap().schema.unwrap()
    );
    assert_eq!(snapshot.counts.records, 2);
    assert_eq!(snapshot.counts.vectors, 2);
    assert_eq!(snapshot.counts.edges, 1);
    assert_eq!(snapshot.counts.records_by_namespace_phase[&1], [2, 0, 0, 0]);
    assert_eq!(snapshot.counts.edges_by_type[&3], 1);
    assert_eq!(snapshot.namespaces.len(), 1);
    assert!(snapshot.namespaces[0].schema.is_some());
    assert_eq!(
        snapshot.derived_artifacts["fts"],
        "omitted_requires_rebuild"
    );
    validate_manifest_snapshot(&source, &manifest).unwrap();
    assert_eq!(bytes(&source), before);
    // Exercise the actual extraction hook, before any publication. The production
    // ancestry fork is Pascal's separately integrated restore bridge.
    let target = root.path().join("restored");
    restore_from_backup(RestoreOptions {
        backup_path: output,
        data_dir: target.clone(),
        force: true,
        no_stop_daemon: true,
            historical: true,
    })
    .await
    .unwrap();
    assert_eq!(inspect_schema(&target).unwrap().record_count, 2);
    assert_eq!(bytes(&source), before);
}

#[tokio::test]
async fn self_consistent_manifest_lies_fail_before_target_publication_and_source_mutation() {
    let root = fixture().await;
    let source = root.path().join("store");
    let before = bytes(&source);
    let files = validation::canonical_files(&source).unwrap();
    let manifest = generate_manifest(&source, &files).unwrap();
    for kind in ["counts", "generation", "profile", "text", "derived", "sha"] {
        let mut forged = manifest.clone();
        let snapshot = forged.snapshot.as_mut().unwrap();
        match kind {
            "counts" => snapshot.counts.records += 1,
            "generation" => {
                snapshot.control.committed_generation =
                    snapshot.control.committed_generation.next().unwrap()
            }
            "profile" => snapshot.namespaces[0].embedding_dim += 1,
            "text" => snapshot.selected_text_generation.sealed_length += 1,
            "derived" => {
                snapshot
                    .derived_artifacts
                    .insert("fts".into(), "ready".into());
            }
            "sha" => forged.files[0].sha256.as_mut().unwrap()[0] ^= 1,
            _ => unreachable!(),
        }
        assert!(
            validate_manifest_snapshot(&source, &forged).is_err(),
            "{kind}"
        );
        let archive = root.path().join(format!("{kind}.zip"));
        create_archive(&source, &files, &forged, &archive).unwrap();
        let target = root.path().join(format!("{kind}-target"));
        assert!(
            restore_from_backup(RestoreOptions {
                backup_path: archive,
                data_dir: target.clone(),
                force: true,
                no_stop_daemon: true,
            historical: true,
            })
            .await
            .is_err(),
            "{kind}"
        );
        assert!(!target.exists(), "{kind}");
        assert_eq!(bytes(&source), before);
    }
    let mut omitted = manifest.clone();
    omitted.files.retain(|f| !f.name.ends_with("vectors.dat"));
    omitted.total_size_bytes = omitted.files.iter().map(|f| f.size_bytes).sum();
    assert!(validate_manifest_snapshot(&source, &omitted).is_err());
}

#[tokio::test]
async fn manifest_versions_are_strict_and_legacy_crc_remains_semantically_validated() {
    let root = fixture().await;
    let source = root.path().join("store");
    let files = validation::canonical_files(&source).unwrap();
    let manifest = generate_manifest(&source, &files).unwrap();
    let wire = serde_json::to_value(&manifest).unwrap();
    for field in ["backupFormatVersion", "canonicalSchemaVersion", "snapshot"] {
        let mut missing = wire.clone();
        missing.as_object_mut().unwrap().remove(field);
        assert!(
            serde_json::from_value::<BackupManifest>(missing).is_err(),
            "{field}"
        );
        let mut null = wire.clone();
        null[field] = serde_json::Value::Null;
        assert!(
            serde_json::from_value::<BackupManifest>(null).is_err(),
            "{field}"
        );
    }
    for version in [0, 1, 3, u32::MAX] {
        let mut future = wire.clone();
        future["backupFormatVersion"] = version.into();
        assert!(serde_json::from_value::<BackupManifest>(future).is_err());
    }
    let mut missing_hash = wire.clone();
    missing_hash["files"][0]
        .as_object_mut()
        .unwrap()
        .remove("sha256");
    assert!(serde_json::from_value::<BackupManifest>(missing_hash).is_err());
    let mut unknown = wire.clone();
    unknown["futureRequiredVersion"] = 1.into();
    assert!(serde_json::from_value::<BackupManifest>(unknown).is_err());

    // Exercise the real bounded archive reader before any staging/publication.
    let oversized = root.path().join("oversized.zip");
    {
        use std::io::Write;
        let mut archive = zip::ZipWriter::new(std::fs::File::create(&oversized).unwrap());
        archive
            .start_file(
                "backup_manifest.json",
                zip::write::SimpleFileOptions::default(),
            )
            .unwrap();
        archive
            .write_all(&vec![
                b' ';
                super::super::manifest::MAX_MANIFEST_BYTES as usize + 1
            ])
            .unwrap();
        archive.finish().unwrap();
    }
    let target = root.path().join("oversized-target");
    assert!(
        restore_from_backup(RestoreOptions {
            backup_path: oversized,
            data_dir: target.clone(),
            force: true,
            no_stop_daemon: true,
            historical: true,
        })
        .await
        .is_err()
    );
    assert!(!target.exists());

    // A CRC-only archive is accepted only after the same full semantic scan.
    let mut legacy = wire;
    for field in ["backupFormatVersion", "canonicalSchemaVersion", "snapshot"] {
        legacy.as_object_mut().unwrap().remove(field);
    }
    for entry in legacy["files"].as_array_mut().unwrap() {
        entry.as_object_mut().unwrap().remove("sha256");
    }
    let legacy: BackupManifest = serde_json::from_value(legacy).unwrap();
    validate_manifest_snapshot(&source, &legacy).unwrap();
    std::fs::write(source.join("fulltext.dat"), b"invalid gate").unwrap();
    assert!(validate_manifest_snapshot(&source, &legacy).is_err());
    assert!(generate_manifest(&source, &files).is_err());
}
