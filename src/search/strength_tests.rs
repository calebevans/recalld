//! Real offline canonical records through QueryEngine; no serving activation.
use super::tests::{ns, query};
use super::*;
use crate::model::schema::{LifecycleVersion, TimestampOrigin};
use crate::model::{DiskRecord, EdgeType};
use crate::storage::control::Generation;
use crate::storage::offline_snapshot::OfflineSnapshot;
use crate::storage::schema_migration::{MigrationOptions, inspect_schema, migrate_schema};
use crate::storage::{RedbStorageEngine, StorageEngine};
use crate::test_support::{Fixture, MemorySpec};
use serde_json::json;
use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};

const DAY: i64 = 86_400_000;
fn revision(n: &str) -> Generation {
    n.to_owned().try_into().unwrap()
}
fn current() -> RetrievalSemantics {
    RetrievalSemantics::V2(VersionMode::Current)
}

struct CountingClock {
    now: AtomicI64,
    calls: AtomicUsize,
}
impl CountingClock {
    fn new(now: i64) -> Arc<Self> {
        Arc::new(Self {
            now: AtomicI64::new(now),
            calls: AtomicUsize::new(0),
        })
    }
}
impl crate::time::Clock for CountingClock {
    fn now_millis(&self) -> i64 {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.now.load(Ordering::SeqCst)
    }
}

struct OfflineFixture {
    _directory: tempfile::TempDir,
    path: std::path::PathBuf,
    view: Arc<OfflineSnapshot>,
    files: Vec<std::path::PathBuf>,
    ids: Vec<MemoryId>, // final record belongs to the foreign namespace
}

async fn offline(
    count: usize,
    version: LifecycleVersion,
    mut edit: impl FnMut(usize, &mut DiskRecord),
) -> OfflineFixture {
    let fixture = Fixture::new();
    let foreign = fixture.create_namespace("foreign");
    let mut ids = Vec::new();
    for index in 0..=count {
        let cosine = if index == count.saturating_sub(1) {
            0.2_f32
        } else {
            1.0
        };
        ids.push(
            fixture
                .insert_memory_with(MemorySpec {
                    namespace: if index == count { foreign } else { ns() },
                    summary: "needle",
                    tags: &["project/query", "entity/caleb"],
                    embedding: Some(&[cosine, (1.0 - cosine * cosine).sqrt(), 0.0, 0.0]),
                    ..MemorySpec::new(ns())
                })
                .await,
        );
    }
    fixture.storage.write().unwrap().sync().unwrap();
    let source = fixture.close();
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("offline");
    migrate_schema(
        source.path(),
        &path,
        &MigrationOptions {
            migration_now: 100,
            expected_source: inspect_schema(source.path())
                .unwrap()
                .control
                .generation_ref(),
            attestations: vec![],
        },
    )
    .unwrap();
    // Sanctioned fixture-only policy setup after closing every writer. This is
    // NOT a production migration or a participating writer/barrier bypass.
    {
        use crate::storage::metadata::{META_TABLE, NAMESPACE_TABLE};
        use redb::ReadableTable;
        let db = redb::Database::create(path.join("meta.db")).unwrap();
        let tx = db.begin_write().unwrap();
        {
            let mut namespaces = tx.open_table(NAMESPACE_TABLE).unwrap();
            let rows: Vec<_> = namespaces
                .iter()
                .unwrap()
                .map(|row| {
                    let (key, value) = row.unwrap();
                    (key.value(), NamespaceConfig::decode(value.value()).unwrap())
                })
                .collect();
            for (key, mut ns) in rows {
                let schema = ns.schema.as_mut().unwrap();
                schema.lifecycle_version = version;
                schema.policy_revision = revision("2");
                namespaces
                    .insert(key, serde_json::to_vec(&ns).unwrap().as_slice())
                    .unwrap();
            }
            let mut table = tx.open_table(META_TABLE).unwrap();
            for (index, id) in ids.iter().enumerate() {
                let mut record = DiskRecord::from_bytes(
                    table
                        .get(id.as_bytes().as_slice())
                        .unwrap()
                        .unwrap()
                        .value(),
                )
                .unwrap();
                let fields = &mut record.schema.as_mut().unwrap().lifecycle;
                fields.lifecycle_version = version;
                fields.policy_revision = revision("1");
                edit(index, &mut record);
                table
                    .insert(
                        id.as_bytes().as_slice(),
                        record.encode().unwrap().as_slice(),
                    )
                    .unwrap();
            }
        }
        tx.commit().unwrap();
    }
    let files = crate::storage::validation::canonical_files(&path).unwrap();
    let view = Arc::new(OfflineSnapshot::open(&path).unwrap()); // full canonical validation
    OfflineFixture {
        _directory: directory,
        path,
        view,
        files,
        ids,
    }
}

