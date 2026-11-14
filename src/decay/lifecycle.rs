//! Pure legacy lifecycle planning against the current record layout.
//!
//! `last_accessed_at` remains the v1 anchor. No inferred feedback dates,
//! lifecycle v2 policy, durable receipts, or schema changes are introduced here.

use super::{DecayConfig, FsrsEngine};
use crate::model::{DecayPhase, DiskRecord, NamespaceConfig};

#[derive(Debug, thiserror::Error)]
pub enum LifecycleError {
    /// An explicit namespace assertion failed before feedback effects.
    #[error("{0}")]
    NamespaceMismatch(String),

    #[error("invalid lifecycle input: {0}")]
    InvalidInput(&'static str),
    #[error("memory content is unavailable for feedback")]
    ContentUnavailable,
    #[error("memory or namespace not found")]
    NotFound,
    #[error("lifecycle storage error: {0}")]
    Storage(String),
}

/// Owned snapshot of resolved namespace settings for one operation.
#[derive(Debug, Clone)]
pub struct EffectiveLifecyclePolicy {
    pub config: DecayConfig,
    pub decay_rate_multiplier: f32,
}

pub fn resolve_policy(
    namespace: &NamespaceConfig,
    global: &DecayConfig,
    global_multiplier: f64,
) -> Result<EffectiveLifecyclePolicy, LifecycleError> {
    let config = DecayConfig {
        initial_stability: namespace.initial_stability,
        difficulty: namespace.default_difficulty,
        phase_1_threshold: namespace.phase_thresholds.full_to_summary,
        phase_2_threshold: namespace.phase_thresholds.summary_to_ghost,
        phase_3_threshold: namespace.phase_thresholds.ghost_to_delete,
        permastore_threshold: namespace.permastore_threshold,
        ..global.clone()
    };
    let multiplier = namespace
        .decay_rate_multiplier
        .map(f64::from)
        .unwrap_or(global_multiplier);
    let policy = EffectiveLifecyclePolicy {
        config,
        decay_rate_multiplier: multiplier as f32,
    };
    if multiplier != 0.0 && policy.decay_rate_multiplier == 0.0 {
        return Err(LifecycleError::InvalidInput(
            "decay multiplier underflows f32",
        ));
    }
    policy.validate()?;
    Ok(policy)
}

impl EffectiveLifecyclePolicy {
    fn validate(&self) -> Result<(), LifecycleError> {
        let c = &self.config;
        let values = [
            c.initial_stability,
            c.difficulty,
            c.phase_1_threshold,
            c.phase_2_threshold,
            c.phase_3_threshold,
            c.permastore_threshold,
            c.max_connection_bonus,
            c.partial_access_weight,
            self.decay_rate_multiplier,
        ];
        if values.iter().any(|v| !v.is_finite())
            || c.initial_stability <= 0.0
            || !(1.0..=10.0).contains(&c.difficulty)
            || !(0.0 < c.phase_3_threshold
                && c.phase_3_threshold < c.phase_2_threshold
                && c.phase_2_threshold < c.phase_1_threshold
                && c.phase_1_threshold < 1.0)
            || c.permastore_threshold <= 0.0
            || !(0.0..=1.0).contains(&c.max_connection_bonus)
            || !(0.0..=1.0).contains(&c.partial_access_weight)
            || self.decay_rate_multiplier < 0.0
        {
            return Err(LifecycleError::InvalidInput("invalid decay policy"));
        }
        Ok(())
    }
}

/// Current-clock diagnostic/sweep strength; legacy retrieval keeps using its
/// materialized strength. Projection never changes timestamps or retained phase.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LifecycleProjection {
    pub strength: f32,
    pub decay_strength: f32,
    pub evaluated_at: i64,
}

pub fn project(
    stability: f32,
    decay_anchor_at: i64,
    policy: &EffectiveLifecyclePolicy,
    now_millis: i64,
    connection_bonus: f32,
) -> Result<LifecycleProjection, LifecycleError> {
    policy.validate()?;
    if !stability.is_finite()
        || stability <= 0.0
        || !connection_bonus.is_finite()
        || !(0.0..=1.0).contains(&connection_bonus)
    {
        return Err(LifecycleError::InvalidInput(
            "invalid stability or connection bonus",
        ));
    }
    let engine = FsrsEngine::new(&policy.config);
    let strength = engine.retrievability(
        crate::time::elapsed_days(now_millis, decay_anchor_at),
        stability,
        policy.decay_rate_multiplier,
    );
    Ok(LifecycleProjection {
        strength,
        decay_strength: engine.effective_retrievability(
            strength,
            connection_bonus.min(policy.config.max_connection_bonus),
        ),
        evaluated_at: now_millis,
    })
}

