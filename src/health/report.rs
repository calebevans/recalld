//! Health report computation functions.
//!
//! All compute_* functions are pure domain logic extracted from
//! `api::handlers`. They operate on pre-loaded records and accept
//! FSRS configuration as parameters -- no direct FsrsCalculator
//! instantiation, no HTTP types.

use std::path::Path as FilePath;

use crate::api::models::{
    AgeDistribution, AtRiskMemory, DecayForecast, HealthOverview, MetadataStats, PhaseCounts,
    StorageBreakdown, TagCount, TransitionCounts, VectorFileSize,
};
use crate::api::{NamespaceRegistry, StorageEngine};
use crate::decay::config::DecayConfig;
use crate::decay::fsrs::FsrsEngine as FsrsCalculator;
use crate::model::decay::DecayPhase;
use crate::model::id::{MemoryId, NamespaceId};
use crate::model::namespace::NamespaceConfig;

/// Build a [`DecayConfig`] from a [`NamespaceConfig`]'s phase thresholds.
pub fn decay_config_for_namespace(ns: &NamespaceConfig) -> DecayConfig {
    DecayConfig {
        initial_stability: ns.initial_stability,
        phase_1_threshold: ns.phase_thresholds.full_to_summary,
        phase_2_threshold: ns.phase_thresholds.summary_to_ghost,
        phase_3_threshold: ns.phase_thresholds.ghost_to_delete,
        permastore_threshold: ns.permastore_threshold,
        ..DecayConfig::default()
    }
}

/// Compute overview section from pre-filtered records.
pub fn compute_overview(
    records: &[&(MemoryId, crate::model::record::DiskRecord)],
) -> HealthOverview {
    let mut full = 0u64;
    let mut summary = 0u64;
    let mut ghost = 0u64;
    let mut permastore = 0u64;

    for (_, r) in records {
        match r.phase {
            DecayPhase::Full => full += 1,
            DecayPhase::Summary => summary += 1,
            DecayPhase::Ghost => ghost += 1,
            DecayPhase::Tombstone => {} // tombstones are not counted
        }
        if r.is_permastore != 0 {
            permastore += 1;
        }
    }

    HealthOverview {
        total_memories: records.len() as u64,
        phase_counts: PhaseCounts {
            full,
            summary,
            ghost,
        },
        permastore_count: permastore,
    }
}

/// Compute decay forecast from pre-filtered records.
///
/// Uses the provided namespace registry to look up per-namespace FSRS
/// configuration rather than instantiating calculators directly.
pub fn compute_decay_forecast(
    records: &[&(MemoryId, crate::model::record::DiskRecord)],
    namespaces: &dyn NamespaceRegistry,
) -> DecayForecast {
    let now_millis = chrono::Utc::now().timestamp_millis();
    let mut t7 = TransitionCounts::default();
    let mut t30 = TransitionCounts::default();
    let mut t90 = TransitionCounts::default();

    for (_, record) in records {
        // Skip permastore -- they never decay
        if record.is_permastore != 0 {
            continue;
        }

        // Get namespace config for thresholds
        let ns_config = match namespaces.get_by_id(record.namespace_id) {
            Some(c) => c,
            None => continue,
        };
        let dc = decay_config_for_namespace(&ns_config);
        let engine = FsrsCalculator::new(&dc);

        // Compute elapsed time
        let elapsed_millis = (now_millis - record.last_accessed_at).max(0) as f64;
        let elapsed_days = (elapsed_millis / 86_400_000.0) as f32;

        // Current retrievability for forecasting
        let current_r = engine.retrievability(elapsed_days, record.stability, 1.0);

        // Determine current phase and next threshold
        let current_phase = record.phase;

        let (phase_label, threshold) = match current_phase {
            DecayPhase::Full => (DecayPhase::Full, dc.phase_1_threshold),
            DecayPhase::Summary => (DecayPhase::Summary, dc.phase_2_threshold),
            DecayPhase::Ghost => (DecayPhase::Ghost, dc.phase_3_threshold),
            // Tombstoned memories are terminal — no further transitions.
            DecayPhase::Tombstone => continue,
        };

        if current_r <= threshold {
            // Already below threshold -- will transition on next sweep
            count_transition(phase_label, 0.0, &mut t7, &mut t30, &mut t90);
        } else {
            let days_until = engine.days_until_threshold(record.stability, threshold);
            let remaining = days_until - elapsed_days;
            if remaining > 0.0 {
                count_transition(phase_label, remaining, &mut t7, &mut t30, &mut t90);
            } else {
                count_transition(phase_label, 0.0, &mut t7, &mut t30, &mut t90);
            }
        }
    }

    DecayForecast {
        transitions_7d: t7,
        transitions_30d: t30,
        transitions_90d: t90,
    }
}

/// Increment the appropriate transition counter based on phase and horizon.
fn count_transition(
    phase: DecayPhase,
    days: f32,
    t7: &mut TransitionCounts,
    t30: &mut TransitionCounts,
    t90: &mut TransitionCounts,
) {
    let increment = |tc: &mut TransitionCounts| match phase {
        DecayPhase::Full => tc.full_to_summary += 1,
        DecayPhase::Summary => tc.summary_to_ghost += 1,
        DecayPhase::Ghost => tc.ghost_to_deleted += 1,
        DecayPhase::Tombstone => {} // tombstones don't transition
    };

    if days <= 7.0 {
        increment(t7);
        increment(t30);
        increment(t90);
    } else if days <= 30.0 {
        increment(t30);
        increment(t90);
    } else if days <= 90.0 {
        increment(t90);
    }
}