struct NoSideEffects;
impl RecordCache for NoSideEffects {
    fn get(&self, _: &MemoryId) -> Option<CachedRecord> {
        panic!("false mode consulted behavioral cache")
    }
}
#[async_trait::async_trait]
impl AccessRecorder for NoSideEffects {
    async fn record_access(&self, _: MemoryId, _: AccessKind) -> Result<()> {
        panic!("offline exposure")
    }
    async fn record_access_batch(&self, _: &[(MemoryId, AccessKind)]) -> Result<()> {
        panic!("offline exposure batch")
    }
}
#[async_trait::async_trait]
impl EmbeddingProviderRegistry for NoSideEffects {
    async fn embed(&self, _: &str, _: &str) -> Result<Vec<f32>> {
        Ok(vec![1.0, 0.0, 0.0, 0.0])
    }
}
impl RifProcessor for NoSideEffects {
    fn compute_suppressions(&self, _: &[MemoryId], _: &[MemoryId]) -> Vec<RifSuppression> {
        vec![]
    }
}

fn engine(
    f: &OfflineFixture,
    clock: Arc<CountingClock>,
    edges: &[(usize, usize, EdgeType)],
) -> QueryEngine {
    use crate::search::adapters::*;
    use crate::search::{EntityIndex, FlatVectorIndex, FtsIndex, VectorIndex, VectorMetadata};
    let mut vectors = FlatVectorIndex::new(4);
    let fts = FtsIndex::new(f._directory.path()).unwrap();
    let mut entities = EntityIndex::new();
    let mut graph = crate::graph::RelationshipGraph::new();
    f.view
        .for_each_record(|item| {
            let r = CachedRecord::from(&item.record);
            if let Some(vector) = item.embedding {
                vectors
                    .add(
                        r.id,
                        &vector,
                        VectorMetadata {
                            namespace_id: r.namespace_id,
                            decay_phase: 1,
                            tags: vec![],
                        },
                    )
                    .unwrap();
            }
            if !matches!(r.phase, DecayPhase::Ghost | DecayPhase::Tombstone) {
                fts.add(r.namespace_id, r.id, &r.summary, None, &[])
                    .unwrap();
            }
            entities.add(r.id, &r.entities);
            // Deliberately stale phase and strength; explicit policy must ignore it.
            graph
                .add_node(r.id, r.namespace_id, DecayPhase::Full, 0.99, r.vector_slot)
                .unwrap();
            Ok(())
        })
        .unwrap();
    for &(a, b, kind) in edges {
        graph
            .add_edge(f.ids[a], f.ids[b], kind, 1.0, false)
            .unwrap();
    }
    let quiet = Arc::new(NoSideEffects);
    QueryEngine::new(
        f.view.clone(),
        quiet.clone(),
        Arc::new(SharedVectorIndexRegistry::new(Arc::new(
            tokio::sync::RwLock::new(vectors),
        ))),
        Arc::new(SharedFtsIndexRegistry::new(Arc::new(
            tokio::sync::Mutex::new(fts),
        ))),
        Arc::new(SharedEntityIndexReader::new(Arc::new(
            tokio::sync::RwLock::new(entities),
        ))),
        quiet.clone(),
        f.view.clone(),
        Arc::new(SharedGraphReader::new(Arc::new(tokio::sync::RwLock::new(
            graph,
        )))),
        quiet.clone(),
        quiet,
    )
    .with_lifecycle_inputs(clock, crate::decay::DecayConfig::default(), 1.0)
}

