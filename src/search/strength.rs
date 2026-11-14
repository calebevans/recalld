//! Operation-local A3 projection. No materialization, policy migration or cache.
use super::*;
use crate::decay::lifecycle::{EffectiveLifecyclePolicy, project, resolve_policy};
use crate::model::schema::{LifecycleVersion, NamespaceSchemaV2};
use crate::storage::control::Generation;

/// Internal observation, not a serving/read-generation certificate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StrengthDiagnostics {
    pub lifecycle_version: LifecycleVersion,
    /// Absent for a physical-v1 namespace: no revision is invented.
    pub policy_revision: Option<Generation>,
    pub evaluated_at: i64,
}

pub(super) struct StrengthContext {
    namespace: NamespaceId,
    schema: Option<NamespaceSchemaV2>,
    policy: Option<EffectiveLifecyclePolicy>,
    pub diagnostics: StrengthDiagnostics,
    bonuses: HashMap<MemoryId, f32>,
}

impl StrengthContext {
    pub fn pin(engine: &QueryEngine, ns: &NamespaceConfig, record_access: bool) -> Result<Self> {
        if let Some(schema) = &ns.schema {
            schema.validate(ns).map_err(SearchError::MetadataError)?;
        }
        let lifecycle_version = ns
            .schema
            .as_ref()
            .map_or(LifecycleVersion::LegacyAccessV1, |s| s.lifecycle_version);
        let explicit = lifecycle_version == LifecycleVersion::ExplicitFeedbackV2;
        if explicit && record_access {
            return Err(SearchError::Internal(
                "explicit-feedback exposure writer unavailable; recordAccess must be false".into(),
            ));
        }
        let policy = explicit
            .then(|| resolve_policy(ns, &engine.decay_policy, engine.decay_multiplier))
            .transpose()
            .map_err(|e| SearchError::MetadataError(e.to_string()))?;
        Ok(Self {
            namespace: ns.id,
            schema: ns.schema.clone(),
            policy,
            diagnostics: StrengthDiagnostics {
                lifecycle_version,
                policy_revision: ns.schema.as_ref().map(|s| s.policy_revision),
                evaluated_at: engine.clock.now_millis(),
            },
            bonuses: HashMap::new(),
        })
    }

    pub fn explicit(&self) -> bool {
        self.policy.is_some()
    }

    pub fn project_namespace(
        &mut self,
        records: &mut [CachedRecord],
        graph: &dyn GraphReader,
    ) -> Result<()> {
        // Project raw donor strength before any query hard filters. Tags and
        // requested result phases do not change lifecycle connection bonuses.
        for record in records.iter_mut() {
            self.apply(record)?;
        }
        if self.explicit() {
            let strengths: HashMap<_, _> = records
                .iter()
                .filter(|r| {
                    r.namespace_id == self.namespace
                        && !matches!(r.phase, DecayPhase::Ghost | DecayPhase::Tombstone)
                })
                .map(|r| (r.id, r.strength))
                .collect();
            self.bonuses = graph.connection_bonuses(&strengths)?;
            for record in records {
                self.apply(record)?;
            }
        }
        Ok(())
    }

    pub fn apply(&self, record: &mut CachedRecord) -> Result<()> {
        // Foreign records are left to the existing scope/path rejection. Never
        // reinterpret another namespace's policy or admit it as a bonus donor.
        if record.namespace_id != self.namespace {
            return Ok(());
        }
        match (&self.schema, &record.lifecycle) {
            (None, None) => {}
            (Some(ns), Some(fields))
                if fields.lifecycle_version == ns.lifecycle_version
                    && fields.policy_revision <= ns.policy_revision => {}
            _ => {
                return Err(SearchError::MetadataError(
                    "record lifecycle/policy does not match pinned namespace".into(),
                ));
            }
        }
        let Some(policy) = &self.policy else {
            return Ok(());
        };
        // Removed records retain structural identity only, never projected relevance.
        if record.phase == DecayPhase::Tombstone {
            return Ok(());
        }
        let fields = record
            .lifecycle
            .as_ref()
            .expect("validated lifecycle fields");
        let projected = project(
            record.stability,
            fields.decay_anchor_at,
            policy,
            self.diagnostics.evaluated_at,
            self.bonuses.get(&record.id).copied().unwrap_or(0.0),
        )
        .map_err(|e| SearchError::MetadataError(e.to_string()))?;
        record.strength = projected.strength;
        record.decay_strength = projected.decay_strength;
        Ok(())
    }
}

impl QueryEngine {
    /// Immutable inputs cloned into each operation; production uses the same
    /// instance clock and global multiplier as A3. Does not activate schema v2.
    pub fn with_lifecycle_inputs(
        mut self,
        clock: Arc<dyn crate::time::Clock>,
        global: crate::decay::DecayConfig,
        multiplier: f64,
    ) -> Self {
        self.clock = clock;
        self.decay_policy = global;
        self.decay_multiplier = multiplier;
        self
    }

    pub(super) async fn strength_record(
        &self,
        id: &MemoryId,
        context: Option<&StrengthContext>,
    ) -> Result<Option<CachedRecord>> {
        let mut record = self.meta_store.get(id).await?;
        if let (Some(context), Some(record)) = (context, &mut record) {
            context.apply(record)?;
        }
        Ok(record)
    }

    pub(super) async fn strength_records(
        &self,
        ids: &[MemoryId],
        context: Option<&StrengthContext>,
    ) -> Result<Vec<CachedRecord>> {
        let mut records = self.meta_store.get_batch(ids).await?;
        if let Some(context) = context {
            for record in &mut records {
                context.apply(record)?;
            }
        }
        Ok(records)
    }
}

pub(super) fn require_legacy_helper(record: &CachedRecord) -> Result<()> {
    if record
        .lifecycle
        .as_ref()
        .is_some_and(|f| f.lifecycle_version == LifecycleVersion::ExplicitFeedbackV2)
    {
        return Err(SearchError::Internal(
            "explicit-feedback direct/similar projection unavailable".into(),
        ));
    }
    Ok(())
}
