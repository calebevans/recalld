//! Explicit native lifecycle entry, not an ordinary serving engine. The session
//! only owns existing MetadataStore/ownership primitives; transactions live there.
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::Path;
use std::sync::Arc;

use super::control::{Generation, GenerationRef, MutationContext, WriteReceipt};
use super::ownership::{OwnedFileLock, StoreOwnership, lock_directory};
use super::{MetadataStore, StorageError};
use crate::decay::{
    DecayConfig,
    lifecycle::{EffectiveLifecyclePolicy, resolve_policy},
};
use crate::model::schema::{LifecycleFields, LifecycleVersion};
use crate::model::{
    AccessEvent, AccessKind, DecayPhase, DiskRecord, MemoryId, NamespaceConfig, NamespaceId,
};
use crate::time::Clock;

/// Finite numeric receipt values preserve equality without permitting NaN states.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "f32", into = "f32")]
pub struct FiniteF32(f32);
impl Eq for FiniteF32 {}
impl TryFrom<f32> for FiniteF32 {
    type Error = String;
    fn try_from(value: f32) -> Result<Self, String> {
        if value.is_finite() {
            Ok(Self(value))
        } else {
            Err("nonfinite lifecycle value".into())
        }
    }
}
impl From<FiniteF32> for f32 {
    fn from(value: FiniteF32) -> f32 {
        value.0
    }
}
impl FiniteF32 {
    pub fn get(self) -> f32 {
        self.0
    }
}
fn finite(value: f32) -> Result<FiniteF32, StorageError> {
    value.try_into().map_err(StorageError::InvalidControl)
}

