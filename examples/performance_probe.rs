//! Isolated actual-engine instrumentation probe. No provider/network or serving-v2.
use anyhow::{Result, ensure};
use clap::Parser;
use recalld::{
    model::*,
    search::{adapters::*, *},
    storage::{RedbStorageEngine, StorageEngine},
};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{BufWriter, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
const NOW: i64 = 1_767_225_600_000;
#[derive(Parser)]
struct Args {
    #[arg(long)]
    store: PathBuf,
    #[arg(long)]
    out: PathBuf,
    #[arg(long, default_value_t = 7)]
    seed: u64,
    #[arg(long, default_value_t = 10000)]
    records: usize,
    #[arg(long, default_value_t = 200)]
    warmup: usize,
    #[arg(long, default_value_t = 1000)]
    samples: usize,
    #[arg(long, default_value_t = 60)]
    throughput_seconds: u64,
    #[arg(long)]
    initialize: bool,
    #[arg(long)]
    trace: bool,
}
#[derive(Clone)]
struct Sink(
    Arc<Mutex<BufWriter<fs::File>>>,
    Arc<std::sync::atomic::AtomicU64>,
);
impl Write for Sink {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        let result = self.0.lock().unwrap().write(b);
        if result.is_err() {
            self.1.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        result
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.0.lock().unwrap().flush()
    }
}
impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Sink {
    type Writer = Self;
    fn make_writer(&'a self) -> Self {
        self.clone()
    }
}
struct NoCache;
impl RecordCache for NoCache {
    fn get(&self, _: &MemoryId) -> Option<CachedRecord> {
        panic!("read-only cache")
    }
}
fn vectors(seed: u64, n: usize) -> Vec<Vec<f32>> {
    let mut state = seed;
    (0..n)
        .map(|_| {
            let mut v: Vec<_> = (0..384)
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
fn digest_files(root: &Path) -> Result<String> {
    fn walk(root: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
        for entry in fs::read_dir(root)? {
            let p = entry?.path();
            if p.is_dir() {
                walk(&p, out)?;
            } else if p.file_name().unwrap() == "meta.db"
                || p.extension().is_some_and(|e| e == "dat")
            {
                out.push(p);
            }
        }
        Ok(())
    }
    let mut files = vec![];
    walk(root, &mut files)?;
    files.sort();
    let mut h = Sha256::new();
    for file in files {
        h.update(file.strip_prefix(root)?.to_string_lossy().as_bytes());
        h.update(fs::read(file)?);
    }
    Ok(h.finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>())
}
// Cross-process corpus identity excludes physical redb open/close bookkeeping.
// Physical bytes are independently compared before/after EACH measured interval.
fn canonical_state(storage: &RedbStorageEngine) -> Result<String> {
    let mut hash = Sha256::new();
    hash.update(serde_json::to_vec(&storage.control_state()?)?);
    let mut namespaces = storage.list_namespaces()?;
    namespaces.sort_by_key(|n| n.id);
    hash.update(serde_json::to_vec(&namespaces)?);
    let mut records = storage.scan_all()?;
    records.sort_by_key(|r| r.0);
    for (id, record) in records {
        hash.update(id.as_bytes());
        hash.update(record.encode()?);
        for value in storage
            .get_vector(NamespaceId::new(record.namespace_id), record.vector_slot)?
            .unwrap()
        {
            hash.update(value.to_bits().to_le_bytes());
        }
    }
    ensure!(
        storage.load_all_edges()?.is_empty(),
        "this workload has no canonical edges"
    );
    Ok(hash.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> Result<()> {
    let args = Args::parse();
    ensure!(
        args.records >= 200 && args.records % 10 == 0,
        "invalid corpus"
    );
    let fixture = vectors(args.seed, args.records);
    if args.initialize {
        ensure!(!args.store.exists(), "fresh initialization required");
        let mut storage = RedbStorageEngine::open(&args.store)?;
        for n in 0..10 {
            let mut ns = NamespaceConfig::default_namespace(NOW);
            ns.name = format!("n{n}");
            ns.embedding_dim = 384;
            ns.decay_rate_multiplier = Some(1.0);
            ensure!(
                storage.create_namespace(&ns)?.get() == n + 1,
                "namespace ID"
            );
        }
        for (i, v) in fixture.iter().enumerate() {
            let id = MemoryId::from_uuid(uuid::Uuid::from_u128((i + 1) as u128));
            let ns = NamespaceId::new((i % 10 + 1) as u32);
            let mut r = DiskRecord {
                version: 1,
                schema: None,
                id: *id.as_bytes(),
                namespace_id: ns.get(),
                created_at: NOW,
                last_accessed_at: NOW,
                phase: DecayPhase::Full,
                strength: 1.0,
                decay_strength: 1.0,
                stability: 1.0,
                difficulty: 5.0,
                is_permastore: 0,
                vector_slot: 0,
                edge_count: 0,
                summary: "needle fixture".into(),
                tags: vec![],
                access_history: vec![],
                text_offset: 0,
                text_length: 0,
            };
            storage.insert_memory(id, ns, &mut r, v, None)?;
        }
        storage.sync()?;
        return Ok(());
    }
    fs::create_dir(&args.out)?;
    let sink = Sink(
        Arc::new(Mutex::new(BufWriter::new(fs::File::create(
            args.out.join("events.jsonl"),
        )?))),
        Arc::new(std::sync::atomic::AtomicU64::new(0)),
    );
    // The same subscriber exists in all arms; only candidate-enabled admits events.
    tracing_subscriber::fmt()
        .json()
        .without_time()
        .with_writer(sink.clone())
        .with_env_filter(if args.trace {
            "off,recalld::performance=trace"
        } else {
            "off"
        })
        .init();
    let mut storage = RedbStorageEngine::open(&args.store)?;
    let rows = storage.scan_all()?;
    ensure!(rows.len() == args.records, "corpus count");
    let mut index = FlatVectorIndex::new(384);
    let fts = FtsIndex::new(&args.out)?;
    let mut graph = recalld::graph::RelationshipGraph::new();
    for (id, r) in rows {
        let ns = NamespaceId::new(r.namespace_id);
        let v = storage.get_vector(ns, r.vector_slot)?.unwrap();
        index.add(
            id,
            &v,
            VectorMetadata {
                namespace_id: ns,
                decay_phase: 1,
                tags: vec![],
            },
        )?;
        fts.add(ns, id, &r.summary, None, &[])?;
        graph.add_node(id, ns, r.phase, r.strength, r.vector_slot)?;
    }
    storage.sync()?;
    let before = digest_files(&args.store)?;
    let canonical_before = canonical_state(&storage)?;
    let storage = Arc::new(std::sync::RwLock::new(storage));
    let graph = Arc::new(tokio::sync::RwLock::new(graph));
    let cache = Arc::new(recalld::cache::CacheManager::new(
        recalld::cache::CacheConfig {
            embedding_dim: 384,
            ..Default::default()
        },
        None,
    ));
    let clock = Arc::new(recalld::time::ManualClock::new(NOW));
    let engine = QueryEngine::new(
        Arc::new(StorageNamespaceResolver::new(storage.clone())),
        Arc::new(SingleEmbeddingRegistry::new(Arc::new(
            recalld::embedding::PassthroughProvider::new(384),
        ))),
        Arc::new(SharedVectorIndexRegistry::new(Arc::new(
            tokio::sync::RwLock::new(index),
        ))),
        Arc::new(SharedFtsIndexRegistry::new(Arc::new(
            tokio::sync::Mutex::new(fts),
        ))),
        Arc::new(SharedEntityIndexReader::new(Arc::new(
            tokio::sync::RwLock::new(EntityIndex::new()),
        ))),
        Arc::new(NoCache),
        Arc::new(StorageMetadataAdapter::new(storage.clone())),
        Arc::new(SharedGraphReader::new(graph.clone())),
        Arc::new(RifProcessorAdapter::new(
            Arc::new(recalld::rif::RifEngine::new(recalld::rif::RifConfig {
                enabled: false,
                ..Default::default()
            })),
            graph,
        )),
        Arc::new(StorageAccessRecorder::new(storage.clone(), cache).with_clock(clock.clone())),
    )
    .with_lifecycle_inputs(clock, Default::default(), 1.0);
    let queries:Vec<SearchQuery>=(0..100.min(args.records / 10)).map(|i| serde_json::from_value(serde_json::json!({
        "namespace":"n0","text":"needle","queryEmbedding":fixture[i*10],"limit":20,"recordAccess":false,"graphDepth":0
    })).unwrap()).collect();
    let mut samples = BufWriter::new(fs::File::create(args.out.join("samples.jsonl"))?);
    let mut result_hash = Sha256::new();
    let mut times = vec![];
    for i in 0..args.warmup + args.samples {
        let start = Instant::now();
        let response = engine.search(queries[i % queries.len()].clone()).await?;
        sink.0.lock().unwrap().flush()?;
        ensure!(
            sink.1.load(std::sync::atomic::Ordering::Relaxed) == 0,
            "trace sink lost events"
        ); // sink service included in request latency
        let elapsed = start.elapsed().as_nanos() as u64;
        ensure!(response.results.len() == 20, "result count");
        if i >= args.warmup {
            for r in &response.results {
                result_hash.update(r.memory_id.as_bytes());
                result_hash.update(r.composite_score.unwrap().to_bits().to_le_bytes());
            }
            times.push(elapsed);
            writeln!(
                samples,
                "{}",
                serde_json::json!({"sample":i-args.warmup,"latency_ns":elapsed,"stage_us":response.timings})
            )?;
        }
    }
    let start = Instant::now();
    let mut completed = 0;
    while start.elapsed() < Duration::from_secs(args.throughput_seconds) || completed < 1000 {
        engine
            .search(queries[completed % queries.len()].clone())
            .await?;
        sink.0.lock().unwrap().flush()?;
        ensure!(
            sink.1.load(std::sync::atomic::Ordering::Relaxed) == 0,
            "trace sink lost events"
        );
        completed += 1;
    }
    let seconds = start.elapsed().as_secs_f64();
    samples.flush()?;
    ensure!(
        before == digest_files(&args.store)?,
        "false mode changed canonical bytes"
    );
    ensure!(
        canonical_before == canonical_state(&storage.read().unwrap())?,
        "canonical state changed"
    );
    times.sort();
    let p95 = times[(times.len() * 95).div_ceil(100) - 1];
    fs::write(
        args.out.join("summary.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "seed":args.seed,"records":args.records,"trace":args.trace,"samples":args.samples,"p95_ns":p95,
            "throughput_completed":completed,"throughput_seconds":seconds,"requests_per_second":completed as f64/seconds,
            "result_sha256":result_hash.finalize().iter().map(|b| format!("{b:02x}")).collect::<String>(),"canonical_file_sha256":before,"canonical_state_sha256":canonical_before,
            "proc_status":fs::read_to_string("/proc/self/status").unwrap_or_default()
        }))?,
    )?;
    Ok(())
}
