//! Cache warming: warm.bin I/O, startup sequence, and prefetch worker.
//!
//! Provides the `WarmSnapshot` aggregate for shutdown persistence,
//! `load_warm_file` / `write_warm_file` for warm.bin round-trips,
//! the `warm_cache` startup warming task, and the background
//! `prefetch_worker` that speculatively loads neighbors.

use std::path::Path;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;

use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;

use crate::cache::pressure::PressureLevel;
use crate::model::{CachedRecord, MemoryId};

// ─── Constants ──────────────────────────────────────────────────────

/// Magic bytes identifying a warm.bin file.
const WARM_MAGIC: [u8; 4] = *b"WARM";

/// Current warm.bin format version.
const WARM_VERSION: u32 = 1;

/// Size of the warm.bin header in bytes.
const WARM_HEADER_SIZE: usize = 16;

// ─── Dependency-injection traits for cache warming ──────────────────
//
// These traits define the warming subsystem's contracts against
// external dependencies. They are intentionally narrow to keep the
// cache module decoupled. Adapter implementations live in
// `cache/warming_adapters.rs`.

/// Storage engine interface for cache warming.
///
/// Provides record, embedding, and edge loading from disk.
#[async_trait::async_trait]
pub trait StorageEngine: Send + Sync + 'static {
    /// Load a single `CachedRecord` from disk by ID.
    async fn load_record(&self, id: MemoryId) -> Result<Option<CachedRecord>, anyhow::Error>;

    /// Load an embedding vector from `vectors.dat`.
    fn load_embedding(
        &self,
        vector_slot: u32,
        namespace_id: crate::model::NamespaceId,
    ) -> Result<Vec<f32>, anyhow::Error>;

    /// Load outgoing edges for a given memory.
    async fn load_outgoing_edges(
        &self,
        id: MemoryId,
    ) -> Result<Vec<(MemoryId, crate::model::EdgeType)>, anyhow::Error>;
}

/// Concurrent vector buffer for inserting embeddings during warming.
pub trait ConcurrentVectorBuffer: Send + Sync + 'static {
    /// Insert an embedding vector for the given memory.
    fn insert(&self, id: MemoryId, embedding: &[f32]);
}

/// Edge store used by `enqueue_neighbors_for_prefetch`.
#[async_trait::async_trait]
pub trait EdgeStore: Send + Sync + 'static {
    /// Load outgoing edges for a given memory.
    async fn outgoing_edges(
        &self,
        id: MemoryId,
    ) -> Result<Vec<(MemoryId, crate::model::EdgeType)>, anyhow::Error>;
}

// ─── Wire Types ─────────────────────────────────────────────────────

/// Header for warm.bin. Fixed 16 bytes at the start of the file.
///
/// Layout:
///   [0..4]   magic:   b"WARM"
///   [4..8]   version: u32 LE
///   [8..12]  count:   u32 LE (number of WarmEntry records)
///   [12..16] crc32:   u32 LE (CRC32 of all entry bytes after the header)
#[derive(Debug, Clone, Copy)]
#[repr(C)]
pub struct WarmHeader {
    /// Magic bytes (must be `b"WARM"`).
    pub magic: [u8; 4],
    /// Format version.
    pub version: u32,
    /// Number of WarmEntry records.
    pub count: u32,
    /// CRC32 of all entry bytes.
    pub crc32: u32,
}

impl WarmHeader {
    /// Byte size of the header on disk.
    pub const SIZE: usize = WARM_HEADER_SIZE;

    /// Serialize to a 16-byte little-endian buffer.
    pub fn to_bytes(&self) -> [u8; Self::SIZE] {
        let mut buf = [0u8; Self::SIZE];
        buf[0..4].copy_from_slice(&self.magic);
        buf[4..8].copy_from_slice(&self.version.to_le_bytes());
        buf[8..12].copy_from_slice(&self.count.to_le_bytes());
        buf[12..16].copy_from_slice(&self.crc32.to_le_bytes());
        buf
    }

    /// Parse from a 16-byte little-endian buffer.
    pub fn from_bytes(buf: &[u8; Self::SIZE]) -> Self {
        Self {
            magic: [buf[0], buf[1], buf[2], buf[3]],
            version: u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]),
            count: u32::from_le_bytes([buf[8], buf[9], buf[10], buf[11]]),
            crc32: u32::from_le_bytes([buf[12], buf[13], buf[14], buf[15]]),
        }
    }
}

