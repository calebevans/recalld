use super::*;
use crate::model::schema::{
    EmbeddingProfile, LabelProvenance, Normalization, PreprocessingIdentity, ProviderKind,
};
use crate::model::{AccessKind, EdgeType, MemoryId, NamespaceId};
use crate::storage::control::{MutationContext, MutationScope};
use crate::storage::schema_migration::{
    MigrationOptions, ProfileAttestationInput, inspect_schema, migrate_schema,
};
use crate::storage::{RedbStorageEngine, StorageEngine};
use crate::test_support::{Fixture, MemorySpec};

const FORGOTTEN: &str = "UNREACHABLE-FORGOTTEN-TEXT-93f4d1";

async fn fixture() -> (tempfile::TempDir, MemoryId) {
    let f = Fixture::new();
    let other = f.create_namespace("unknown");
    let first = f
        .insert_memory_with(MemorySpec {
            embedding: Some(&[1., -0., 0., 0.]),
            tags: &["entity/john-smith"],
            ..MemorySpec::new(NamespaceId::new(1))
        })
        .await;
    f.insert_memory_with(MemorySpec {
        namespace: other,
        phase: DecayPhase::Ghost,
        summary: "",
        tags: &["retained/legacy"],
        embedding: Some(&[0., 1., 0., 0.]),
        ..MemorySpec::new(other)
    })
    .await;
    let tombstone = f
        .insert_memory_with(MemorySpec {
            embedding: Some(&[1., 0., 0., 0.]),
            ..MemorySpec::new(NamespaceId::new(1))
        })
        .await;
    {
        let mut engine = f.storage.write().unwrap();
        let mut base = engine.get_record(first).unwrap().unwrap();
        base.strength = -0.0;
        base.stability = f32::from_bits(0x406dba5e);
        engine
            .meta_store()
            .batch_update_records(&[(first, base.clone())])
            .unwrap();
        let dead = MemoryId::new();
        base.id = *dead.as_bytes();
        base.summary = FORGOTTEN.into();
        engine
            .insert_memory(
                dead,
                NamespaceId::new(1),
                &mut base,
                &[0.125, 0.25, 0.5, 0.75],
                Some(FORGOTTEN),
            )
            .unwrap();
        let live = MemoryId::new();
        base.id = *live.as_bytes();
        base.summary = "retained résumé".into();
        engine
            .insert_memory(
                live,
                NamespaceId::new(1),
                &mut base,
                &[0., 0., 1., -0.],
                Some("retained text José 🦀\n"),
            )
            .unwrap();
        let state = engine.control_state().unwrap();
        engine
            .update_access_with_receipt(
                &MutationContext {
                    store_id: Some(state.store_id),
                    scope: MutationScope::Namespace(NamespaceId::new(1)),
                    idempotency_key: Some("deleted-receipt-retained".into()),
                    expected_generation: Some(state.committed_generation),
                    contract_version: 1,
                },
                dead,
                42,
                AccessKind::DirectRetrieval,
            )
            .unwrap();
        engine.delete_memory(dead).unwrap();
        engine.tombstone_memory(tombstone).unwrap();
        engine
            .add_edge(
                first,
                live,
                EdgeType::Associative,
                f32::from_bits(0x3ec00001),
                true,
                123,
            )
            .unwrap();
        engine.sync().unwrap();
    }
    let legacy = f.close();
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("source");
    let inspected = inspect_schema(legacy.path()).unwrap();
    let profile = EmbeddingProfile {
        identity_version: 1,
        provider_kind: ProviderKind::Passthrough,
        model_id: "operator-known-fixture".into(),
        model_revision: None,
        dimension: 4,
        document_preprocessing: PreprocessingIdentity {
            version: "recalld-document-v1".into(),
            prefix: "".into(),
        },
        query_preprocessing: PreprocessingIdentity {
            version: "recalld-query-v1".into(),
            prefix: "".into(),
        },
        normalization: Normalization::L2UnitV1,
    };
    migrate_schema(
        legacy.path(),
        &source,
        &MigrationOptions {
            migration_now: 100,
            expected_source: inspected.control.generation_ref(),
            attestations: vec![ProfileAttestationInput {
                namespace_id: NamespaceId::new(1),
                namespace_name: "default".into(),
                descriptor: profile,
            }],
        },
    )
    .unwrap();
    // Controlled canonical input fixture, not a claimed participating v2 writer.
    {
        let db = redb::Database::open(source.join("meta.db")).unwrap();
        let tx = db.begin_write().unwrap();
        {
            let mut table = tx.open_table(crate::storage::metadata::META_TABLE).unwrap();
            let mut record = DiskRecord::from_bytes(
                table
                    .get(first.as_bytes().as_slice())
                    .unwrap()
                    .unwrap()
                    .value(),
            )
            .unwrap();
            let schema = record.schema.as_mut().unwrap();
            schema.labels.entities = vec![
                "John Smith".into(),
                "José".into(),
                "Alice's".into(),
                "John Smith".into(),
            ];
            schema.labels.provenance = LabelProvenance::NativeV2;
            schema.extensions.insert(
                "retainedOptional".into(),
                serde_json::json!({"case":"Exact","nullable":null}),
            );
            table
                .insert(
                    first.as_bytes().as_slice(),
                    record.encode().unwrap().as_slice(),
                )
                .unwrap();
        }
        tx.commit().unwrap();
    }
    (root, first)
}