/// Compute at-risk memories (Ghost phase, closest to deletion).
///
/// Uses the provided namespace registry to look up per-namespace FSRS
/// configuration rather than instantiating calculators directly.
pub fn compute_at_risk(
    records: &[&(MemoryId, crate::model::record::DiskRecord)],
    namespaces: &dyn NamespaceRegistry,
) -> Vec<AtRiskMemory> {
    let now_millis = chrono::Utc::now().timestamp_millis();
    let mut candidates: Vec<AtRiskMemory> = Vec::new();

    for (id, record) in records {
        // Only Ghost phase, non-permastore
        if record.phase != DecayPhase::Ghost || record.is_permastore != 0 {
            continue;
        }

        let ns_config = match namespaces.get_by_id(record.namespace_id) {
            Some(c) => c,
            None => continue,
        };
        let dc = decay_config_for_namespace(&ns_config);
        let engine = FsrsCalculator::new(&dc);

        let elapsed_millis = (now_millis - record.last_accessed_at).max(0) as f64;
        let elapsed_days = (elapsed_millis / 86_400_000.0) as f32;
        let current_r = engine.retrievability(elapsed_days, record.stability, 1.0);

        let days_until_deletion = if current_r <= dc.phase_3_threshold {
            0.0
        } else {
            let days_until = engine.days_until_threshold(record.stability, dc.phase_3_threshold);
            (days_until - elapsed_days).max(0.0)
        };

        let id_str = id.to_string();
        let short_id = if id_str.len() >= 8 {
            id_str[..8].to_string()
        } else {
            id_str.clone()
        };

        candidates.push(AtRiskMemory {
            id: short_id,
            summary: record.summary.chars().take(100).collect(),
            strength: current_r,
            days_until_deletion,
            phase: "ghost".to_string(),
        });
    }

    // Sort by days_until_deletion ascending, take top 10
    candidates.sort_by(|a, b| {
        a.days_until_deletion
            .partial_cmp(&b.days_until_deletion)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    candidates.truncate(10);

    candidates
}

/// Compute age distribution from pre-filtered records.
pub fn compute_age_distribution(
    records: &[&(MemoryId, crate::model::record::DiskRecord)],
) -> AgeDistribution {
    if records.is_empty() {
        return AgeDistribution {
            oldest_created_at: None,
            newest_created_at: None,
            avg_age_days: 0.0,
            median_stability: 0.0,
        };
    }

    let now = chrono::Utc::now().timestamp_millis();

    let mut oldest = i64::MAX;
    let mut newest = i64::MIN;
    let mut total_age_ms = 0i64;
    let mut stabilities: Vec<f32> = Vec::with_capacity(records.len());

    for (_, r) in records {
        if r.created_at < oldest {
            oldest = r.created_at;
        }
        if r.created_at > newest {
            newest = r.created_at;
        }
        total_age_ms += now - r.created_at;
        stabilities.push(r.stability);
    }

    let avg_age_days = (total_age_ms as f64 / records.len() as f64) / 86_400_000.0;

    stabilities.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let median_stability = stabilities[stabilities.len() / 2];

    AgeDistribution {
        oldest_created_at: Some(oldest),
        newest_created_at: Some(newest),
        avg_age_days: avg_age_days as f32,
        median_stability,
    }
}

/// Compute storage breakdown from file sizes on disk.
pub async fn compute_storage_breakdown(
    storage: &dyn StorageEngine,
    namespaces: &dyn NamespaceRegistry,
    namespace_filter: Option<NamespaceId>,
) -> StorageBreakdown {
    let db_path = storage.storage_path();

    let meta_db_bytes = file_size(&db_path.join("meta.db")).unwrap_or(0);
    let edges_db_bytes = file_size(&db_path.join("edges.db")).unwrap_or(0);
    let text_log_bytes = file_size(&db_path.join("fulltext.dat")).unwrap_or(0);

    let ns_list = namespaces.list_all().await;
    let mut vector_files = Vec::new();

    for ns in &ns_list {
        // Apply namespace filter if present
        if let Some(filter_id) = namespace_filter {
            if ns.id != filter_id.get() {
                continue;
            }
        }

        let vector_path = db_path.join("vectors").join(format!("{}.dat", ns.name));
        let bytes = file_size(&vector_path).unwrap_or(0);
        vector_files.push(VectorFileSize {
            namespace: ns.name.clone(),
            bytes,
        });
    }

    let total_bytes = meta_db_bytes
        + edges_db_bytes
        + text_log_bytes
        + vector_files.iter().map(|v| v.bytes).sum::<u64>();

    StorageBreakdown {
        total_bytes,
        meta_db_bytes,
        edges_db_bytes,
        text_log_bytes,
        vector_files,
    }
}

/// Get file size in bytes, returning an io::Error on failure.
fn file_size(path: &FilePath) -> Result<u64, std::io::Error> {
    std::fs::metadata(path).map(|m| m.len())
}

/// Compute metadata/tag statistics.
pub async fn compute_metadata_stats(
    storage: &dyn StorageEngine,
    _namespace_filter: Option<NamespaceId>,
) -> Result<MetadataStats, Box<dyn std::error::Error + Send + Sync>> {
    // list_tags returns all tags sorted by count descending
    let all_tags = storage.list_tags().await?;

    let unique_tags = all_tags.len() as u64;
    let top_tags: Vec<TagCount> = all_tags
        .into_iter()
        .take(10)
        .map(|(tag, count)| TagCount { tag, count })
        .collect();

    Ok(MetadataStats {
        top_tags,
        unique_tags,
    })
}