/// A single entry in warm.bin. Fixed 32 bytes.
///
/// Sorted by `priority_score` descending in the file so the warming
/// loop can iterate in insertion order without re-sorting.
///
/// Uses `#[repr(C)]` for deterministic field ordering across compilations.
/// We use raw byte transmutation instead of rkyv/bincode for simplicity
/// since WarmEntry is a fixed-size, naturally-aligned `#[repr(C)]` struct.
#[derive(Debug, Clone, Copy)]
#[repr(C)]
pub struct WarmEntry {
    /// Memory UUID bytes (16 bytes, from MemoryId::as_bytes()).
    pub id: [u8; 16],
    /// Last access timestamp in milliseconds since Unix epoch.
    pub last_accessed_at: i64,
    /// Degree centrality (edge_count) at shutdown time.
    pub degree_centrality: f32,
    /// Eviction priority score at shutdown time (higher = more important).
    pub priority_score: f32,
}

// Static assertion: 16 + 8 + 4 + 4 = 32 bytes.
const _: () = assert!(std::mem::size_of::<WarmEntry>() == 32);

// ─── WarmSnapshot ───────────────────────────────────────────────────

/// A complete warm.bin snapshot: header metadata + priority-ordered entries.
///
/// Constructed from a live cache on shutdown, or parsed from a file on startup.
#[derive(Debug)]
pub struct WarmSnapshot {
    /// Format version (always WARM_VERSION for writes).
    pub version: u32,
    /// Timestamp when the snapshot was created (millis since epoch).
    pub timestamp: i64,
    /// Priority-ordered entries (highest priority first).
    pub entries: Vec<WarmEntry>,
}

