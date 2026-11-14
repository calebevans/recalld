//! Internal production-engine diagnostic; NOT the A6 public-action harness.
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File},
    io::Write,
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, ensure};
use clap::Parser;
use recalld::{
    cache::{CacheConfig, CacheManager},
    graph::RelationshipGraph,
    model::{AccessKind, DecayPhase, DiskRecord, MemoryId, NamespaceConfig, NamespaceId},
    rif::{RifConfig, RifEngine},
    search::{adapters::*, *},
    storage::{RedbStorageEngine, StorageEngine},
};
use serde_json::{Value, json};

const TIME: i64 = 1_767_225_600_000;

#[derive(Parser, Debug)]
struct Args {
    #[arg(long)]
    out: PathBuf,
    #[arg(long, default_value_t = 1000)]
    records: usize,
    #[arg(long, default_value_t = 384)]
    dimensions: usize,
    #[arg(long, default_value_t = 20)]
    k: usize,
    #[arg(long, default_value_t = 20)]
    warmup: usize,
    #[arg(long, default_value_t = 100)]
    samples: usize,
    #[arg(long, default_value_t = 7)]
    seed: u64,
}

// SplitMix64, with explicitly defined integer -> f32 conversion. No RNG-version drift.
fn vectors(seed: u64, count: usize, dim: usize) -> Vec<Vec<f32>> {
    let mut state = seed;
    (0..count)
        .map(|_| {
            let mut v: Vec<f32> = (0..dim)
                .map(|_| {
                    state = state.wrapping_add(0x9e3779b97f4a7c15);
                    let mut z = state;
                    z = (z ^ (z >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
                    z = (z ^ (z >> 27)).wrapping_mul(0x94d049bb133111eb);
                    ((z ^ (z >> 31)) >> 40) as f32 / 8_388_608.0 - 1.0
                })
                .collect();
            normalize_l2(&mut v);
            v
        })
        .collect()
}

fn id(i: usize) -> MemoryId {
    // Stable fixture identity, not a production-generated UUID.
    MemoryId::from_uuid(uuid::Uuid::from_u128((i + 1) as u128))
}

fn namespace(i: usize) -> NamespaceId {
    NamespaceId::new(if i.is_multiple_of(10) { 1 } else { 2 })
}

// Independent scalar f64 oracle; never used to generate the engine's results.
fn oracle(vectors: &[Vec<f32>], query: usize, filtered: bool, k: usize) -> Vec<(MemoryId, f64)> {
    let mut scores: Vec<_> = vectors
        .iter()
        .enumerate()
        .filter(|(i, _)| !filtered || namespace(*i).get() == 1)
        .map(|(i, v)| {
            (
                id(i),
                v.iter()
                    .zip(&vectors[query])
                    .map(|(a, b)| *a as f64 * *b as f64)
                    .sum::<f64>(),
            )
        })
        .collect();
    scores.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
    scores.truncate(k);
    scores
}

fn percentiles(values: &[f64]) -> Value {
    if values.is_empty() {
        return Value::Null;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    let at = |p: f64| sorted[((p * sorted.len() as f64).ceil() as usize).saturating_sub(1)];
    json!({"count": sorted.len(), "p50": at(0.50), "p95": at(0.95), "p99": at(0.99)})
}

// Source IDs are evaluator-only. Deduplicate at first occurrence before DCG.
fn evidence_metrics(relevant: &[usize], retained: &[usize], retrieved: &[usize]) -> Value {
    let gold: BTreeSet<_> = relevant.iter().copied().collect();
    if gold.is_empty() {
        return json!({"scored": false, "reason": "missing_or_empty_annotations"});
    }
    let retained: BTreeSet<_> = retained.iter().copied().collect();
    let mut seen = BTreeSet::new();
    let ranked: Vec<_> = retrieved
        .iter()
        .copied()
        .filter(|s| seen.insert(*s))
        .collect();
    let recall =
        |k| ranked.iter().take(k).filter(|s| gold.contains(s)).count() as f64 / gold.len() as f64;
    let dcg: f64 = ranked
        .iter()
        .take(10)
        .enumerate()
        .filter(|(_, s)| gold.contains(s))
        .map(|(i, _)| 1.0 / ((i + 2) as f64).log2())
        .sum();
    let ideal: f64 = (0..gold.len().min(10))
        .map(|i| 1.0 / ((i + 2) as f64).log2())
        .sum();
    json!({"scored": true, "annotatedSourceUnits": gold.len(),
        "extractionEvidenceRetention": gold.intersection(&retained).count() as f64 / gold.len() as f64,
        "retrievalRecallAt5": recall(5), "retrievalRecallAt10": recall(10),
        "retrievalRecallAt20": recall(20), "allRequiredEvidence": recall(usize::MAX) == 1.0,
        "binarySourceNdcgAt10": dcg / ideal,
        "propositionPreservation": "not measured", "answerQuality": "not measured"})
}

struct ReplayEmbeddings(BTreeMap<String, Vec<f32>>);
#[async_trait::async_trait]
impl EmbeddingProviderRegistry for ReplayEmbeddings {
    async fn embed(&self, namespace: &str, text: &str) -> Result<Vec<f32>, SearchError> {
        if namespace != "fixture" {
            return Err(SearchError::NamespaceNotFound(namespace.into()));
        }
        self.0
            .get(text)
            .cloned()
            .ok_or_else(|| SearchError::EmbeddingFailed("offline replay miss".into()))
    }
}

// The current public API cannot disable access. Suppress ONLY in this diagnostic,
// and acknowledge every scheduled batch so post-query inspection is not racy.
struct DiagnosticAccessRecorder {
    completed: tokio::sync::Semaphore,
}
#[async_trait::async_trait]
impl AccessRecorder for DiagnosticAccessRecorder {
    async fn record_access(&self, _: MemoryId, _: AccessKind) -> Result<(), SearchError> {
        self.completed.add_permits(1);
        Ok(())
    }
    async fn record_access_batch(&self, _: &[(MemoryId, AccessKind)]) -> Result<(), SearchError> {
        self.completed.add_permits(1);
        Ok(())
    }
}

fn write_json(path: impl AsRef<std::path::Path>, value: &Value) -> Result<()> {
    fs::write(path, serde_json::to_vec_pretty(value)?)?;
    Ok(())
}

fn record_state(storage: &RedbStorageEngine) -> Result<Value> {
    // DiskRecord has a canonical byte encoder, deliberately no Serialize impl.
    let records: BTreeMap<_, _> = storage
        .scan_all()?
        .into_iter()
        .map(|(id, record)| (id, record.to_bytes()))
        .collect();
    Ok(serde_json::to_value(records)?)
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> Result<()> {
    let args = Args::parse();
    ensure!(
        (100..=100_000).contains(&args.records) && args.records % 10 == 0,
        "records must be a multiple of 10 in 100..=100000"
    );
    ensure!(
        (4..=8192).contains(&args.dimensions),
        "dimensions must be in 4..=8192"
    );
    ensure!(
        (1..=200).contains(&args.k) && args.k <= args.records / 10,
        "k must fit the eligible corpus and 1..=200"
    );
    ensure!(
        (1..=100_000).contains(&args.samples) && args.warmup <= 100_000,
        "invalid sample count"
    );
    fs::create_dir(&args.out).context("output must be a fresh directory")?;
    let mut trace = File::create(args.out.join("trace.jsonl"))?;
    let fixture = vectors(args.seed, args.records, args.dimensions);
    write_json(
        args.out.join("fixture.json"),
        &json!({"generator": "splitmix64-f32-l2-v1", "seed": args.seed,
        "time": TIME, "vectors": fixture, "sourceMappingVersion": 1,
        "sources": (0..args.records).map(|i| json!({"sourceId": i, "memoryId": id(i),
            "ordinal": i, "namespace": namespace(i).get(), "transformation": "raw_synthetic_unit",
            "ingestPlan": "store_raw_unit"})).collect::<Vec<_>>()}),
    )?;

    let mut index = FlatVectorIndex::new(args.dimensions);
    for (i, v) in fixture.iter().enumerate() {
        index.add(
            id(i),
            v,
            VectorMetadata {
                namespace_id: namespace(i),
                decay_phase: 1,
                tags: vec![],
            },
        )?;
    }
    let mut summaries = Vec::new();
    let mut replay = Vec::new();
    for filtered in [false, true] {
        let cell = if filtered {
            "exact-index-10pct"
        } else {
            "exact-index-100pct"
        };
        let filter = SearchFilter {
            namespace_id: filtered.then(|| NamespaceId::new(1)),
            ..Default::default()
        };
        let mut times = Vec::new();
        for step in 0..args.warmup + args.samples {
            let q = (step % (args.records / 10)) * 10;
            let start = Instant::now();
            let result = index.search(&fixture[q], args.k, &filter)?;
            let elapsed = start.elapsed().as_secs_f64() * 1e6;
            let expected = oracle(&fixture, q, filtered, args.k);
            ensure!(
                result.len() == expected.len(),
                "exact result count mismatch"
            );
            // Random continuous fixtures avoid designed ties; near ties are tolerated
            // only in score, never by silently substituting a different UUID.
            for (actual, expected) in result.iter().zip(&expected) {
                ensure!(
                    actual.id == expected.0 && (actual.score as f64 - expected.1).abs() <= 2e-6,
                    "scalar exact oracle mismatch in {cell}"
                );
            }
            if step >= args.warmup {
                times.push(elapsed);
                let ids: Vec<_> = result.iter().map(|r| r.id).collect();
                writeln!(
                    trace,
                    "{}",
                    json!({"diagnosticTraceVersion": 1, "cell": cell, "step": step - args.warmup,
                    "querySource": q, "latencyUs": elapsed, "resultIds": ids, "oracleRecallAtK": 1.0})
                )?;
                replay.push(json!({"cell": cell, "query": q, "ids": ids}));
            }
        }
        summaries.push(
            json!({"cell": cell, "attempted": args.samples, "completed": times.len(), "failed": 0,
            "latencyUs": percentiles(&times), "eligibleOracleRecallAtK": 1.0}),
        );
    }

    // Fresh fixture-only storage. This deliberately bypasses public ingestion.
    let store_dir = tempfile::tempdir()?;
    let build_start = Instant::now();
    let mut storage = RedbStorageEngine::open(store_dir.path())?;
    for (name, expected_id) in [("fixture", 1), ("distractors", 2)] {
        let mut ns = NamespaceConfig::default_namespace(TIME);
        ns.name = name.into();
        ns.embedding_dim = args.dimensions as u32;
        ns.decay_rate_multiplier = Some(0.0);
        ensure!(
            storage.create_namespace(&ns)?.get() == expected_id,
            "unexpected namespace identity"
        );
    }
    let fts = FtsIndex::new(store_dir.path())?;
    let mut graph = RelationshipGraph::new();
    let mut ledger = File::create(args.out.join("turn-ledger.jsonl"))?;
    for (i, v) in fixture.iter().enumerate() {
        writeln!(
            ledger,
            "{}",
            json!({"sourceId": i, "memoryId": id(i), "status": "attempted"})
        )?;
        let mut record = DiskRecord {
            version: DiskRecord::CURRENT_VERSION,
            id: *id(i).as_bytes(),
            namespace_id: namespace(i).get(),
            created_at: TIME,
            last_accessed_at: TIME,
            phase: DecayPhase::Full,
            strength: 1.0,
            decay_strength: 1.0,
            stability: 1.0,
            difficulty: 5.0,
            is_permastore: 0,
            vector_slot: 0,
            edge_count: 0,
            summary: format!("fixture token{i}"),
            tags: vec![],
            access_history: vec![],
            text_offset: 0,
            text_length: 0,
        };
        storage.insert_memory(id(i), namespace(i), &mut record, v, None)?;
        fts.add(namespace(i), id(i), &record.summary, None, &[])?;
        graph.add_node(
            id(i),
            namespace(i),
            DecayPhase::Full,
            1.0,
            record.vector_slot,
        )?;
        writeln!(
            ledger,
            "{}",
            json!({"sourceId": i, "memoryId": id(i), "status": "accepted"})
        )?;
    }
    let build_us = build_start.elapsed().as_secs_f64() * 1e6;
    let before = record_state(&storage)?;
    write_json(args.out.join("state-before.json"), &before)?;
    let storage = Arc::new(std::sync::RwLock::new(storage));
    let graph = Arc::new(tokio::sync::RwLock::new(graph));
    let recorder = Arc::new(DiagnosticAccessRecorder {
        completed: tokio::sync::Semaphore::new(0),
    });
    let embeddings = Arc::new(ReplayEmbeddings(
        (0..args.records)
            .step_by(10)
            .map(|i| (format!("query{i}"), fixture[i].clone()))
            .collect(),
    ));
    ensure!(
        embeddings
            .embed("fixture", "unrecorded input")
            .await
            .is_err(),
        "replay misses must fail closed"
    );
    let engine = QueryEngine::new(
        Arc::new(StorageNamespaceResolver::new(storage.clone())),
        embeddings,
        Arc::new(SharedVectorIndexRegistry::new(Arc::new(
            tokio::sync::RwLock::new(index),
        ))),
        Arc::new(SharedFtsIndexRegistry::new(Arc::new(
            tokio::sync::Mutex::new(fts),
        ))),
        Arc::new(SharedEntityIndexReader::new(Arc::new(
            tokio::sync::RwLock::new(EntityIndex::new()),
        ))),
        Arc::new(CacheManagerAdapter::new(Arc::new(CacheManager::new(
            CacheConfig {
                embedding_dim: args.dimensions,
                ..Default::default()
            },
            None,
        )))),
        Arc::new(StorageMetadataAdapter::new(storage.clone())),
        Arc::new(SharedGraphReader::new(graph.clone())),
        Arc::new(RifProcessorAdapter::new(
            Arc::new(RifEngine::new(RifConfig {
                enabled: false,
                ..Default::default()
            })),
            graph,
        )),
        recorder.clone(),
    );

    let retained: Vec<_> = (0..args.records).collect();
    for hybrid in [false, true] {
        let cell = if hybrid {
            "internal-engine-hybrid"
        } else {
            "internal-engine-vector"
        };
        let mut stages: BTreeMap<String, Vec<f64>> = BTreeMap::new();
        for step in 0..args.warmup + args.samples {
            let q = (step % (args.records / 10)) * 10;
            let query: SearchQuery = serde_json::from_value(json!({"text": format!("query{q}"),
                "ftsQuery": if hybrid { format!("token{q}") } else { "unmatchedfixtureterm".into() },
                "namespace": "fixture", "limit": args.k, "graphDepth": 0}))?;
            let start = Instant::now();
            let response = engine.search(query).await?;
            let call_us = start.elapsed().as_secs_f64() * 1e6;
            ensure!(!response.results.is_empty(), "empty fixture response");
            tokio::time::timeout(Duration::from_secs(30), recorder.completed.acquire())
                .await??
                .forget();
            let drained_us = start.elapsed().as_secs_f64() * 1e6;
            let encode_start = Instant::now();
            let encoded = serde_json::to_vec(&response)?;
            let encode_us = encode_start.elapsed().as_secs_f64() * 1e6;
            ensure!(
                response.results[0].memory_id == id(q),
                "self-retrieval baseline failed"
            );
            if hybrid {
                ensure!(
                    response.results[0]
                        .fts_score
                        .is_some_and(|score| score > 0.0),
                    "hybrid fixture did not exercise FTS"
                );
            } else {
                ensure!(
                    response.results.iter().all(|r| r.fts_score.is_none()),
                    "vector control unexpectedly matched FTS"
                );
            }
            ensure!(
                response.results.iter().all(|r| r.namespace == "fixture"
                    && r.phase == DecayPhase::Full
                    && (r.memory_id.into_inner().as_u128() - 1) % 10 == 0),
                "eligibility violation"
            );
            if !hybrid {
                let expected: Vec<_> = oracle(&fixture, q, true, args.k)
                    .into_iter()
                    .filter(|s| s.1 >= 0.0)
                    .map(|s| s.0)
                    .collect();
                ensure!(
                    response
                        .results
                        .iter()
                        .map(|r| r.memory_id)
                        .collect::<Vec<_>>()
                        == expected,
                    "vector engine versus eligible scalar oracle mismatch"
                );
            }
            if step >= args.warmup {
                let ids: Vec<_> = response.results.iter().map(|r| r.memory_id).collect();
                let sources: Vec<_> = ids
                    .iter()
                    .map(|id| (id.into_inner().as_u128() - 1) as usize)
                    .collect();
                let mut timing = serde_json::to_value(
                    response.timings.as_ref().context("missing stage timings")?,
                )?;
                timing["engineCallUs"] = json!(call_us);
                timing["engineAndDiagnosticDrainUs"] = json!(drained_us);
                timing["jsonSerializationUs"] = json!(encode_us);
                for (name, value) in timing.as_object().unwrap() {
                    stages
                        .entry(name.clone())
                        .or_default()
                        .push(value.as_f64().unwrap());
                }
                writeln!(
                    trace,
                    "{}",
                    json!({"diagnosticTraceVersion": 1, "cell": cell, "step": step - args.warmup,
                    "querySource": q, "resultIds": ids, "stageUs": timing, "responseBytes": encoded.len(),
                    "totalMatchesCandidateCount": response.total_matches,
                    "metrics": evidence_metrics(&[q], &retained, &sources)})
                )?;
                replay.push(json!({"cell": cell, "query": q, "results": response.results}));
            }
        }
        summaries.push(json!({"cell": cell, "attempted": args.samples, "completed": args.samples, "failed": 0,
            "stageUs": stages.iter().map(|(k,v)| (k.clone(), percentiles(v))).collect::<BTreeMap<_,_>>() }));
    }
    let after = record_state(&storage.read().unwrap())?;
    ensure!(before == after, "diagnostic queries mutated stored records");
    write_json(args.out.join("state-after.json"), &after)?;
    write_json(args.out.join("replay.json"), &json!(replay))?;
    write_json(
        args.out.join("summary.json"),
        &json!({"diagnosticManifestVersion": 1,
        "track": "legacy-internal-offline-foundation", "publicApiConformance": false,
        "syntheticSemanticQualityClaim": false, "records": args.records, "dimensions": args.dimensions,
        "k": args.k, "seed": args.seed, "warmupPerCell": args.warmup, "samplesPerCell": args.samples,
        "fixtureIngestUs": build_us, "canonicalRecordsUnchanged": true,
        "extraction": {"track": "raw-synthetic-unit", "sourceUnits": args.records,
            "attempted": args.records, "accepted": args.records, "rejected": 0,
            "sourceUnitCoverage": 1.0, "propositionPreservation": "not measured"},
        "cells": summaries, "unmeasured": ["transport", "liveEmbedding", "lockWaitHold", "scanCounters",
            "receiptsLedgerReadiness", "answerQuality", "groupedConfidenceIntervals", "sustainableThroughput"]}),
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn scorer_and_percentile_goldens() {
        assert_eq!(percentiles(&[]), Value::Null);
        assert_eq!(
            percentiles(&(1..=100).map(f64::from).collect::<Vec<_>>())["p95"],
            95.0
        );
        assert_eq!(evidence_metrics(&[], &[], &[])["scored"], false);
        assert_eq!(evidence_metrics(&[1], &[1], &[])["retrievalRecallAt5"], 0.0);
        let partial = evidence_metrics(&[1, 2], &[1], &[1, 1, 3]);
        assert_eq!(partial["extractionEvidenceRetention"], 0.5);
        assert_eq!(partial["retrievalRecallAt5"], 0.5);
        assert_eq!(partial["allRequiredEvidence"], false);
        let perfect = evidence_metrics(&[1, 2], &[1, 2], &[1, 1, 2]);
        assert_eq!(perfect["binarySourceNdcgAt10"], 1.0);
        assert_eq!(perfect["allRequiredEvidence"], true);
    }
    #[test]
    fn fixture_is_seeded_normalized_and_oracle_filters() {
        let a = vectors(7, 100, 16);
        assert_eq!(a, vectors(7, 100, 16));
        assert_ne!(a, vectors(19, 100, 16));
        assert!(
            a.iter()
                .all(|v| (v.iter().map(|x| x * x).sum::<f32>() - 1.0).abs() < 1e-6)
        );
        let ranks = oracle(&a, 0, true, 5);
        assert_eq!(ranks[0].0, id(0));
        assert!(
            ranks
                .iter()
                .all(|r| (r.0.into_inner().as_u128() - 1) % 10 == 0)
        );
    }
}
