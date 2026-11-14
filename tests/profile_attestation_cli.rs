use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use recalld::model::schema::{
    EmbeddingProfile, Normalization, PreprocessingIdentity, ProviderKind,
};
use recalld::model::{DecayPhase, DiskRecord, MemoryId, NamespaceConfig, Tag};
use recalld::storage::{RedbStorageEngine, StorageEngine};
use serde_json::{Value, json};

fn profile() -> EmbeddingProfile {
    EmbeddingProfile {
        identity_version: 1,
        provider_kind: ProviderKind::Ollama,
        model_id: "explicit-test-model".into(),
        model_revision: None,
        dimension: 4,
        document_preprocessing: PreprocessingIdentity {
            version: "recalld-document-v1".into(),
            prefix: "document: ".into(),
        },
        query_preprocessing: PreprocessingIdentity {
            version: "recalld-query-v1".into(),
            prefix: "query: ".into(),
        },
        normalization: Normalization::L2UnitV1,
    }
}

fn fixture(path: &Path, vector: &[f32]) {
    let mut engine = RedbStorageEngine::open(path).unwrap();
    for name in ["default", "other"] {
        let mut namespace = NamespaceConfig::default_namespace(0);
        namespace.name = name.into();
        namespace.embedding_dim = 4;
        let ns = engine.create_namespace(&namespace).unwrap();
        let id = MemoryId::new();
        let mut record = DiskRecord {
            version: 1,
            schema: None,
            id: *id.as_bytes(),
            namespace_id: ns.get(),
            created_at: 100,
            last_accessed_at: 200,
            phase: DecayPhase::Full,
            strength: 0.9,
            decay_strength: 0.9,
            stability: 3.0,
            difficulty: 5.0,
            is_permastore: 0,
            vector_slot: 0,
            edge_count: 0,
            summary: "retained résumé".into(),
            tags: vec![Tag::new("entity/john-smith").unwrap()],
            access_history: vec![],
            text_offset: 0,
            text_length: 0,
        };
        engine
            .insert_memory(id, ns, &mut record, vector, Some("exact retained text"))
            .unwrap();
    }
    engine.sync().unwrap();
}

