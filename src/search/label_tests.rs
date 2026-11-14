//! Synthetic canonical label payloads exercise read consumers, not native writer
//! admission or publication. Start with real migrated vectors/lifecycle, then replace only label
//! fields offline before freezing the fixture. These authored values are not
//! recovered historical labels or a native writer receipt. Ordinary serving is
//! still gated; OfflineSnapshot validates only the offline reader contract.
use super::*;
use crate::model::schema::{LabelProvenance, StructuredLabels};
use crate::model::tag::StructuredLabelFilter;
use redb::ReadableTable;

fn labels(provenance: LabelProvenance) -> StructuredLabels {
    StructuredLabels {
        entities: vec![
            "caleb".into(),
            "John Smith".into(),
            "José".into(),
            "Alice's".into(),
            "José".into(),
        ],
        topics: vec!["日本語".into(), "Mixed Case".into()],
        emotions: vec!["mixed feelings".into()],
        provenance,
    }
}

pub(super) async fn persisted_labels(
    count: usize,
    mut set_labels: impl FnMut(usize, &mut StructuredLabels),
) -> OfflineFixture {
    let OfflineFixture {
        _directory,
        path,
        view,
        files,
        ids,
    } = offline(count, LifecycleVersion::LegacyAccessV1, |_, _| {}).await;
    drop(view);
    {
        let db = redb::Database::create(path.join("meta.db")).unwrap();
        let tx = db.begin_write().unwrap();
        {
            let mut table = tx.open_table(crate::storage::metadata::META_TABLE).unwrap();
            for (index, id) in ids.iter().enumerate() {
                let mut record = {
                    let bytes = table.get(id.as_bytes().as_slice()).unwrap().unwrap();
                    DiskRecord::from_bytes(bytes.value()).unwrap()
                };
                set_labels(index, &mut record.schema.as_mut().unwrap().labels);
                let bytes = record.encode().unwrap();
                table
                    .insert(id.as_bytes().as_slice(), bytes.as_slice())
                    .unwrap();
            }
        }
        tx.commit().unwrap();
    }
    let view = Arc::new(OfflineSnapshot::open(&path).unwrap());
    OfflineFixture {
        _directory,
        path,
        view,
        files,
        ids,
    }
}