impl WarmSnapshot {
    /// Build a snapshot from the live moka cache.
    ///
    /// Iterates the cache (lock-free), computes priority scores,
    /// and sorts entries by priority descending.
    pub fn from_cache(
        cache: &moka::future::Cache<MemoryId, Arc<CachedRecord>>,
    ) -> Self {
        let mut entries: Vec<WarmEntry> = cache
            .iter()
            .map(|(id, record)| {
                let memory_id: MemoryId = *id;
                WarmEntry {
                    id: *memory_id.as_bytes(),
                    last_accessed_at: record.last_accessed_at,
                    degree_centrality: record.edge_count as f32,
                    priority_score: compute_priority(&record),
                }
            })
            .collect();

        // Sort by priority descending -- highest-value entries load first.
        entries.sort_unstable_by(|a, b| {
            b.priority_score
                .partial_cmp(&a.priority_score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        Self {
            version: WARM_VERSION,
            timestamp: chrono::Utc::now().timestamp_millis(),
            entries,
        }
    }
}

// ─── Writing warm.bin ───────────────────────────────────────────────

/// Write the warm.bin file atomically.
///
/// Steps:
/// 1. Serialize all entries to a byte buffer.
/// 2. Compute CRC32 over the entry bytes.
/// 3. Write header + entries to a temp file.
/// 4. fsync the temp file.
/// 5. Atomic rename to the final path.
pub async fn write_warm_file(
    cache: &moka::future::Cache<MemoryId, Arc<CachedRecord>>,
    path: &Path,
) -> std::io::Result<()> {
    let snapshot = WarmSnapshot::from_cache(cache);

    if snapshot.entries.is_empty() {
        tracing::info!("cache empty -- skipping warm.bin write");
        return Ok(());
    }

    // Serialize entries to a contiguous byte buffer.
    // SAFETY: WarmEntry is #[repr(C)] with no padding (32 bytes, all fields
    // are naturally aligned). Transmuting to bytes is safe.
    let entry_bytes: &[u8] = unsafe {
        std::slice::from_raw_parts(
            snapshot.entries.as_ptr() as *const u8,
            snapshot.entries.len() * std::mem::size_of::<WarmEntry>(),
        )
    };

    // CRC32 over all entry bytes.
    let crc = crc32fast::hash(entry_bytes);

    let header = WarmHeader {
        magic: WARM_MAGIC,
        version: snapshot.version,
        count: snapshot.entries.len() as u32,
        crc32: crc,
    };

    // Write to temp file, fsync, then atomic rename.
    let tmp_path = path.with_extension("warm.tmp");
    let mut file = tokio::fs::File::create(&tmp_path).await?;
    file.write_all(&header.to_bytes()).await?;
    file.write_all(entry_bytes).await?;
    file.sync_all().await?;

    tokio::fs::rename(&tmp_path, path).await?;

    tracing::info!(
        "wrote warm.bin: {} entries, {} bytes",
        snapshot.entries.len(),
        WarmHeader::SIZE + entry_bytes.len(),
    );

    Ok(())
}

// ─── Loading warm.bin ───────────────────────────────────────────────

/// Result of loading warm.bin.
#[derive(Debug)]
pub enum WarmLoadResult {
    /// Successfully loaded N entries.
    Loaded(Vec<WarmEntry>),
    /// File does not exist -- first run or was deleted.
    NotFound,
    /// File exists but failed validation. The String contains the reason.
    Corrupt(String),
}

/// Load and validate warm.bin.
///
/// Reads the file into memory, validates the header (magic, version, CRC32),
/// and returns the entries as an owned Vec.
///
/// Stale entry handling: entries referencing deleted memories are NOT
/// filtered here. The warming loop skips them when the disk load returns
/// `None`.
pub fn load_warm_file(path: &Path) -> WarmLoadResult {
    // Check existence.
    if !path.exists() {
        tracing::info!("warm.bin not found -- cold start");
        return WarmLoadResult::NotFound;
    }

    // Read the file into memory.
    let data = match std::fs::read(path) {
        Ok(d) => d,
        Err(e) => {
            tracing::warn!("failed to read warm.bin: {}", e);
            return WarmLoadResult::Corrupt(format!("read failed: {}", e));
        }
    };

    // Validate minimum size.
    if data.len() < WarmHeader::SIZE {
        return WarmLoadResult::Corrupt("file too small for header".into());
    }

    // Parse header.
    let header_bytes: &[u8; WarmHeader::SIZE] = data[..WarmHeader::SIZE]
        .try_into()
        .expect("slice length verified above");
    let header = WarmHeader::from_bytes(header_bytes);

    // Validate magic.
    if header.magic != WARM_MAGIC {
        return WarmLoadResult::Corrupt(format!(
            "bad magic: expected {:?}, got {:?}",
            WARM_MAGIC, header.magic
        ));
    }

    // Validate version.
    if header.version != WARM_VERSION {
        return WarmLoadResult::Corrupt(format!(
            "unsupported version: expected {}, got {}",
            WARM_VERSION, header.version
        ));
    }

    // Validate file size.
    let expected_entry_bytes =
        header.count as usize * std::mem::size_of::<WarmEntry>();
    let entry_data = &data[WarmHeader::SIZE..];

    if entry_data.len() < expected_entry_bytes {
        // Truncated file. Load as many complete entries as possible.
        let recoverable =
            entry_data.len() / std::mem::size_of::<WarmEntry>();
        tracing::warn!(
            "warm.bin truncated: expected {} entries, can recover {}",
            header.count,
            recoverable,
        );
        if recoverable == 0 {
            return WarmLoadResult::Corrupt(
                "truncated, no recoverable entries".into(),
            );
        }
        // Fall through -- CRC will fail, but we handle that below.
    }

    // Validate CRC32 over entry bytes.
    let valid_entry_bytes =
        &entry_data[..std::cmp::min(expected_entry_bytes, entry_data.len())];
    let computed_crc = crc32fast::hash(valid_entry_bytes);

    let expected_total = WarmHeader::SIZE + expected_entry_bytes;
    if computed_crc != header.crc32 {
        // CRC mismatch. If file is truncated, this is expected.
        if data.len() < expected_total {
            tracing::warn!(
                "warm.bin CRC mismatch (truncated file) -- loading recoverable entries"
            );
        } else {
            tracing::warn!(
                "warm.bin CRC mismatch: expected {:#010x}, computed {:#010x}",
                header.crc32,
                computed_crc,
            );
            return WarmLoadResult::Corrupt("CRC32 mismatch".into());
        }
    }

    // Copy entries from the buffer into an owned Vec.
    // SAFETY: WarmEntry is #[repr(C)], all fields are naturally aligned,
    // and we verified the byte count above.
    let entry_count =
        valid_entry_bytes.len() / std::mem::size_of::<WarmEntry>();
    let entries: Vec<WarmEntry> = unsafe {
        let ptr = valid_entry_bytes.as_ptr() as *const WarmEntry;
        std::slice::from_raw_parts(ptr, entry_count).to_vec()
    };

    tracing::info!("loaded warm.bin: {} entries", entries.len());
    WarmLoadResult::Loaded(entries)
}

// ─── Startup Warming Sequence ───────────────────────────────────────

/// Background cache warming task.
///
/// Sequence:
/// 1. Load warm.bin.
/// 2. Validate header and CRC.
/// 3. For each entry in priority order:
///    a. Skip if already in cache (loaded by a concurrent query).
///    b. Load CachedRecord from meta.db.
///    c. Load embedding from vectors.dat.
///    d. Insert into moka cache + vector buffer.
///    e. Enqueue 1-hop neighbors for prefetch.
/// 4. Log completion stats.
///
/// Cooperative scheduling: yields every 500 entries to avoid starving
/// query-handling tasks on the Tokio runtime.
pub async fn warm_cache<S, V>(
    warm_path: std::path::PathBuf,
    cache: moka::future::Cache<MemoryId, Arc<CachedRecord>>,
    storage: Arc<S>,
    vector_buffer: Arc<V>,
    reverse_index: Arc<
        dashmap::DashMap<MemoryId, std::collections::HashSet<MemoryId>>,
    >,
    prefetch_tx: mpsc::Sender<PrefetchRequest>,
) where
    S: StorageEngine,
    V: ConcurrentVectorBuffer,
{
    // Load warm.bin.
    let entries = match load_warm_file(&warm_path) {
        WarmLoadResult::Loaded(entries) => entries,
        WarmLoadResult::NotFound => {
            tracing::info!(
                "no warm.bin -- cache will warm organically via queries"
            );
            return;
        }
        WarmLoadResult::Corrupt(reason) => {
            tracing::warn!(
                "warm.bin corrupt ({}), falling back to cold start",
                reason
            );
            return;
        }
    };

    let total = entries.len();
    let mut loaded = 0u64;
    let mut skipped_cached = 0u64;
    let mut skipped_missing = 0u64;
    let mut errors = 0u64;
    let start = std::time::Instant::now();

    // Load entries in priority order.
    for (i, entry) in entries.iter().enumerate() {
        let memory_id = MemoryId::from_bytes(entry.id);

        // Skip if a concurrent query already loaded this entry.
        if cache.contains_key(&memory_id) {
            skipped_cached += 1;
            continue;
        }

        // Load from disk.
        match storage.load_record(memory_id).await {
            Ok(Some(record)) => {
                // Load embedding into vector buffer.
                if let Ok(embedding) =
                    storage.load_embedding(record.vector_slot, record.namespace_id)
                {
                    vector_buffer.insert(memory_id, &embedding);
                }

                // Update reverse neighborhood index.
                if let Ok(neighbors) =
                    storage.load_outgoing_edges(memory_id).await
                {
                    for (target_id, _edge_type) in &neighbors {
                        reverse_index
                            .entry(*target_id)
                            .or_default()
                            .insert(memory_id);
                    }
                }

                let arc_record = Arc::new(record);
                cache.insert(memory_id, arc_record).await;
                loaded += 1;

                // Enqueue 1-hop neighbors for prefetch.
                // Best-effort -- dropped if the prefetch channel is full.
                if let Ok(neighbors) =
                    storage.load_outgoing_edges(memory_id).await
                {
                    for (target_id, _edge_type) in neighbors {
                        if !cache.contains_key(&target_id) {
                            let _ = prefetch_tx
                                .try_send(PrefetchRequest::Eager(target_id));
                        }
                    }
                }
            }
            Ok(None) => {
                // Memory was deleted since last shutdown.
                skipped_missing += 1;
            }
            Err(e) => {
                tracing::warn!(
                    "warming: failed to load {:?}: {}",
                    memory_id,
                    e
                );
                errors += 1;
            }
        }

        // Cooperative scheduling: yield every 500 entries.
        if (i + 1) % 500 == 0 {
            tokio::task::yield_now().await;
        }

        // Progress logging every 10,000 entries.
        if (i + 1) % 10_000 == 0 {
            tracing::info!(
                "warming progress: {}/{} entries ({} loaded, {} skipped, {} missing)",
                i + 1,
                total,
                loaded,
                skipped_cached,
                skipped_missing,
            );
        }
    }

    let elapsed = start.elapsed();
    tracing::info!(
        "warming complete in {:.2?}: {} loaded, {} already cached, {} deleted, {} errors (from {} warm.bin entries)",
        elapsed,
        loaded,
        skipped_cached,
        skipped_missing,
        errors,
        total,
    );

    // Flush moka maintenance after bulk insert.
    cache.run_pending_tasks().await;
}

// ─── Prefetch Request ───────────────────────────────────────────────

/// A request to speculatively load a memory into the cache.
///
/// Sent via bounded mpsc channel (capacity: 4096).
/// When the channel is full, requests are silently dropped (best-effort).
#[derive(Debug)]
pub enum PrefetchRequest {
    /// K=1 -- direct neighbor of an accessed memory.
    /// Loaded immediately by the prefetch worker.
    Eager(MemoryId),

    /// K=2 -- neighbor-of-neighbor.
    /// Loaded only when the worker has no Eager requests pending.
    Lazy(MemoryId),
}

impl PrefetchRequest {
    /// Returns the target MemoryId regardless of variant.
    pub fn memory_id(&self) -> MemoryId {
        match self {
            PrefetchRequest::Eager(id) | PrefetchRequest::Lazy(id) => *id,
        }
    }

    /// Returns true if this is an Eager (K=1) request.
    pub fn is_eager(&self) -> bool {
        matches!(self, PrefetchRequest::Eager(_))
    }
}

// ─── Prefetch Metrics ───────────────────────────────────────────────

/// Metrics for monitoring prefetch effectiveness.
pub struct PrefetchMetrics {
    /// Total requests received from the channel.
    pub received: metrics::Counter,
    /// Skipped because the entry was already in cache.
    pub already_cached: metrics::Counter,
    /// Successfully loaded from disk into cache.
    pub prefetched: metrics::Counter,
    /// Not found on disk (memory deleted between enqueue and load).
    pub not_found: metrics::Counter,
    /// Disk I/O errors during load.
    pub errors: metrics::Counter,
    /// Skipped due to memory pressure restrictions.
    pub pressure_skipped: metrics::Counter,
}

impl PrefetchMetrics {
    /// Create a new set of prefetch metrics.
    pub fn new() -> Self {
        Self {
            received: metrics::counter!("prefetch.received"),
            already_cached: metrics::counter!("prefetch.already_cached"),
            prefetched: metrics::counter!("prefetch.prefetched"),
            not_found: metrics::counter!("prefetch.not_found"),
            errors: metrics::counter!("prefetch.errors"),
            pressure_skipped: metrics::counter!("prefetch.pressure_skipped"),
        }
    }
}

impl Default for PrefetchMetrics {
    fn default() -> Self {
        Self::new()
    }
}

// ─── Prefetch Worker ────────────────────────────────────────────────

/// Background prefetch worker.
///
/// Runs until the channel is closed (all senders dropped = shutdown).
///
/// Behavior under memory pressure (checked via `pressure_level` AtomicU8):
/// - Normal:   process all requests (Eager + Lazy).
/// - Warning:  process Eager only, skip Lazy.
/// - Critical: skip ALL requests (drain channel without loading).
pub async fn prefetch_worker<S, V>(
    mut rx: mpsc::Receiver<PrefetchRequest>,
    cache: moka::future::Cache<MemoryId, Arc<CachedRecord>>,
    storage: Arc<S>,
    vector_buffer: Arc<V>,
    pressure_level: Arc<AtomicU8>,
    lazy_tx: mpsc::Sender<PrefetchRequest>,
    metrics: PrefetchMetrics,
) where
    S: StorageEngine,
    V: ConcurrentVectorBuffer,
{
    while let Some(req) = rx.recv().await {
        metrics.received.increment(1);

        // Check pressure level.
        let pressure =
            PressureLevel::from_u8(pressure_level.load(Ordering::Relaxed));

        match pressure {
            PressureLevel::Critical => {
                // Drain without loading -- just discard.
                metrics.pressure_skipped.increment(1);
                continue;
            }
            PressureLevel::Warning => {
                // Skip all Lazy (K=2) requests.
                if !req.is_eager() {
                    metrics.pressure_skipped.increment(1);
                    continue;
                }
            }
            PressureLevel::Normal => {
                // Process everything.
            }
        }

        let memory_id = req.memory_id();

        // Double-check: skip if already cached.
        if cache.contains_key(&memory_id) {
            metrics.already_cached.increment(1);
            continue;
        }

        // Load from disk.
        match storage.load_record(memory_id).await {
            Ok(Some(record)) => {
                // Load embedding for vector search buffer.
                if let Ok(embedding) =
                    storage.load_embedding(record.vector_slot, record.namespace_id)
                {
                    vector_buffer.insert(memory_id, &embedding);
                }

                cache.insert(memory_id, Arc::new(record)).await;
                metrics.prefetched.increment(1);

                // If Eager (K=1) and pressure Normal, enqueue neighbors
                // as Lazy (K=2) prefetches via the feedback loop sender.
                if req.is_eager() && pressure == PressureLevel::Normal {
                    if let Ok(neighbors) =
                        storage.load_outgoing_edges(memory_id).await
                    {
                        for (target_id, _edge_type) in neighbors {
                            if !cache.contains_key(&target_id) {
                                let _ = lazy_tx.try_send(
                                    PrefetchRequest::Lazy(target_id),
                                );
                            }
                        }
                    }
                }
            }
            Ok(None) => {
                metrics.not_found.increment(1);
            }
            Err(e) => {
                tracing::warn!(
                    "prefetch failed for {:?}: {}",
                    memory_id,
                    e
                );
                metrics.errors.increment(1);
            }
        }

        // Yield after each load to avoid starving query tasks.
        tokio::task::yield_now().await;
    }

    tracing::info!("prefetch worker exiting -- channel closed");
}

// ─── Enqueue Helper ─────────────────────────────────────────────────

/// Enqueue 1-hop neighbors for background prefetch after a cache access.
///
/// Called by CacheManager::get() on every cache hit or read-through load.
/// Under Warning pressure, the worker will skip Lazy requests; at Critical,
/// all requests are skipped.
pub async fn enqueue_neighbors_for_prefetch<E: EdgeStore>(
    record: &CachedRecord,
    edge_store: &E,
    cache: &moka::future::Cache<MemoryId, Arc<CachedRecord>>,
    tx: &mpsc::Sender<PrefetchRequest>,
    pressure_level: &AtomicU8,
) {
    let pressure =
        PressureLevel::from_u8(pressure_level.load(Ordering::Relaxed));

    // Critical: don't even query edges.
    if pressure == PressureLevel::Critical {
        return;
    }

    let neighbors = match edge_store.outgoing_edges(record.id).await {
        Ok(n) => n,
        Err(e) => {
            tracing::debug!("failed to load edges for prefetch: {}", e);
            return;
        }
    };

    for (target_id, _edge_type) in neighbors {
        // Skip already-cached neighbors.
        if cache.contains_key(&target_id) {
            continue;
        }

        let _ = tx.try_send(PrefetchRequest::Eager(target_id));
    }
}

// ─── Priority Score ─────────────────────────────────────────────────

/// Compute eviction priority for a cached record.
/// Higher score = higher priority to keep.
///
/// Formula (from Spec 06, Section 7.3):
///   score = 0.6 * recency + 0.4 * centrality
/// where:
///   recency    = 1 / (1 + hours_since_access)
///   centrality = 1 - e^(-degree / 10)
pub fn compute_priority(record: &CachedRecord) -> f32 {
    let now_ms = chrono::Utc::now().timestamp_millis();
    let age_hours =
        (now_ms - record.last_accessed_at) as f32 / 3_600_000.0;
    let degree = record.edge_count as f32;

    let recency_score = 1.0 / (1.0 + age_hours);
    let centrality_score = 1.0 - (-degree / 10.0_f32).exp();

    0.6 * recency_score + 0.4 * centrality_score
}