fn snapshot(path: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    fn walk(root: &Path, path: &Path, out: &mut BTreeMap<PathBuf, Vec<u8>>) {
        for entry in std::fs::read_dir(path).unwrap() {
            let entry = entry.unwrap();
            if entry.file_type().unwrap().is_dir() {
                walk(root, &entry.path(), out);
            } else if entry.file_name() != "recalld.lock" {
                out.insert(
                    entry.path().strip_prefix(root).unwrap().to_owned(),
                    std::fs::read(entry.path()).unwrap(),
                );
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(path, path, &mut out);
    out
}

fn inspect(path: &Path) -> Value {
    let output = Command::new(env!("CARGO_BIN_EXE_recalld-schema"))
        .arg("inspect")
        .arg(path)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn migrate(source: &Path, target: &Path, control: &Value, files: &[PathBuf]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_recalld-schema"));
    command.arg("migrate").arg(source).arg(target).args([
        "--at",
        "1000",
        "--expected-store-id",
        control["storeId"].as_str().unwrap(),
        "--expected-generation",
        control["committedGeneration"].as_str().unwrap(),
    ]);
    for file in files {
        command.arg("--attest-profile").arg(file);
    }
    command.output().unwrap()
}

fn attestation(root: &Path, value: &Value) -> PathBuf {
    let path = root.join("attestation.json");
    std::fs::write(&path, serde_json::to_vec(value).unwrap()).unwrap();
    path
}

fn input() -> Value {
    json!({"namespaceId":1,"namespaceName":"default","descriptor":profile()})
}

#[test]
fn offline_cli_attests_only_explicit_namespace_preserves_source_and_reconciles_retry() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("source");
    let target = root.path().join("target");
    fixture(&source, &[1., 0., 0., 0.]);
    let before = snapshot(&source);
    let source_inspection = inspect(&source);
    assert_eq!(snapshot(&source), before, "inspection must be nonmutating");
    let file = attestation(root.path(), &input());
    let output = migrate(
        &source,
        &target,
        &source_inspection["control"],
        std::slice::from_ref(&file),
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stderr).contains("legacy labels cannot be recovered"));
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["state"]["servingMode"], "offline");
    let source_generation: u64 = source_inspection["control"]["committedGeneration"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(
        report["receipt"]["committedGeneration"],
        (source_generation + 1).to_string()
    );
    assert!(!report["state"]["receiptOperationId"].is_null());
    assert_eq!(snapshot(&source), before);
    let published_before = snapshot(&target);
    let inspected = inspect(&target);
    assert_eq!(inspected["schema"], report["state"]);
    assert_eq!(inspected["recordCount"], 2);
    let namespaces = inspected["namespaces"].as_array().unwrap();
    let bound = namespaces
        .iter()
        .find(|ns| ns["name"] == "default")
        .unwrap();
    let binding = &bound["schema"]["embeddingBinding"];
    assert_eq!(binding["state"], "bound");
    assert_eq!(binding["profileId"], profile().profile_id().unwrap());
    assert_eq!(
        binding["descriptor"],
        serde_json::to_value(profile()).unwrap()
    );
    assert_eq!(binding["provenance"], "operator_attestation");
    let other = namespaces.iter().find(|ns| ns["name"] == "other").unwrap();
    assert_eq!(
        other["schema"]["embeddingBinding"]["state"],
        "unknownLegacy"
    );
    let retry = migrate(&source, &target, &source_inspection["control"], &[file]);
    assert!(
        !retry.status.success(),
        "existing destination is reconciled, never overwritten"
    );
    assert_eq!(
        inspect(&target),
        inspected,
        "migration ID, receipt and provenance must be stable"
    );
    assert_eq!(snapshot(&target), published_before);
    assert_eq!(snapshot(&source), before);
    assert!(
        RedbStorageEngine::open(&target).is_err(),
        "ordinary serving stays gated"
    );
    assert_eq!(snapshot(&target), published_before);
    // A new copy cannot re-attest a bound namespace, even to an identical profile.
    for changed in [false, true] {
        let mut request = input();
        if changed {
            request["descriptor"]["modelId"] = json!("different-same-width-model");
        }
        let file = attestation(root.path(), &request);
        let rejected = root
            .path()
            .join(if changed { "changed" } else { "identical" });
        assert!(
            !migrate(&target, &rejected, &inspected["control"], &[file])
                .status
                .success()
        );
        assert!(!rejected.exists());
        assert_eq!(snapshot(&target), published_before);
    }
}

#[test]
fn offline_cli_rejects_bad_assertions_profiles_and_vectors_without_source_changes() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("source");
    fixture(&source, &[1., 0., 0., 0.]);
    let before = snapshot(&source);
    let control = inspect(&source)["control"].clone();
    for (pointer, value) in [
        ("/namespaceId", json!(999)),
        ("/namespaceName", json!("other")),
        ("/descriptor/dimension", json!(5)),
        ("/descriptor/dimension", Value::Null),
        ("/descriptor/dimension", json!("4")),
        ("/descriptor/dimension", json!(8193)),
        ("/descriptor/documentPreprocessing/version", json!("future")),
    ] {
        let mut request = input();
        *request.pointer_mut(pointer).unwrap() = value;
        let file = attestation(root.path(), &request);
        let target = root.path().join("invalid");
        assert!(
            !migrate(&source, &target, &control, &[file])
                .status
                .success(),
            "{pointer}"
        );
        assert!(!target.exists());
        assert_eq!(snapshot(&source), before);
    }
    // Raw files exercise duplicate/unknown-field rejection before migration.
    let valid_json = serde_json::to_string(&input()).unwrap();
    for malformed in [
        valid_json.replacen('{', "{\"namespaceId\":1,", 1),
        valid_json.replacen('{', "{\"apiKey\":\"sentinel-secret\",", 1),
        "null".into(),
    ] {
        let file = root.path().join("invalid.json");
        std::fs::write(&file, malformed).unwrap();
        let target = root.path().join("invalid-json");
        let output = migrate(&source, &target, &control, &[file]);
        assert!(!output.status.success());
        assert!(!String::from_utf8_lossy(&output.stderr).contains("sentinel-secret"));
        assert!(!target.exists());
        assert_eq!(snapshot(&source), before);
    }
    let duplicate = attestation(root.path(), &input());
    let duplicate_target = root.path().join("duplicate-namespace");
    assert!(
        !migrate(
            &source,
            &duplicate_target,
            &control,
            &[duplicate.clone(), duplicate]
        )
        .status
        .success()
    );
    assert!(!duplicate_target.exists());
    assert_eq!(snapshot(&source), before);
    let mut wrong_control = control.clone();
    wrong_control["storeId"] = json!(uuid::Uuid::new_v4().to_string());
    let file = attestation(root.path(), &input());
    let target = root.path().join("wrong-store");
    assert!(
        !migrate(
            &source,
            &target,
            &wrong_control,
            std::slice::from_ref(&file)
        )
        .status
        .success()
    );
    assert!(!target.exists());
    wrong_control = control.clone();
    wrong_control["committedGeneration"] = json!("00");
    assert!(
        !migrate(&source, &target, &wrong_control, &[file])
            .status
            .success()
    );
    assert_eq!(snapshot(&source), before);
    let invalid_vector_source = root.path().join("scaled");
    fixture(&invalid_vector_source, &[2., 0., 0., 0.]);
    let before = snapshot(&invalid_vector_source);
    let inspection = inspect(&invalid_vector_source);
    let file = attestation(root.path(), &input());
    let target = root.path().join("scaled-target");
    assert!(
        !migrate(
            &invalid_vector_source,
            &target,
            &inspection["control"],
            &[file]
        )
        .status
        .success()
    );
    assert!(!target.exists());
    assert_eq!(snapshot(&invalid_vector_source), before);
}

#[test]
fn offline_cli_without_attestation_preserves_unknown_provenance() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("source");
    let target = root.path().join("target");
    fixture(&source, &[1., 0., 0., 0.]);
    let before = snapshot(&source);
    let control = inspect(&source)["control"].clone();
    let output = migrate(&source, &target, &control, &[]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(report["receipt"].is_null());
    assert!(report["state"]["receiptOperationId"].is_null());
    assert_eq!(
        inspect(&target)["control"]["committedGeneration"],
        control["committedGeneration"]
    );
    for namespace in inspect(&target)["namespaces"].as_array().unwrap() {
        assert_eq!(
            namespace["schema"]["embeddingBinding"]["state"],
            "unknownLegacy"
        );
    }
    assert_eq!(snapshot(&source), before);
}

fn transfer(command: &str, source: &Path, destination: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_recalld-schema"))
        .arg(command)
        .arg(source)
        .arg(destination)
        .output()
        .unwrap()
}

fn export_summary(output: &Output) -> recalld::storage::logical_export::ExportSummary {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn offline_cli_export_import_preserves_profiles_source_and_completed_retry() {
    for attested in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let legacy = root.path().join("legacy");
        fixture(&legacy, &[1., 0., 0., 0.]);
        let legacy_before = snapshot(&legacy);
        let schema = root.path().join("schema");
        let control = inspect(&legacy)["control"].clone();
        let files = if attested {
            vec![attestation(root.path(), &input())]
        } else {
            vec![]
        };
        assert!(migrate(&legacy, &schema, &control, &files).status.success());
        let inspection = inspect(&schema);
        let source_before = snapshot(&schema);
        let file = root.path().join("retained.jsonl");
        let exported = export_summary(&transfer("export", &schema, &file));
        assert_eq!(exported.records, 2);
        let file_before = std::fs::read(&file).unwrap();
        let destination = root.path().join("imported");
        let output = transfer("import", &file, &destination);
        assert_eq!(export_summary(&output), exported);
        assert!(String::from_utf8_lossy(&output.stderr).contains("remains offline"));
        assert_eq!(inspect(&destination), inspection);
        let imported_before = snapshot(&destination);
        assert_eq!(
            export_summary(&transfer("import", &file, &destination)),
            exported
        );
        assert_eq!(snapshot(&destination), imported_before);
        let second = root.path().join("reexport.jsonl");
        let reexported = export_summary(&transfer("export", &destination, &second));
        assert_eq!(reexported.source, exported.source);
        assert_eq!(reexported.logical_sha256, exported.logical_sha256);
        assert_eq!(reexported.records, exported.records);
        assert_ne!(reexported.export_id, exported.export_id);
        assert!(RedbStorageEngine::open(&destination).is_err());
        assert_eq!(snapshot(&destination), imported_before);
        assert_eq!(snapshot(&schema), source_before);
        assert_eq!(snapshot(&legacy), legacy_before);
        assert_eq!(std::fs::read(&file).unwrap(), file_before);
    }
}

#[test]
fn offline_cli_export_import_rejects_legacy_malformed_and_existing_destinations() {
    let root = tempfile::tempdir().unwrap();
    let legacy = root.path().join("legacy");
    fixture(&legacy, &[1., 0., 0., 0.]);
    let legacy_before = snapshot(&legacy);
    let file = root.path().join("retained.jsonl");
    let failed = transfer("export", &legacy, &file);
    assert!(!failed.status.success());
    assert!(failed.stdout.is_empty());
    assert!(!file.exists());
    assert_eq!(snapshot(&legacy), legacy_before);
    let schema = root.path().join("schema");
    assert!(
        migrate(&legacy, &schema, &inspect(&legacy)["control"], &[])
            .status
            .success()
    );
    export_summary(&transfer("export", &schema, &file));
    let original = std::fs::read(&file).unwrap();
    let schema_before = snapshot(&schema);
    assert!(!transfer("export", &schema, &file).status.success());
    assert_eq!(std::fs::read(&file).unwrap(), original);
    assert!(!transfer("import", &file, &legacy).status.success());
    assert_eq!(snapshot(&legacy), legacy_before);
    let imported = root.path().join("imported");
    export_summary(&transfer("import", &file, &imported));
    let imported_before = snapshot(&imported);
    let other_file = root.path().join("other.jsonl");
    export_summary(&transfer("export", &schema, &other_file));
    assert!(!transfer("import", &other_file, &imported).status.success());
    assert_eq!(snapshot(&imported), imported_before);

    let mut lines: Vec<Value> = std::str::from_utf8(&original)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    lines[0]["data"]["exportFormatVersion"] = json!(99);
    let future = lines
        .iter()
        .map(|v| format!("{v}\n"))
        .collect::<String>()
        .into_bytes();
    for (name, bytes) in [
        ("future", future),
        ("truncated", original[..original.len() / 2].to_vec()),
        ("garbage", b"not json\n".to_vec()),
    ] {
        let bad = root.path().join(format!("{name}.jsonl"));
        std::fs::write(&bad, &bytes).unwrap();
        let destination = root.path().join(name);
        let output = transfer("import", &bad, &destination);
        assert!(!output.status.success(), "{name}");
        if name == "future" {
            assert!(String::from_utf8_lossy(&output.stderr).contains("unsupported export version"));
        }
        assert!(output.stdout.is_empty());
        assert!(!destination.exists());
        assert_eq!(std::fs::read(&bad).unwrap(), bytes);
    }
    assert_eq!(snapshot(&schema), schema_before);
    assert_eq!(std::fs::read(&file).unwrap(), original);
}