fn file_bytes(path: &Path) -> BTreeMap<std::path::PathBuf, Vec<u8>> {
    crate::storage::validation::canonical_files(path)
        .unwrap()
        .into_iter()
        .map(|p| {
            (
                p.strip_prefix(path).unwrap().to_owned(),
                std::fs::read(p).unwrap(),
            )
        })
        .collect()
}

#[tokio::test]
async fn logical_roundtrip_preserves_bits_labels_profiles_receipts_without_dead_payload() {
    let (root, first) = fixture().await;
    let source = root.path().join("source");
    let export = root.path().join("retained.jsonl");
    let target = root.path().join("imported");
    let before = file_bytes(&source);
    assert!(before.values().any(|v| {
        v.windows(FORGOTTEN.len())
            .any(|w| w == FORGOTTEN.as_bytes())
    }));
    let expected = export_store(&source, &export).unwrap();
    assert_eq!(file_bytes(&source), before);
    scan_stream(&export, |_, chunk, _| {
        match &chunk.item {
            Item::Memory(m) => {
                assert!(
                    !m.record
                        .windows(FORGOTTEN.len())
                        .any(|w| w == FORGOTTEN.as_bytes())
                );
                assert!(!m.full_text.as_ref().is_some_and(|s| s.contains(FORGOTTEN)));
            }
            Item::Row { value, .. } => assert!(
                !value
                    .windows(FORGOTTEN.len())
                    .any(|w| w == FORGOTTEN.as_bytes())
            ),
            _ => {}
        }
        Ok(())
    })
    .unwrap();
    let imported = import_store(&export, &target).unwrap();
    assert_eq!(imported, expected);
    let after = file_bytes(&target);
    assert!(!after.values().any(|v| {
        v.windows(FORGOTTEN.len())
            .any(|w| w == FORGOTTEN.as_bytes())
    }));
    let a = OfflineSnapshot::open(&source).unwrap();
    let b = OfflineSnapshot::open(&target).unwrap();
    assert_eq!(a.control(), b.control());
    assert_eq!(
        encoded(&a.namespaces()).unwrap(),
        encoded(&b.namespaces()).unwrap()
    );
    let original = logical_memory(a.record(first).unwrap().unwrap()).unwrap();
    let copied = logical_memory(b.record(first).unwrap().unwrap()).unwrap();
    assert_eq!(encoded(&original).unwrap(), encoded(&copied).unwrap());
    assert_eq!(
        DiskRecord::from_bytes(&copied.record)
            .unwrap()
            .strength
            .to_bits(),
        (-0.0f32).to_bits()
    );
    let mut edges_a = vec![];
    let mut edges_b = vec![];
    a.for_each_edge(|e| {
        edges_a.push((
            e.source,
            e.target,
            e.weight.to_bits(),
            e.auto_created,
            e.created_at,
        ));
        Ok(())
    })
    .unwrap();
    b.for_each_edge(|e| {
        edges_b.push((
            e.source,
            e.target,
            e.weight.to_bits(),
            e.auto_created,
            e.created_at,
        ));
        Ok(())
    })
    .unwrap();
    assert_eq!(edges_a, edges_b);
    drop((a, b));
    assert_eq!(import_store(&export, &target).unwrap(), expected);
    assert_eq!(file_bytes(&target), after);
    let again = export_store(&target, &root.path().join("again.jsonl")).unwrap();
    assert_eq!(again.logical_sha256, expected.logical_sha256);
    assert_eq!(again.source, expected.source);
    assert!(RedbStorageEngine::open(&target).is_err());
    assert_eq!(file_bytes(&target), after);
    assert_eq!(file_bytes(&source), before);
}