#[derive(Default)]
struct CountProvider(AtomicUsize);
#[async_trait::async_trait]
impl EmbeddingProviderRegistry for CountProvider {
    fn profile(&self) -> Option<crate::model::schema::EmbeddingProfile> {
        self.0.fetch_add(1, Ordering::SeqCst);
        panic!("metadata/supplied-vector path consulted provider identity")
    }
    fn dimensions(&self) -> Option<usize> {
        self.0.fetch_add(1, Ordering::SeqCst);
        panic!("metadata/supplied-vector path consulted provider dimensions")
    }
    async fn embed(&self, _: &str, _: &str) -> Result<Vec<f32>> {
        self.0.fetch_add(1, Ordering::SeqCst);
        panic!("metadata/supplied-vector path called provider")
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn synthetic_canonical_labels_filter_before_every_channel_cap_without_provider_calls() {
    let expected = labels(LabelProvenance::LegacyRetainedOnly);
    let f = persisted_labels(105, |index, stored| {
        *stored = expected.clone();
        if index != 104 && index != 105 {
            stored.topics = vec!["different topic".into()];
        }
    })
    .await;
    let bytes = file_bytes(&f.files);
    let provider = Arc::new(CountProvider::default());
    let mut engine = engine(&f, CountingClock::new(0), &[]);
    engine.embedding_providers = provider.clone();
    let predicate = json!({"entities":["josé", "John Smith"], "topics":["日本語"], "emotions":["mixed feelings"]});
    let supplied = query(json!({
        "text":"needle", "queryEmbedding":[1.0,0.0,0.0,0.0], "entities":["caleb"],
        "filter":{"labels":predicate}, "limit":1, "recordAccess":false
    }));
    let out = engine
        .search_with_semantics(supplied, current())
        .await
        .unwrap();
    assert_eq!(out.response.results.len(), 1);
    let hit = &out.response.results[0];
    assert_eq!(hit.memory_id, f.ids[104]);
    assert_eq!(hit.labels.as_ref(), Some(&expected));
    for channel in [
        RetrievalChannel::Vector,
        RetrievalChannel::Fts,
        RetrievalChannel::Entity,
    ] {
        assert!(
            out.diagnostics.provenance.as_ref().unwrap()[&hit.memory_id]
                .own_channels
                .contains(&channel),
            "{channel:?}"
        );
    }
    assert_eq!(out.diagnostics.fts.coverage.as_ref().unwrap().eligible, 1);
    // A literal metadata-only request has no text/vector/profile requirement.
    let metadata = query(
        json!({"queryMode":"metadata_only", "filter":{"labels":predicate}, "limit":1, "recordAccess":false}),
    );
    let out = engine
        .search_with_semantics(metadata, current())
        .await
        .unwrap();
    assert_eq!(out.response.results.len(), 1);
    assert_eq!(out.response.results[0].memory_id, f.ids[104]);
    assert!(!out.diagnostics.metadata_candidate_limited);
    assert_eq!(provider.0.load(Ordering::SeqCst), 0);
    drop(engine);
    assert_eq!(file_bytes(&f.files), bytes);
    let path = f.path.clone();
    let id = f.ids[104];
    drop(f.view);
    assert!(
        matches!(
            RedbStorageEngine::open(&path),
            Err(crate::storage::StorageError::RecoveryRequired(_))
        ),
        "authored reader fixtures must not enable ordinary serving"
    );
    assert_eq!(file_bytes(&f.files), bytes);
    let reopened = OfflineSnapshot::open(&path).unwrap();
    assert_eq!(
        reopened
            .record(id)
            .unwrap()
            .unwrap()
            .record
            .schema
            .unwrap()
            .labels,
        expected
    );
    assert_eq!(file_bytes(&f.files), bytes);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn synthetic_canonical_empty_labels_never_fall_back_or_upgrade_provenance() {
    for provenance in [
        LabelProvenance::LegacyRetainedOnly,
        LabelProvenance::NativeV2,
    ] {
        let expected = labels(provenance);
        let f = persisted_labels(2, |index, stored| {
            *stored = expected.clone();
            if index == 0 {
                stored.entities.clear();
                stored.topics.clear();
                stored.emotions.clear();
            }
        })
        .await;
        let bytes = file_bytes(&f.files);
        let empty_record = f.view.record(f.ids[0]).unwrap().unwrap().record;
        assert!(
            empty_record
                .tags
                .iter()
                .any(|t| t.as_str() == "entity/caleb")
        );
        let empty = CachedRecord::from(&empty_record);
        assert!(empty.entities.is_empty());
        let serialized = serde_json::to_value(crate::serialization::MemoryResponse::from_cached(
            &empty,
            "default".into(),
        ))
        .unwrap();
        assert_eq!(serialized["labels"]["entities"], json!([]));
        assert!(serialized.get("entities").is_none());
        assert_eq!(serialized["labels"]["provenance"], json!(provenance));
        let predicate = StructuredLabelFilter {
            entities: vec!["caleb".into()],
            ..Default::default()
        };
        assert!(
            !predicate
                .compile()
                .matches(&empty.tags, empty.labels.as_ref())
        );
        let full = CachedRecord::from(&f.view.record(f.ids[1]).unwrap().unwrap().record);
        let serialized = serde_json::to_value(crate::serialization::MemoryResponse::from_cached(
            &full,
            "default".into(),
        ))
        .unwrap();
        assert_eq!(serialized["entities"], json!(expected.entities));
        assert_eq!(serialized["topics"], json!(expected.topics));
        assert_eq!(serialized["emotions"], json!(expected.emotions));
        assert_eq!(full.labels.as_ref(), Some(&expected));
        assert!(full.estimated_size() > empty.estimated_size());
        let mut no_topics = full.clone();
        no_topics.labels.as_mut().unwrap().topics.clear();
        no_topics.labels.as_mut().unwrap().emotions.clear();
        assert!(
            crate::cache::weight::weigh_cached_record(&full)
                > crate::cache::weight::weigh_cached_record(&no_topics)
        );
        assert_eq!(file_bytes(&f.files), bytes);
    }
}

#[tokio::test]
async fn legacy_label_filter_rejection_precedes_provider_and_keeps_legacy_shape() {
    let provider = Arc::new(CountProvider::default());
    let mut engine = super::super::tests::search_engine(vec![], &[]);
    engine.embedding_providers = provider.clone();
    for field in ["entities", "topics", "emotions"] {
        let mut filter = json!({});
        filter[field] = json!(["José"]);
        let result = engine
            .search(query(
                json!({"text":"needle", "filter":{"labels":filter}, "recordAccess":false}),
            ))
            .await;
        assert!(matches!(result, Err(SearchError::InvalidFilter(_))));
    }
    assert_eq!(provider.0.load(Ordering::SeqCst), 0);
    let mut record = super::super::tests::rec(MemoryId::new());
    record.tags = vec![crate::model::Tag::new("entity/alice").unwrap()];
    let serialized = serde_json::to_value(crate::serialization::MemoryResponse::from_cached(
        &record,
        "default".into(),
    ))
    .unwrap();
    assert_eq!(serialized["entities"], json!(["Alice"]));
    assert!(serialized.get("labels").is_none());
    let boundary = StructuredLabelFilter {
        topics: vec!["é".repeat(64)],
        ..Default::default()
    };
    assert!(boundary.validate(true).is_ok());
    assert!(boundary.validate(false).is_err());
    let oversize = StructuredLabelFilter {
        topics: vec!["é".repeat(65)],
        ..Default::default()
    };
    assert!(oversize.validate(true).is_err());
    let too_many = StructuredLabelFilter {
        entities: vec!["a".into(); crate::model::constants::MAX_TAGS + 1],
        ..Default::default()
    };
    assert!(too_many.validate(true).is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn synthetic_canonical_labels_survive_actual_http_and_mcp_hydration() {
    use super::by_id_transport_tests::{FixtureEmbedding, http, router};
    let expected = labels(LabelProvenance::LegacyRetainedOnly);
    let f = persisted_labels(2, |index, stored| {
        *stored = expected.clone();
        if index == 0 {
            stored.entities.clear();
            stored.topics.clear();
            stored.emotions.clear();
        }
    })
    .await;
    let bytes = file_bytes(&f.files);
    let engine = Arc::new(engine(&f, CountingClock::new(0), &[]));
    let placeholder = Fixture::new();
    let api = router(&placeholder, f.view.clone(), engine.clone());
    let stub = Arc::new(crate::test_support::StubBridge::default());
    let bridge = crate::mcp::bridge::McpBridge {
        search: Arc::new(crate::mcp::bridge_adapters::McpSearchAdapter::new(
            engine,
            Arc::new(FixtureEmbedding),
            placeholder.storage.clone(),
            placeholder.graph.clone(),
            chrono_tz::UTC,
        )),
        storage: stub.clone(),
        namespaces: stub.clone(),
        health: stub,
        default_namespace: "foreign".into(),
        timezone: chrono_tz::UTC,
    };
    for (source, target) in [(0, 1), (1, 0)] {
        let retained = f
            .view
            .record(f.ids[target])
            .unwrap()
            .unwrap()
            .record
            .schema
            .unwrap()
            .labels;
        let (status, http_body) = http(api.clone(), f.ids[source], false).await;
        assert!(status.is_success(), "{http_body}");
        let memory = &http_body["hits"][0]["memory"];
        assert_eq!(memory["id"], json!(f.ids[target]));
        assert_eq!(memory["labels"], json!(retained));
        let result = crate::mcp::tools::dispatch_tool(
            &bridge,
            "find_similar_memories",
            json!({"id":f.ids[source], "recordAccess":false}),
        )
        .await;
        assert_ne!(result.is_error, Some(true));
        let response = serde_json::to_value(result).unwrap();
        let body: serde_json::Value =
            serde_json::from_str(response["content"][0]["text"].as_str().unwrap()).unwrap();
        let memory = &body["memories"][0];
        assert_eq!(memory["id"], json!(f.ids[target]));
        assert_eq!(memory["labels"], json!(retained));
        if target == 1 {
            assert_eq!(memory["entities"], json!(expected.entities));
            assert_eq!(memory["topics"], json!(expected.topics));
            assert_eq!(memory["emotions"], json!(expected.emotions));
        } else {
            assert!(memory.get("entities").is_none());
        }
    }
    assert_eq!(file_bytes(&f.files), bytes);
}

#[test]
fn compiled_label_predicates_preserve_caller_text_and_use_whole_label_lowercase() {
    let input = json!({"entities":["JOSÉ"], "topics":["MIXED CASE"], "emotions":[]});
    let filter: StructuredLabelFilter = serde_json::from_value(input.clone()).unwrap();
    filter.validate(true).unwrap();
    let compiled = filter.compile();
    assert_eq!(serde_json::to_value(&filter).unwrap(), input);
    let mut stored = labels(LabelProvenance::LegacyRetainedOnly);
    assert!(compiled.matches(&[], Some(&stored)));
    stored.entities = vec!["Jose\u{301}".into()];
    assert!(
        !compiled.matches(&[], Some(&stored)),
        "no Unicode normalization is implied"
    );
    stored.entities = vec![" José".into()];
    assert!(
        !compiled.matches(&[], Some(&stored)),
        "no trimming is implied"
    );
    let alias = StructuredLabelFilter {
        entities: vec!["Smith".into()],
        ..Default::default()
    }
    .compile();
    assert!(
        !alias.matches(&[], Some(&labels(LabelProvenance::NativeV2))),
        "entity aliases are not hard filters"
    );
}
