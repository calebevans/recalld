//! Cached record weight calculation and centrality-adjusted eviction.
//!
//! Provides the weigher function for moka's capacity-based eviction and the
//! centrality discount curve that makes high-degree hub nodes stickier in cache.

use std::sync::Arc;

use crate::model::{CachedRecord, MemoryId, Tag};

/// A cached representation of a memory record, as stored in the moka cache.
///
/// This is a type alias re-exporting `crate::model::CachedRecord` for
/// convenience within the cache module. The canonical definition lives in
/// `crate::model::record`.
pub use crate::model::CachedRecord as CacheRecord;

/// Estimate the byte cost of a CachedRecord for moka's weight budget.
///
/// Accounts for stack size, heap allocations (String, Vec), Arc
/// overhead, and moka's internal bookkeeping. This is an approximation
/// -- allocator fragmentation is absorbed by the 10% overhead margin
/// in the budget split (Spec 06, Section 3).
///
/// Typical return: ~650-800 bytes per entry.
pub fn weigh_cached_record(record: &CachedRecord) -> u32 {
    let base = std::mem::size_of::<CachedRecord>(); // ~80 bytes stack fields
    let summary_heap = record.summary.len() // heap String content
        + std::mem::size_of::<String>(); // 24 bytes (ptr+len+cap)
    let tags_heap = record
        .tags
        .iter()
        .map(|t| t.as_str().len() + std::mem::size_of::<Tag>())
        .sum::<usize>()
        + std::mem::size_of::<Vec<Tag>>(); // 24 bytes (ptr+len+cap)
    let arc_overhead = std::mem::size_of::<usize>() * 2; // strong + weak refcount
    let moka_overhead = 64_usize; // internal bookkeeping estimate

    let total = base + summary_heap + tags_heap + arc_overhead + moka_overhead;
    total.try_into().unwrap_or(u32::MAX)
}

/// Calculate the effective weight of a CachedRecord, applying a
/// centrality discount to make high-degree nodes stickier in cache.
///
/// The discount reduces the *reported* weight (not actual memory
/// usage). This makes moka's capacity accounting slightly inaccurate
/// -- the cache may use ~5-10% more memory than `max_capacity` in
/// the worst case. The budget split accounts for this headroom.
///
/// # Centrality discount curve
///
/// ```text
/// discount_factor = 1.0 - 0.30 * (1 - e^(-degree / 10))
///
///   degree  0:  discount =  0.0%  (no edges = no bonus)
///   degree  5:  discount = 11.8%
///   degree 10:  discount = 18.9%
///   degree 20:  discount = 25.9%
///   degree 50:  discount = 29.9%  (approaches 30% cap)
/// ```
///
/// # Arguments
///
/// * `record` - The cached record to weigh.
/// * `centrality` - The degree centrality value. In the standard path
///   this is `record.edge_count as f32`, but it is accepted as a
///   parameter so callers (e.g., tests) can inject arbitrary values.
///
/// # Returns
///
/// Effective weight in bytes as `u32`. Minimum 128 to prevent
/// pathological pinning of massive hub nodes.
pub fn calculate_weight(record: &CachedRecord, centrality: f32) -> u32 {
    let base_weight = weigh_cached_record(record) as f64;

    let discount = 1.0 - 0.30 * (1.0 - (-centrality as f64 / 10.0).exp());
    let effective = (base_weight * discount) as u32;

    // Floor: never report less than 128 bytes. Prevents a hub node
    // with 1000+ edges from reporting near-zero weight and pinning
    // indefinitely.
    effective.max(128)
}

/// Weigher function matching moka's expected signature.
///
/// Called by the moka cache on every insertion to determine the
/// entry's contribution toward the capacity budget.
///
/// Signature: `Fn(&K, &V) -> u32`
pub fn moka_weigher(_key: &MemoryId, value: &Arc<CachedRecord>) -> u32 {
    calculate_weight(value, value.edge_count as f32)
}