/// Update-only feedback result. Retained phase never follows refreshed strength.
#[derive(Debug, Clone, PartialEq)]
pub struct FeedbackPlan {
    pub timestamp: i64,
    pub stability: f32,
    pub strength: f32,
    pub phase: DecayPhase,
    pub is_permastore: bool,
}

pub fn plan_feedback(
    record: &DiskRecord,
    policy: &EffectiveLifecyclePolicy,
    now_millis: i64,
    quality: u8,
) -> Result<FeedbackPlan, LifecycleError> {
    if !(1..=4).contains(&quality) {
        return Err(LifecycleError::InvalidInput("quality must be in 1..=4"));
    }
    if matches!(record.phase, DecayPhase::Ghost | DecayPhase::Tombstone)
        || (record.phase == DecayPhase::Summary && record.summary.is_empty())
    {
        return Err(LifecycleError::ContentUnavailable);
    }
    policy.validate()?;
    if !record.stability.is_finite()
        || record.stability <= 0.0
        || !record.difficulty.is_finite()
        || !(1.0..=10.0).contains(&record.difficulty)
    {
        return Err(LifecycleError::InvalidInput(
            "invalid record stability or difficulty",
        ));
    }
    // Until A1 persists lifecycle_updated_at, these are the known v1 times.
    let timestamp = now_millis
        .max(record.last_accessed_at)
        .max(record.created_at);
    let config = DecayConfig {
        difficulty: record.difficulty,
        ..policy.config.clone()
    };
    let stability = FsrsEngine::new(&config).review_stability(
        record.stability,
        crate::time::elapsed_days(timestamp, record.last_accessed_at),
        quality,
        policy.decay_rate_multiplier,
    );
    Ok(FeedbackPlan {
        timestamp,
        stability,
        strength: 1.0,
        phase: record.phase,
        is_permastore: record.is_permastore != 0 || stability >= config.permastore_threshold,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::MemoryId;
    use crate::storage::StorageEngine;
    use crate::test_support::{Fixture, MemorySpec};
    use crate::time::{Clock, ManualClock};
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    const DAY: i64 = 86_400_000;

    #[test]
    fn projection_is_deterministic_bounded_and_uses_the_legacy_multiplier_direction() {
        let mut ns = NamespaceConfig::default_namespace(0);
        let global = DecayConfig {
            max_connection_bonus: 0.1,
            ..DecayConfig::default()
        };
        let mut strengths = Vec::new();
        for multiplier in [0.0, 1.0, 2.0] {
            ns.decay_rate_multiplier = Some(multiplier);
            let policy = resolve_policy(&ns, &global, 10.0).unwrap();
            let p = project(3.7145, 0, &policy, 8 * DAY, 0.5).unwrap();
            assert_eq!(p, project(3.7145, 0, &policy, 8 * DAY, 0.5).unwrap());
            assert_eq!(p.decay_strength, p.strength + 0.1 * (1.0 - p.strength));
            strengths.push(p.strength);
            let jump = project(3.7145, i64::MIN, &policy, i64::MAX, 0.0).unwrap();
            assert!((0.0..=1.0).contains(&jump.strength));
            assert_eq!(project(3.7145, DAY, &policy, 0, 0.0).unwrap().strength, 1.0);
            assert!(project(3.7145, 0, &policy, DAY, f32::NAN).is_err());
        }
        assert_eq!(strengths[0], 1.0);
        assert!(strengths[1] < strengths[2] && strengths[2] < strengths[0]);
    }

    fn record() -> DiskRecord {
        DiskRecord {
            version: DiskRecord::CURRENT_VERSION,
            id: [0; 16],
            namespace_id: 1,
            created_at: 0,
            last_accessed_at: 0,
            phase: DecayPhase::Full,
            strength: 1.0,
            decay_strength: 1.0,
            stability: 3.7145,
            difficulty: 8.0,
            is_permastore: 0,
            vector_slot: 0,
            edge_count: 0,
            summary: "retained".into(),
            tags: vec![],
            access_history: vec![],
            text_offset: 0,
            text_length: 0,
        }
    }

    #[test]
    fn policy_inheritance_multiplier_direction_and_record_difficulty() {
        let mut ns = NamespaceConfig::default_namespace(0);
        ns.permastore_threshold = 10.0;
        ns.default_difficulty = 2.0;
        let inherited = resolve_policy(&ns, &DecayConfig::default(), 2.0).unwrap();
        assert_eq!(inherited.decay_rate_multiplier, 2.0);
        let r = record();
        let expected_config = DecayConfig {
            difficulty: r.difficulty,
            ..DecayConfig::default()
        };
        let expected = FsrsEngine::new(&expected_config).review_stability(r.stability, 8.0, 3, 2.0);
        let plan = plan_feedback(&r, &inherited, 8 * DAY, 3).unwrap();
        assert_eq!(plan.stability, expected);
        assert_eq!(plan.is_permastore, expected >= 10.0);
        let mut stability = Vec::new();
        for multiplier in [0.0, 1.0, 2.0] {
            ns.decay_rate_multiplier = Some(multiplier);
            let policy = resolve_policy(&ns, &DecayConfig::default(), 9.0).unwrap();
            stability.push(plan_feedback(&r, &policy, 8 * DAY, 3).unwrap().stability);
        }
        assert_eq!(stability[0], r.stability);
        assert!(stability[1] > stability[2] && stability[2] > stability[0]);
    }

    #[test]
    fn feedback_keeps_retained_phase_and_sticky_permastore() {
        let mut ns = NamespaceConfig::default_namespace(0);
        ns.permastore_threshold = 0.01;
        let policy = resolve_policy(&ns, &DecayConfig::default(), 1.0).unwrap();
        let mut r = record();
        r.phase = DecayPhase::Summary;
        let before = r.to_bytes();
        let p = plan_feedback(&r, &policy, 8 * DAY, 4).unwrap();
        assert_eq!(p.phase, DecayPhase::Summary);
        assert_eq!(p.strength, 1.0);
        assert!(p.is_permastore);
        assert_eq!(before, r.to_bytes(), "planning cannot change content");
        r.is_permastore = 1;
        ns.permastore_threshold = 36500.0;
        let policy = resolve_policy(&ns, &DecayConfig::default(), 1.0).unwrap();
        let lapse = plan_feedback(&r, &policy, 8 * DAY, 1).unwrap();
        assert!(lapse.stability < r.stability);
        assert!(lapse.is_permastore);
        for phase in [DecayPhase::Ghost, DecayPhase::Tombstone] {
            r.phase = phase;
            assert!(matches!(
                plan_feedback(&r, &policy, 8 * DAY, 3),
                Err(LifecycleError::ContentUnavailable)
            ));
        }
        r.phase = DecayPhase::Summary;
        r.summary.clear();
        assert!(matches!(
            plan_feedback(&r, &policy, 8 * DAY, 3),
            Err(LifecycleError::ContentUnavailable)
        ));
    }

    #[test]
    fn invalid_policy_and_feedback_are_rejected_without_mutation() {
        let mut ns = NamespaceConfig::default_namespace(0);
        for invalid in [f32::NAN, f32::INFINITY, -1.0] {
            ns.decay_rate_multiplier = Some(invalid);
            assert!(resolve_policy(&ns, &DecayConfig::default(), 1.0).is_err());
        }
        ns.decay_rate_multiplier = None;
        assert!(resolve_policy(&ns, &DecayConfig::default(), f64::MAX).is_err());
        assert!(resolve_policy(&ns, &DecayConfig::default(), f64::MIN_POSITIVE).is_err());
        ns.phase_thresholds.full_to_summary = f32::NAN;
        assert!(resolve_policy(&ns, &DecayConfig::default(), 1.0).is_err());
        ns.phase_thresholds = Default::default();
        ns.phase_thresholds.ghost_to_delete = ns.phase_thresholds.summary_to_ghost;
        assert!(resolve_policy(&ns, &DecayConfig::default(), 1.0).is_err());
        let policy = resolve_policy(
            &NamespaceConfig::default_namespace(0),
            &DecayConfig::default(),
            1.0,
        )
        .unwrap();
        let mut r = record();
        for quality in [0, 5, 255] {
            assert!(plan_feedback(&r, &policy, DAY, quality).is_err());
        }
        for invalid in [f32::NAN, f32::INFINITY, 0.0, -1.0] {
            r.stability = invalid;
            assert!(plan_feedback(&r, &policy, DAY, 3).is_err());
        }
    }

    #[test]
    fn clock_regression_and_large_jump_keep_feedback_finite() {
        let policy = resolve_policy(
            &NamespaceConfig::default_namespace(0),
            &DecayConfig::default(),
            1.0,
        )
        .unwrap();
        let mut r = record();
        r.created_at = 2 * DAY;
        r.last_accessed_at = 3 * DAY;
        let p = plan_feedback(&r, &policy, DAY, 3).unwrap();
        assert_eq!(p.timestamp, 3 * DAY);
        assert_eq!(p.stability, r.stability);
        r.last_accessed_at = i64::MIN;
        for quality in 1..=4 {
            let p = plan_feedback(&r, &policy, i64::MAX, quality).unwrap();
            assert!(p.stability.is_finite() && p.stability > 0.0);
        }
    }

    struct CountingClock {
        now: i64,
        calls: AtomicUsize,
    }
    impl Clock for CountingClock {
        fn now_millis(&self) -> i64 {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.now
        }
    }

    #[tokio::test]
    async fn mcp_and_http_feedback_share_policy_clock_and_terminal_guards() {
        use crate::api::FsrsEngine as _;
        use crate::mcp::bridge::StorageEngine as _;
        let f = Fixture::new();
        let mut ns = NamespaceConfig::default_namespace(0);
        ns.name = "feedback".into();
        ns.embedding_dim = crate::test_support::DIM as u32;
        ns.decay_rate_multiplier = Some(2.0);
        ns.permastore_threshold = 0.01;
        let namespace = f.storage.write().unwrap().create_namespace(&ns).unwrap();
        let a = f
            .insert_memory_with(MemorySpec {
                phase: DecayPhase::Summary,
                ..MemorySpec::new(namespace)
            })
            .await;
        let b = f
            .insert_memory_with(MemorySpec {
                phase: DecayPhase::Summary,
                ..MemorySpec::new(namespace)
            })
            .await;
        let clock = Arc::new(CountingClock {
            now: 8 * DAY,
            calls: AtomicUsize::new(0),
        });
        let mcp = f.storage_adapter().with_clock(clock.clone());
        let mut config = crate::config::RecalldConfig::default();
        config.decay.decay_rate_multiplier = 2.0;
        let http = crate::api::adapters::FsrsEngineAdapter::new(
            f.storage.clone(),
            f.cache.clone(),
            f.graph.clone(),
            Arc::new(config),
            false,
        )
        .with_clock(clock.clone());
        let expected = plan_feedback(
            &f.record(a).unwrap(),
            &resolve_policy(&ns, &DecayConfig::default(), 1.0).unwrap(),
            8 * DAY,
            3,
        )
        .unwrap();
        let ma = mcp.reinforce_memory(a, 3, None).await.unwrap();
        let hb = http.reinforce(b, 3, None).await.unwrap();
        assert_eq!(
            clock.calls.load(Ordering::SeqCst),
            2,
            "one clock sample per operation"
        );
        assert_eq!(ma.stability, expected.stability);
        assert_eq!(ma.stability, hb.stability);
        // HTTP must inherit global multiplier 2 when the namespace has no override.
        let inherited = f.insert_memory().await;
        let policy = resolve_policy(
            &NamespaceConfig::default_namespace(0),
            &DecayConfig::default(),
            2.0,
        )
        .unwrap();
        let expected = plan_feedback(&f.record(inherited).unwrap(), &policy, 8 * DAY, 3).unwrap();
        assert_eq!(
            http.reinforce(inherited, 3, None).await.unwrap().stability,
            expected.stability
        );
        for id in [a, b] {
            let r = f.record(id).unwrap();
            assert_eq!(r.last_accessed_at, 8 * DAY);
            assert_eq!(r.access_history.len(), 1);
            assert_eq!(r.phase, DecayPhase::Summary);
            assert_eq!(r.text_length, 0);
            assert_eq!(r.is_permastore, 1);
            let before = r.to_bytes();
            assert!(mcp.reinforce_memory(id, 0, None).await.is_err());
            assert!(http.reinforce(id, 5, None).await.is_err());
            assert_eq!(before, f.record(id).unwrap().to_bytes());
        }
        // A backward production clock cannot move the existing anchor backward.
        let regressed = f
            .storage_adapter()
            .with_clock(Arc::new(ManualClock::new(DAY)));
        regressed.reinforce_memory(a, 1, None).await.unwrap();
        assert_eq!(f.record(a).unwrap().last_accessed_at, 8 * DAY);
        let ghost = f
            .insert_memory_with(MemorySpec {
                phase: DecayPhase::Ghost,
                summary: "",
                ..MemorySpec::new(namespace)
            })
            .await;
        assert!(mcp.reinforce_memory(ghost, 3, None).await.is_err());
        assert!(http.reinforce(ghost, 3, None).await.is_err());
        assert!(f.record(ghost).unwrap().access_history.is_empty());
        mcp.delete_memory(a, None).await.unwrap();
        let tombstone = f.record(a).unwrap().to_bytes();
        assert!(mcp.reinforce_memory(a, 3, None).await.is_err());
        assert!(http.reinforce(a, 3, None).await.is_err());
        assert_eq!(tombstone, f.record(a).unwrap().to_bytes());
        f.storage.write().unwrap().delete_memory(b).unwrap();
        assert!(http.reinforce(b, 3, None).await.is_err());
        assert!(f.record(b).is_none());
        assert!(
            mcp.reinforce_memory(MemoryId::new(), 3, None)
                .await
                .is_err()
        );
    }
}