#[tokio::test]
async fn malformed_streams_never_publish_or_modify_source() {
    let (root, _) = fixture().await;
    let source = root.path().join("source");
    let export = root.path().join("valid.jsonl");
    export_store(&source, &export).unwrap();
    let original = std::fs::read(&export).unwrap();
    let before = file_bytes(&source);
    for mode in ["future", "truncated", "checksum", "trailing", "oversized"] {
        let mut bytes = original.clone();
        match mode {
            "future" => {
                let end = bytes.iter().position(|b| *b == b'\n').unwrap();
                let mut h: serde_json::Value = serde_json::from_slice(&bytes[..end]).unwrap();
                h["data"]["exportFormatVersion"] = serde_json::json!(2);
                let mut changed = serde_json::to_vec(&h).unwrap();
                changed.extend_from_slice(&bytes[end..]);
                bytes = changed;
            }
            "truncated" => {
                bytes.pop();
            }
            "checksum" => {
                let end = bytes.iter().position(|b| *b == b'\n').unwrap();
                let next = bytes[end + 1..].iter().position(|b| *b == b'\n').unwrap() + end + 1;
                let mut line: serde_json::Value =
                    serde_json::from_slice(&bytes[end + 1..next]).unwrap();
                line["data"]["sha256"][0] =
                    serde_json::json!(line["data"]["sha256"][0].as_u64().unwrap() ^ 1);
                let mut changed = bytes[..end + 1].to_vec();
                changed.extend(serde_json::to_vec(&line).unwrap());
                changed.extend_from_slice(&bytes[next..]);
                bytes = changed;
            }
            "trailing" => bytes.extend_from_slice(b"{}\n"),
            _ => bytes = vec![b' '; (MAX_LINE + 1) as usize],
        }
        let input = root.path().join(format!("{mode}.jsonl"));
        std::fs::write(&input, &bytes).unwrap();
        let target = root.path().join(format!("target-{mode}"));
        assert!(import_store(&input, &target).is_err(), "{mode}");
        assert!(!target.exists());
        assert_eq!(std::fs::read(&input).unwrap(), bytes);
        assert_eq!(file_bytes(&source), before);
    }
    assert_eq!(std::fs::read(&export).unwrap(), original);
}

#[test]
fn import_interruption_child() {
    let Ok(input) = std::env::var("RECALLD_IMPORT_TEST_INPUT") else {
        return;
    };
    let target = std::env::var("RECALLD_IMPORT_TEST_TARGET").unwrap();
    import_store(Path::new(&input), Path::new(&target)).unwrap();
    panic!("boundary not reached");
}