/// Payload-free projection of fields affected by lifecycle operations. Shared
/// LifecycleFields remains the sole canonical lifecycle codec.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LifecycleState {
    pub lifecycle: LifecycleFields,
    pub created_at: i64,
    pub last_accessed_at: i64,
    pub stability: FiniteF32,
    pub difficulty: FiniteF32,
    pub strength: FiniteF32,
    pub decay_strength: FiniteF32,
    pub phase: DecayPhase,
    pub is_permastore: bool,
    pub has_summary: bool,
    pub has_text: bool,
}
impl LifecycleState {
    pub(crate) fn capture(r: &DiskRecord) -> Result<Self, StorageError> {
        Ok(Self {
            lifecycle: r
                .schema
                .as_ref()
                .ok_or_else(|| invalid("native record schema missing"))?
                .lifecycle
                .clone(),
            created_at: r.created_at,
            last_accessed_at: r.last_accessed_at,
            stability: finite(r.stability)?,
            difficulty: finite(r.difficulty)?,
            strength: finite(r.strength)?,
            decay_strength: finite(r.decay_strength)?,
            phase: r.phase,
            is_permastore: r.is_permastore != 0,
            has_summary: !r.summary.is_empty(),
            has_text: r.text_length != 0,
        })
    }
    pub(crate) fn apply(&self, r: &mut DiskRecord) {
        r.schema.as_mut().unwrap().lifecycle = self.lifecycle.clone();
        r.created_at = self.created_at;
        r.last_accessed_at = self.last_accessed_at;
        r.stability = self.stability.get();
        r.difficulty = self.difficulty.get();
        r.strength = self.strength.get();
        r.decay_strength = self.decay_strength.get();
        r.phase = self.phase;
        r.is_permastore = u8::from(self.is_permastore);
    }
    pub(crate) fn eligible(&self) -> bool {
        matches!(self.phase, DecayPhase::Full | DecayPhase::Summary)
            && (self.has_summary || (self.phase == DecayPhase::Full && self.has_text))
    }
    fn validate(&self) -> Result<(), StorageError> {
        let t = &self.lifecycle;
        if self.stability.get() <= 0.
            || !(1. ..=10.).contains(&self.difficulty.get())
            || !(0. ..=1.).contains(&self.strength.get())
            || !(0. ..=1.).contains(&self.decay_strength.get())
        {
            return Err(invalid("invalid native lifecycle numeric state"));
        }
        for time in [
            Some(self.created_at),
            Some(self.last_accessed_at),
            Some(t.observed_at),
            t.retrieved_at,
            t.reinforced_at,
            Some(t.decay_anchor_at),
            Some(t.lifecycle_updated_at),
            Some(t.phase_changed_at),
        ] {
            if time.is_some_and(|v| chrono::DateTime::from_timestamp_millis(v).is_none()) {
                return Err(invalid("unrepresentable native lifecycle time"));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LifecycleChange {
    pub memory_id: MemoryId,
    pub kind: AccessKind,
    pub before: LifecycleState,
    pub after: LifecycleState,
    pub history_before: [u8; 32],
    pub history_after: [u8; 32],
    /// One bounded numeric event allows exact reverse validation of truncation.
    pub evicted_event: Option<AccessEvent>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FeedbackEvidence {
    pub quality: u8,
    pub decay_rate_multiplier: FiniteF32,
    pub permastore_threshold: FiniteF32,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NativeLifecycleWrite {
    pub namespace_id: NamespaceId,
    pub lifecycle_version: LifecycleVersion,
    pub policy_revision: Generation,
    pub previous_generation: Generation,
    /// Actual intent-capture read transaction, not an inferred whole-query view.
    pub capture_generation: GenerationRef,
    pub sampled_at: i64,
    pub records: Vec<LifecycleChange>,
    pub feedback: Option<FeedbackEvidence>,
}

pub(crate) fn invalid(s: &str) -> StorageError {
    StorageError::InvalidControl(s.into())
}
pub(crate) fn history_hash(history: &[AccessEvent]) -> Result<[u8; 32], StorageError> {
    Ok(Sha256::digest(super::control::encode(&history)?).into())
}
pub(crate) fn append_event(r: &mut DiskRecord, time: i64, kind: AccessKind) -> Option<AccessEvent> {
    let evicted = if r.access_history.len() >= crate::model::constants::ACCESS_HISTORY_MAX {
        Some(r.access_history.remove(0))
    } else {
        None
    };
    r.access_history.push(AccessEvent {
        timestamp: time,
        kind,
    });
    evicted
}
fn compatibility_time(state: &LifecycleState) -> i64 {
    state
        .created_at
        .max(state.lifecycle.retrieved_at.unwrap_or(i64::MIN))
        .max(state.lifecycle.reinforced_at.unwrap_or(i64::MIN))
}
pub(crate) fn exposed(before: &LifecycleState, now: i64) -> Result<LifecycleState, StorageError> {
    before.validate()?;
    if !before.eligible() {
        return Err(StorageError::LifecycleContentUnavailable);
    }
    let mut result = before.clone();
    let time = now.max(before.lifecycle.lifecycle_updated_at);
    result.lifecycle.retrieved_at = Some(time);
    result.lifecycle.lifecycle_updated_at = time;
    if result.lifecycle.lifecycle_version == LifecycleVersion::LegacyAccessV1 {
        result.lifecycle.decay_anchor_at = time;
        result.last_accessed_at = time;
    } else {
        result.last_accessed_at = compatibility_time(&result);
    }
    result.lifecycle.record_generation = result.lifecycle.record_generation.next()?;
    Ok(result)
}
pub(crate) fn reinforced(
    before: &LifecycleState,
    now: i64,
    evidence: &FeedbackEvidence,
) -> Result<LifecycleState, StorageError> {
    before.validate()?;
    if !before.eligible() {
        return Err(StorageError::LifecycleContentUnavailable);
    }
    let config = DecayConfig {
        permastore_threshold: evidence.permastore_threshold.get(),
        ..Default::default()
    };
    let policy = EffectiveLifecyclePolicy {
        config,
        decay_rate_multiplier: evidence.decay_rate_multiplier.get(),
    };
    let input = crate::decay::lifecycle::FeedbackState {
        stability: before.stability.get(),
        difficulty: before.difficulty.get(),
        phase: before.phase,
        is_permastore: before.is_permastore,
        has_summary: before.has_summary,
        anchor: before.lifecycle.decay_anchor_at,
        updated_at: before.lifecycle.lifecycle_updated_at,
    };
    let plan = crate::decay::lifecycle::plan_feedback_state(&input, &policy, now, evidence.quality)
        .map_err(|e| StorageError::InvalidMutation(e.to_string()))?;
    let mut result = before.clone();
    result.stability = finite(plan.stability)?;
    result.strength = finite(plan.strength)?;
    result.decay_strength = finite(plan.strength)?;
    result.is_permastore = plan.is_permastore;
    result.lifecycle.reinforced_at = Some(plan.timestamp);
    result.lifecycle.decay_anchor_at = plan.timestamp;
    result.lifecycle.lifecycle_updated_at = plan.timestamp;
    result.lifecycle.record_generation = result.lifecycle.record_generation.next()?;
    result.last_accessed_at =
        if result.lifecycle.lifecycle_version == LifecycleVersion::LegacyAccessV1 {
            plan.timestamp
        } else {
            compatibility_time(&result)
        };
    Ok(result)
}
impl NativeLifecycleWrite {
    pub(crate) fn validate(
        &self,
        receipt: &WriteReceipt,
        feedback: bool,
    ) -> Result<(), StorageError> {
        if self.namespace_id.is_unset()
            || self.records.is_empty()
            || self.capture_generation.store_id != receipt.store_id
            || self.capture_generation.generation > self.previous_generation
            || self.previous_generation.next()? != receipt.committed_generation
            || chrono::DateTime::from_timestamp_millis(self.sampled_at).is_none()
            || self.feedback.is_some() != feedback
            || (feedback && self.records.len() != 1)
        {
            return Err(invalid("invalid native lifecycle receipt boundary"));
        }
        let mut seen = std::collections::HashSet::new();
        for step in &self.records {
            if !seen.insert(step.memory_id)
                || step.before.lifecycle.lifecycle_version != self.lifecycle_version
                || step.before.lifecycle.policy_revision != self.policy_revision
            {
                return Err(invalid("invalid native lifecycle receipt scope/policy"));
            }
            let expected = if let Some(evidence) = &self.feedback {
                if step.kind != AccessKind::ManualReinforcement {
                    return Err(invalid("invalid feedback history kind"));
                }
                reinforced(&step.before, self.sampled_at, evidence)?
            } else {
                if !matches!(
                    step.kind,
                    AccessKind::DirectRetrieval | AccessKind::AssociativeRetrieval
                ) {
                    return Err(invalid("invalid exposure history kind"));
                }
                exposed(&step.before, self.sampled_at)?
            };
            if expected != step.after {
                return Err(invalid("native lifecycle transition mismatch"));
            }
            if step
                .evicted_event
                .is_some_and(|e| chrono::DateTime::from_timestamp_millis(e.timestamp).is_none())
            {
                return Err(invalid("invalid evicted history timestamp"));
            }
        }
        Ok(())
    }
    pub(crate) fn rewind(&self, rows: &mut [DiskRecord]) -> Result<(), StorageError> {
        for step in &self.records {
            let r = rows
                .iter_mut()
                .find(|r| r.id == *step.memory_id.as_bytes())
                .ok_or_else(|| invalid("native receipt record absent"))?;
            if LifecycleState::capture(r)? != step.after
                || history_hash(&r.access_history)? != step.history_after
            {
                return Err(invalid("native receipt does not match record state"));
            }
            if r.access_history.pop()
                != Some(AccessEvent {
                    timestamp: step.after.lifecycle.lifecycle_updated_at,
                    kind: step.kind,
                })
            {
                return Err(invalid("native receipt history event missing"));
            }
            if let Some(event) = step.evicted_event {
                if r.access_history.len() + 1 != crate::model::constants::ACCESS_HISTORY_MAX {
                    return Err(invalid("invalid native history eviction"));
                }
                r.access_history.insert(0, event);
            }
            if history_hash(&r.access_history)? != step.history_before {
                return Err(invalid("native receipt prior history mismatch"));
            }
            step.before.apply(r);
        }
        Ok(())
    }
}

/// The configuration is frozen for a session. Capture binds resolved inputs by
/// hash; a new session's changed configuration cannot use old preconditions.
#[derive(Clone)]
pub struct NativePolicyInputs {
    pub global: DecayConfig,
    pub multiplier: f64,
}
impl Default for NativePolicyInputs {
    fn default() -> Self {
        Self {
            global: DecayConfig::default(),
            multiplier: 1.,
        }
    }
}
impl NativePolicyInputs {
    pub(crate) fn resolve(
        &self,
        ns: &NamespaceConfig,
    ) -> Result<EffectiveLifecyclePolicy, StorageError> {
        resolve_policy(ns, &self.global, self.multiplier)
            .map_err(|e| StorageError::InvalidMutation(e.to_string()))
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LifecycleScope {
    pub namespace_id: NamespaceId,
    pub namespace_name: String,
    pub source: Option<MemoryId>,
}
#[derive(Debug, Clone)]
/// Verified internal explicit-ID intent. Selected search IDs are not stable
/// caller query intent; this type does not implement public keyed query replay.
pub struct ExposureIntent {
    pub(crate) scope: LifecycleScope,
    pub(crate) original: Vec<(MemoryId, AccessKind)>,
    pub(crate) capture: CapturedLifecycle,
}
#[derive(Debug, Clone)]
pub struct FeedbackIntent {
    pub(crate) scope: LifecycleScope,
    pub(crate) id: MemoryId,
    pub(crate) quality: u8,
    pub(crate) capture: CapturedLifecycle,
}
#[derive(Debug, Clone)]
pub(crate) struct CapturedLifecycle {
    pub generation: GenerationRef,
    pub version: LifecycleVersion,
    pub revision: Generation,
    pub policy_hash: [u8; 32],
    pub records: Vec<(MemoryId, Generation)>,
}
impl ExposureIntent {
    pub fn capture_generation(&self) -> GenerationRef {
        self.capture.generation
    }
}
impl FeedbackIntent {
    pub fn capture_generation(&self) -> GenerationRef {
        self.capture.generation
    }
}

pub struct NativeLifecycleSession {
    pub(crate) metadata: MetadataStore,
    policy: NativePolicyInputs,
    clock: Arc<dyn Clock>,
    _legacy_lock: OwnedFileLock,
    _owner: StoreOwnership,
}
impl NativeLifecycleSession {
    /// Reconcile an uncertain commit after reopening this same origin. This is
    /// canonical receipt lookup, not a derived publication/readiness assertion.
    pub fn lookup_operation(
        &self,
        store_id: uuid::Uuid,
        namespace_id: NamespaceId,
        operation_id: uuid::Uuid,
    ) -> Result<Option<WriteReceipt>, StorageError> {
        self.metadata.lookup_operation(
            store_id,
            super::control::MutationScope::Namespace(namespace_id),
            operation_id,
        )
    }
    pub fn open(
        path: &Path,
        policy: NativePolicyInputs,
        clock: Arc<dyn Clock>,
    ) -> Result<Self, StorageError> {
        if !path.is_dir() {
            return Err(StorageError::InvalidPath);
        }
        let owner = StoreOwnership::acquire(path)?;
        let lock = lock_directory(&owner.path)?;
        super::validation::canonical_files(&owner.path)?;
        let metadata = MetadataStore::open_native_lifecycle(&owner.path.join("meta.db"))?;
        Ok(Self {
            metadata,
            policy,
            clock,
            _legacy_lock: lock,
            _owner: owner,
        })
    }
    pub fn capture_exposure(
        &self,
        scope: LifecycleScope,
        original: &[(MemoryId, AccessKind)],
    ) -> Result<ExposureIntent, StorageError> {
        self.metadata
            .capture_exposure(scope, original, &self.policy)
    }
    pub fn record_exposure(
        &self,
        ctx: &MutationContext,
        intent: &ExposureIntent,
    ) -> Result<Option<WriteReceipt>, StorageError> {
        self.metadata
            .record_native_exposure(ctx, intent, &self.policy, self.clock.as_ref())
    }
    pub fn capture_feedback(
        &self,
        id: MemoryId,
        namespace: Option<NamespaceId>,
        quality: u8,
    ) -> Result<FeedbackIntent, StorageError> {
        self.metadata
            .capture_feedback(id, namespace, quality, &self.policy)
    }
    pub fn apply_feedback(
        &self,
        ctx: &MutationContext,
        intent: &FeedbackIntent,
    ) -> Result<WriteReceipt, StorageError> {
        self.metadata
            .apply_native_feedback(ctx, intent, &self.policy, self.clock.as_ref())
    }
}