fn metadata() -> SearchQuery {
    query(
        json!({"queryMode":"metadata_only", "filter":{"requireTags":["project/query"]}, "recordAccess":false, "graphDepth":0, "limit":50}),
    )
}
fn file_bytes(files: &[std::path::PathBuf]) -> Vec<(std::path::PathBuf, Vec<u8>)> {
    files
        .iter()
        .map(|p| (p.clone(), std::fs::read(p).unwrap()))
        .collect()
}

fn result(r: &SemanticsResponse, id: MemoryId) -> &SearchResult {
    r.response
        .results
        .iter()
        .find(|r| r.memory_id == id)
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn canonical_clock_projection_is_read_only_and_retains_committed_phase_and_unknown_dates() {
    let f = offline(1, LifecycleVersion::ExplicitFeedbackV2, |_, r| {
        r.phase = DecayPhase::Summary;
        r.is_permastore = 1;
        r.strength = 0.01;
        r.decay_strength = 0.02;
        r.last_accessed_at = 20 * DAY; // not the explicit-policy anchor
        let fields = &mut r.schema.as_mut().unwrap().lifecycle;
        fields.observed_at = -10 * DAY;
        fields.retrieved_at = Some(20 * DAY);
        fields.reinforced_at = None;
        fields.decay_anchor_at = 0;
        fields.timestamp_origin = TimestampOrigin::LegacyInferred;
    })
    .await;
    let bytes = file_bytes(&f.files);
    let original = f.view.record(f.ids[0]).unwrap().unwrap().record;
    let cached = CachedRecord::from(&original);
    assert_eq!(
        cached.lifecycle.as_ref(),
        original.schema.as_ref().map(|s| &s.lifecycle)
    );
    let clock = CountingClock::new(0);
    let engine = engine(&f, clock.clone(), &[]);
    let first = engine
        .search_with_semantics(metadata(), current())
        .await
        .unwrap();
    assert_eq!(result(&first, f.ids[0]).retrievability, 1.0);
    clock.now.store(30 * DAY, Ordering::SeqCst);
    let second = engine
        .search_with_semantics(metadata(), current())
        .await
        .unwrap();
    let r = result(&second, f.ids[0]);
    assert!(r.retrievability < 0.7 && r.retrievability > 0.01);
    assert_eq!(r.effective_r, r.retrievability);
    assert_eq!(r.phase, DecayPhase::Summary);
    assert!(r.is_permastore);
    assert_eq!(r.summary.as_deref(), Some("needle"));
    assert_eq!(clock.calls.load(Ordering::SeqCst), 2);
    let observed = second.diagnostics.strength.unwrap();
    assert_eq!(observed.evaluated_at, 30 * DAY);
    assert_eq!(observed.policy_revision, Some(revision("2")));
    assert_eq!(
        observed.lifecycle_version,
        LifecycleVersion::ExplicitFeedbackV2
    );
    let mut filtered = metadata();
    filtered.filter.min_strength = Some(0.7);
    assert!(
        engine
            .search_with_semantics(filtered, current())
            .await
            .unwrap()
            .response
            .results
            .is_empty()
    );
    tokio::task::yield_now().await;
    assert_eq!(
        f.view
            .record(f.ids[0])
            .unwrap()
            .unwrap()
            .record
            .encode()
            .unwrap(),
        original.encode().unwrap()
    );
    assert_eq!(file_bytes(&f.files), bytes);
    drop(engine);
    drop(f.view);
    assert!(
        RedbStorageEngine::open(&f.path).is_err(),
        "projection cannot open the serving gate"
    );
    assert_eq!(file_bytes(&f.files), bytes);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn projected_eligibility_precedes_vector_fts_entity_and_metadata_caps() {
    let f = offline(105, LifecycleVersion::ExplicitFeedbackV2, |i, r| {
        r.stability = if i == 104 { 1000.0 } else { 0.01 };
        r.strength = if i == 104 { 0.01 } else { 1.0 };
        r.decay_strength = r.strength;
    })
    .await;
    let engine = engine(&f, CountingClock::new(DAY), &[]);
    let q = query(json!({"text":"needle", "entities":["caleb"], "limit":1,
        "minScore":0.0, "filter":{"minStrength":0.8}, "graphDepth":0, "recordAccess":false}));
    let out = engine.search_with_semantics(q, current()).await.unwrap();
    assert_eq!(out.response.results.len(), 1);
    assert_eq!(out.response.results[0].memory_id, f.ids[104]);
    let provenance = &out.diagnostics.provenance.as_ref().unwrap()[&f.ids[104]];
    for channel in [
        RetrievalChannel::Vector,
        RetrievalChannel::Fts,
        RetrievalChannel::Entity,
    ] {
        assert!(provenance.own_channels.contains(&channel), "{channel:?}");
    }
    assert_eq!(out.diagnostics.fts.coverage.as_ref().unwrap().eligible, 1);
    let mut q = metadata();
    q.limit = 1; // rank before the 100-candidate metadata cap, without minStrength
    let out = engine.search_with_semantics(q, current()).await.unwrap();
    assert_eq!(out.response.results[0].memory_id, f.ids[104]);
    assert!(out.diagnostics.metadata_candidate_limited);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn legacy_policy_materialized_strength_is_identical_under_both_retrieval_semantics() {
    let f = offline(2, LifecycleVersion::LegacyAccessV1, |i, r| {
        r.strength = if i == 0 { 0.8 } else { 0.4 };
        r.decay_strength = if i == 0 { 0.85 } else { 0.5 };
        r.stability = 0.01;
    })
    .await;
    let clock = CountingClock::new(0);
    let engine = engine(&f, clock.clone(), &[]);
    let mut expected = None;
    for now in [0, 300 * DAY] {
        clock.now.store(now, Ordering::SeqCst);
        for semantics in [RetrievalSemantics::V1, current()] {
            let mut q = metadata();
            q.filter.min_strength = Some(0.6);
            let out = engine.search_with_semantics(q, semantics).await.unwrap();
            assert_eq!(out.response.results.len(), 1);
            let r = &out.response.results[0];
            assert_eq!((r.retrievability, r.effective_r), (0.8, 0.85));
            let values = (r.memory_id, r.composite_score);
            if let Some(expected) = expected {
                assert_eq!(values, expected);
            } else {
                expected = Some(values);
            }
        }
    }
    assert_eq!(clock.calls.load(Ordering::SeqCst), 4);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bonuses_use_projected_live_same_scope_donors_independent_of_query_tags() {
    let f = offline(3, LifecycleVersion::ExplicitFeedbackV2, |i, r| {
        if i > 0 {
            r.tags.clear();
        } // donors do not match the query
        if i == 2 {
            r.phase = DecayPhase::Ghost;
            r.summary.clear();
            r.schema.as_mut().unwrap().labels.entities.clear();
        }
        r.stability = if i == 0 { 1.0 } else { 0.01 };
    })
    .await;
    // B is eligible as a bonus donor but not a query result; ghost C and
    // foreign D must not supply cached .99 strengths or relay second hops.
    let engine = engine(
        &f,
        CountingClock::new(DAY),
        &[
            (0, 1, EdgeType::Associative),
            (0, 2, EdgeType::Associative),
            (0, 3, EdgeType::Associative),
        ],
    );
    let out = engine
        .search_with_semantics(metadata(), current())
        .await
        .unwrap();
    let config = crate::decay::DecayConfig::default();
    let raw = crate::decay::FsrsEngine::new(&config);
    let target_r = raw.retrievability(1.0, 1.0, 1.0);
    let donor_r = raw.retrievability(1.0, 0.01, 1.0);
    let bonus = (2.0 - 2.0_f32.ln()) * donor_r * 0.05;
    let expected = raw.effective_retrievability(target_r, bonus);
    let r = result(&out, f.ids[0]);
    assert!((r.effective_r - expected).abs() < 1e-7);
    assert_eq!(out.response.results.len(), 1);
    let mut q = metadata();
    q.filter.min_strength = Some((target_r + expected) / 2.0);
    assert!(
        engine
            .search_with_semantics(q, current())
            .await
            .unwrap()
            .response
            .results
            .is_empty(),
        "hard minStrength uses raw, not bonused R"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn explicit_policy_fails_closed_before_legacy_exposure_under_either_semantics() {
    let f = offline(1, LifecycleVersion::ExplicitFeedbackV2, |_, _| {}).await;
    let bytes = file_bytes(&f.files);
    let engine = engine(&f, CountingClock::new(DAY), &[]);
    for semantics in [RetrievalSemantics::V1, current()] {
        let mut q = metadata();
        q.record_access = true;
        let error = engine
            .search_with_semantics(q, semantics)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("recordAccess must be false"));
        let out = engine
            .search_with_semantics(metadata(), semantics)
            .await
            .unwrap();
        assert!(result(&out, f.ids[0]).retrievability < 1.0);
    }
    assert!(
        engine
            .get_memory_with_access(f.ids[0], false)
            .await
            .unwrap_err()
            .to_string()
            .contains("projection unavailable")
    );
    assert!(
        engine
            .similar_with_access(f.ids[0], 1, false)
            .await
            .unwrap_err()
            .to_string()
            .contains("projection unavailable")
    );
    assert!(engine.get_memory_with_access(f.ids[0], true).await.is_err());
    assert!(engine.similar_with_access(f.ids[0], 1, true).await.is_err());
    assert_eq!(file_bytes(&f.files), bytes);
}

struct PinnedNamespace {
    config: NamespaceConfig,
    calls: AtomicUsize,
}
#[async_trait::async_trait]
impl NamespaceResolver for PinnedNamespace {
    async fn resolve(&self, _: &str) -> Result<NamespaceConfig> {
        assert_eq!(
            self.calls.fetch_add(1, Ordering::SeqCst),
            0,
            "namespace policy reread mid-query"
        );
        Ok(self.config.clone())
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn query_pins_current_revision_and_resolves_namespace_override_before_global() {
    let f = offline(1, LifecycleVersion::ExplicitFeedbackV2, |_, r| {
        r.schema.as_mut().unwrap().lifecycle.decay_anchor_at = DAY;
    })
    .await;
    let clock = CountingClock::new(9 * DAY);
    let mut observed = Vec::new();
    for multiplier in [None, Some(0.0), Some(1.0), Some(2.0)] {
        let mut engine = engine(&f, clock.clone(), &[]).with_lifecycle_inputs(
            clock.clone(),
            crate::decay::DecayConfig::default(),
            2.0,
        );
        let mut ns = f
            .view
            .namespaces()
            .iter()
            .find(|n| n.id == ns())
            .unwrap()
            .clone();
        ns.decay_rate_multiplier = multiplier;
        ns.schema.as_mut().unwrap().policy_revision = revision("3");
        let pinned = Arc::new(PinnedNamespace {
            config: ns,
            calls: AtomicUsize::new(0),
        });
        engine.namespace_resolver = pinned.clone();
        let out = engine
            .search_with_semantics(metadata(), current())
            .await
            .unwrap();
        assert_eq!(pinned.calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            out.diagnostics.strength.unwrap().policy_revision,
            Some(revision("3"))
        );
        observed.push(result(&out, f.ids[0]).retrievability);
    }
    assert_eq!(observed[0], observed[3]); // global 2 and namespace 2
    assert_eq!(observed[1], 1.0); // multiplier 0
    assert!(observed[2] < observed[3] && observed[3] < observed[1]);
    assert_eq!(clock.calls.load(Ordering::SeqCst), 4);
}

/// The raw namespace enumeration is sound; a subsequent canonical read reveals
/// malformed/mismatched lifecycle provenance. Revalidation must fail, not use
/// the initial score or silently fall back to the materialized replacement.
struct InvalidRevalidation {
    source: Arc<OfflineSnapshot>,
    target: MemoryId,
    corruption: u8,
}
impl InvalidRevalidation {
    fn corrupt(&self, record: &mut CachedRecord) {
        if record.id != self.target {
            return;
        }
        match self.corruption {
            0 => record.lifecycle = None,
            1 => {
                record.lifecycle.as_mut().unwrap().lifecycle_version =
                    LifecycleVersion::LegacyAccessV1
            }
            2 => record.lifecycle.as_mut().unwrap().policy_revision = revision("3"),
            _ => record.stability = f32::NAN,
        }
    }
}
#[async_trait::async_trait]
impl MetadataStore for InvalidRevalidation {
    async fn get(&self, id: &MemoryId) -> Result<Option<CachedRecord>> {
        let mut record = MetadataStore::get(self.source.as_ref(), id).await?;
        if let Some(record) = &mut record {
            self.corrupt(record);
        }
        Ok(record)
    }
    async fn get_batch(&self, ids: &[MemoryId]) -> Result<Vec<CachedRecord>> {
        let mut records = MetadataStore::get_batch(self.source.as_ref(), ids).await?;
        // Initial complete namespace read stays valid; replacement batch has one ID.
        if ids.len() == 1 {
            for record in &mut records {
                self.corrupt(record);
            }
        }
        Ok(records)
    }
    async fn list_namespace_memories(&self, namespace: NamespaceId) -> Result<Vec<MemoryId>> {
        self.source.list_namespace_memories(namespace).await
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn replacement_revalidation_rejects_missing_version_revision_and_stability_errors() {
    let f = offline(2, LifecycleVersion::ExplicitFeedbackV2, |_, _| {}).await;
    for corruption in 0..4 {
        for semantics in [RetrievalSemantics::V1, current()] {
            let mut engine = engine(&f, CountingClock::new(DAY), &[(1, 0, EdgeType::Supersedes)]);
            engine.meta_store = Arc::new(InvalidRevalidation {
                source: f.view.clone(),
                target: f.ids[1],
                corruption,
            });
            let error = engine
                .search_with_semantics(metadata(), semantics)
                .await
                .unwrap_err();
            assert!(
                matches!(error, SearchError::MetadataError(_)),
                "{corruption}: {error}"
            );
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn projected_graph_activation_and_filtered_successors_do_not_use_materialized_strength() {
    let f = offline(3, LifecycleVersion::ExplicitFeedbackV2, |i, r| {
        r.stability = if i == 1 { 0.01 } else { 10.0 };
        r.strength = 1.0;
        r.decay_strength = 1.0;
    })
    .await;
    let clock = CountingClock::new(DAY);
    let engine = engine(
        &f,
        clock.clone(),
        &[(1, 0, EdgeType::Supersedes), (0, 2, EdgeType::Associative)],
    );
    let mut q = metadata();
    q.filter.min_strength = Some(0.8);
    q.graph_depth = 1;
    let out = engine.search_with_semantics(q, current()).await.unwrap();
    assert!(
        out.response
            .results
            .iter()
            .all(|r| r.memory_id != f.ids[0] && r.memory_id != f.ids[1])
    );
    assert!(
        out.diagnostics
            .unresolved
            .get(&UnresolvedReason::Ineligible)
            .copied()
            .unwrap_or(0)
            > 0
    );
    assert_eq!(clock.calls.load(Ordering::SeqCst), 1);
    // A direct engine graph read with projected eligibility measures the same
    // strength consumers as the query expansion, independently of source scores.
    let mut ns_records = MetadataStore::get_batch(f.view.as_ref(), &f.ids[..3])
        .await
        .unwrap();
    let config = f.view.resolve("default").await.unwrap();
    let mut context = StrengthContext::pin(&engine, &config, false).unwrap();
    context
        .project_namespace(&mut ns_records, engine.graph.as_ref())
        .unwrap();
    let eligible: HashMap<_, _> = ns_records.into_iter().map(|r| (r.id, r)).collect();
    let first = engine
        .graph
        .expand_eligible(&[(f.ids[0], 1.0)], ns(), 1, &eligible)
        .unwrap();
    let activation = first
        .hits
        .iter()
        .find(|h| h.id == f.ids[2])
        .unwrap()
        .activation;
    clock.now.store(200 * DAY, Ordering::SeqCst);
    let mut context = StrengthContext::pin(&engine, &config, false).unwrap();
    let mut records = MetadataStore::get_batch(f.view.as_ref(), &f.ids[..3])
        .await
        .unwrap();
    context
        .project_namespace(&mut records, engine.graph.as_ref())
        .unwrap();
    let eligible = records.into_iter().map(|r| (r.id, r)).collect();
    let later = engine
        .graph
        .expand_eligible(&[(f.ids[0], 1.0)], ns(), 1, &eligible)
        .unwrap();
    let later_activation = later
        .hits
        .iter()
        .find(|h| h.id == f.ids[2])
        .map_or(0.0, |h| h.activation);
    assert!(later_activation < activation);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn returned_graph_result_uses_the_same_projection_as_admission_and_ranking() {
    let f = offline(2, LifecycleVersion::ExplicitFeedbackV2, |_, r| {
        r.strength = 1.0;
        r.decay_strength = 1.0;
    })
    .await;
    let bytes = file_bytes(&f.files);
    let clock = CountingClock::new(DAY);
    let engine = engine(&f, clock.clone(), &[(0, 1, EdgeType::Associative)]);
    let q = query(json!({"text":"needle", "minScore":0.8, "graphDepth":1, "recordAccess":false}));
    let first = engine
        .search_with_semantics(q.clone(), current())
        .await
        .unwrap();
    let target = result(&first, f.ids[1]);
    assert!(
        target.activation_score.is_some(),
        "low direct relevance is recovered by graph expansion"
    );
    assert!(target.retrievability < 1.0);
    assert!(target.effective_r > target.retrievability);
    assert_eq!(
        first.diagnostics.provenance.as_ref().unwrap()[&f.ids[1]].own_channels,
        [RetrievalChannel::Graph].into_iter().collect()
    );
    let activation = target.activation_score.unwrap();
    clock.now.store(200 * DAY, Ordering::SeqCst);
    let later = engine.search_with_semantics(q, current()).await.unwrap();
    if let Some(target) = later
        .response
        .results
        .iter()
        .find(|r| r.memory_id == f.ids[1])
    {
        assert!(target.activation_score.unwrap() < activation);
        assert!(target.retrievability < result(&first, f.ids[1]).retrievability);
    }
    assert_eq!(clock.calls.load(Ordering::SeqCst), 2);
    assert_eq!(file_bytes(&f.files), bytes);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn legacy_retrieval_graph_formulas_also_consume_explicit_policy_projection() {
    use crate::search::adapters::SharedVectorIndexRegistry;
    use crate::search::{FlatVectorIndex, VectorIndex, VectorMetadata};
    let f = offline(2, LifecycleVersion::ExplicitFeedbackV2, |_, _| {}).await;
    let clock = CountingClock::new(DAY);
    let mut engine = engine(&f, clock.clone(), &[(0, 1, EdgeType::Associative)]);
    // Only the seed has a direct channel hit; the neighbor is graph-discovered.
    let item = f.view.record(f.ids[0]).unwrap().unwrap();
    let mut index = FlatVectorIndex::new(4);
    index
        .add(
            f.ids[0],
            &item.embedding.unwrap(),
            VectorMetadata {
                namespace_id: ns(),
                decay_phase: 1,
                tags: vec![],
            },
        )
        .unwrap();
    engine.vector_indexes = Arc::new(SharedVectorIndexRegistry::new(Arc::new(
        tokio::sync::RwLock::new(index),
    )));
    let q = query(
        json!({"text":"needle", "ftsQuery":"no-keyword-match", "graphDepth":1, "recordAccess":false}),
    );
    let out = engine
        .search_with_semantics(q, RetrievalSemantics::V1)
        .await
        .unwrap();
    let target = result(&out, f.ids[1]);
    let config = crate::decay::DecayConfig::default();
    let raw = crate::decay::FsrsEngine::new(&config).retrievability(1.0, 1.0, 1.0);
    let expected = raw * (2.0 - 2.0_f32.ln()) * 0.75 * raw;
    assert!((target.activation_score.unwrap() - expected).abs() < 1e-7);
    assert_eq!(target.retrievability, raw);
    assert!(target.effective_r > raw);
    assert_eq!(clock.calls.load(Ordering::SeqCst), 1);
}