#[tokio::test]
async fn unknown_tables_and_checksumming_a_future_control_version_do_not_make_it_importable() {
    let (root, _) = fixture().await;
    let source = root.path().join("source");
    let export = root.path().join("valid.jsonl");
    export_store(&source, &export).unwrap();
    let mut reader = BufReader::new(std::fs::File::open(&export).unwrap());
    let Some(Line::Header(header)) = read_line(&mut reader).unwrap() else {
        panic!("header");
    };
    let mut output = std::fs::File::create(root.path().join("future.jsonl")).unwrap();
    write_line(&mut output, &Line::Header(header.clone())).unwrap();
    let mut current = String::new();
    let mut previous = digest(&encoded(&header).unwrap());
    let mut logical = LogicalHash::new(header.source).unwrap();
    let mut count = 0;
    while let Some(Line::Chunk(mut chunk)) = read_line(&mut reader).unwrap() {
        match &mut chunk.item {
            Item::Table { name, .. } => current = name.clone(),
            Item::Row { value, .. } if current == "canonical_control" => {
                let mut state: serde_json::Value = serde_json::from_slice(value).unwrap();
                state["controlSchemaVersion"] = serde_json::json!(999);
                *value = encoded(&state).unwrap();
            }
            _ => {}
        }
        count += 1;
        previous = chain(previous, count, &encoded(&chunk.item).unwrap());
        chunk.sha256 = previous;
        logical.item(&chunk.item).unwrap();
        write_line(&mut output, &Line::Chunk(chunk)).unwrap();
    }
    let summary = ExportSummary {
        export_id: header.export_id,
        source: header.source,
        chunks: count,
        records: logical.records,
        logical_sha256: logical.hash.finalize().into(),
    };
    write_line(
        &mut output,
        &Line::Footer {
            summary,
            chain_sha256: previous,
        },
    )
    .unwrap();
    drop(output);
    let future = root.path().join("future.jsonl");
    assert!(scan_stream(&future, |_, _, _| Ok(())).is_ok());
    let target = root.path().join("rejected");
    assert!(import_store(&future, &target).is_err());
    assert!(!target.exists());
    {
        let db = redb::Database::open(source.join("meta.db")).unwrap();
        let tx = db.begin_write().unwrap();
        tx.open_table(TableDefinition::<&str, &[u8]>::new(
            "future_required_relation",
        ))
        .unwrap()
        .insert("state", FORGOTTEN.as_bytes())
        .unwrap();
        tx.commit().unwrap();
    }
    let before = file_bytes(&source);
    let rejected = root.path().join("unsupported.jsonl");
    assert!(export_store(&source, &rejected).is_err());
    assert!(!rejected.exists());
    assert_eq!(file_bytes(&source), before);
}

#[tokio::test]
async fn interrupted_import_resumes_checksummed_chunks_without_duplicate_logical_records() {
    let (root, _) = fixture().await;
    let source = root.path().join("source");
    let export = root.path().join("export.jsonl");
    let expected = export_store(&source, &export).unwrap();
    let before = file_bytes(&source);
    let export_bytes = std::fs::read(&export).unwrap();
    for point in [
        "initialized",
        "chunk_committed",
        "payload_synced",
        "edge_committed",
        "selected",
        "validated",
        "published",
    ] {
        let target = root.path().join(point);
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "storage::logical_export::tests::import_interruption_child",
                "--nocapture",
            ])
            .env("RECALLD_IMPORT_TEST_INPUT", &export)
            .env("RECALLD_IMPORT_TEST_TARGET", &target)
            .env("RECALLD_IMPORT_TEST_STOP", point)
            .status()
            .unwrap();
        assert_eq!(status.code(), Some(76), "{point}");
        assert_eq!(target.exists(), point == "published");
        for entry in std::fs::read_dir(root.path()).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() && path.join(".recalld-importing").exists() {
                let metadata_path = path.join("meta.db");
                let before = std::fs::read(&metadata_path).unwrap();
                assert!(crate::storage::MetadataStore::open(&metadata_path).is_err());
                assert_eq!(std::fs::read(&metadata_path).unwrap(), before);
                assert!(OfflineSnapshot::open(&path).is_err());
                assert!(inspect_schema(&path).is_err());
            }
        }
        assert_eq!(import_store(&export, &target).unwrap(), expected, "{point}");
        assert_eq!(import_store(&export, &target).unwrap(), expected);
        assert_eq!(file_bytes(&source), before);
        assert_eq!(std::fs::read(&export).unwrap(), export_bytes);
    }
}

#[tokio::test]
async fn insufficient_staging_space_rejects_before_stage_or_source_mutation() {
    let (root, _) = fixture().await;
    let source = root.path().join("source");
    let before = file_bytes(&source);
    let input = root.path().join("export.jsonl");
    export_store(&source, &input).unwrap();
    let exported = std::fs::read(&input).unwrap();
    let target = root.path().join("insufficient-space");
    TEST_AVAILABLE_SPACE.with(|v| v.set(Some(0)));
    let error = import_store(&input, &target).unwrap_err();
    assert!(
        error.to_string().contains("insufficient staging space"),
        "{error}"
    );
    assert!(!target.exists());
    assert!(!std::fs::read_dir(root.path()).unwrap().any(|entry| {
        entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".recalld-import-")
    }));
    assert_eq!(std::fs::read(&input).unwrap(), exported);
    assert_eq!(file_bytes(&source), before);
    import_store(&input, &target).unwrap();
}
