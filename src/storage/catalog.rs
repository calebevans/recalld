//! Legacy catalog participation, not ordinary canonical-v2 serving.
use super::StorageError;
use crate::model::{NamespaceConfig, namespace::PhaseThresholds};
use serde::{Deserialize, Serialize};

/// A complete replacement of mutable legacy policy fields. Identity, directory,
/// dimensions, creation time, lifecycle version and embedding profile are absent.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NamespacePolicy {
    pub initial_stability: f32,
    pub default_difficulty: f32,
    pub phase_thresholds: PhaseThresholds,
    pub permastore_threshold: f32,
    pub desired_retention: f32,
    pub decay_rate_multiplier: Option<f32>,
}

impl From<&NamespaceConfig> for NamespacePolicy {
    fn from(c: &NamespaceConfig) -> Self {
        Self {
            initial_stability: c.initial_stability,
            default_difficulty: c.default_difficulty,
            phase_thresholds: c.phase_thresholds,
            permastore_threshold: c.permastore_threshold,
            desired_retention: c.desired_retention,
            decay_rate_multiplier: c.decay_rate_multiplier,
        }
    }
}
impl NamespacePolicy {
    pub(crate) fn apply(&self, c: &mut NamespaceConfig) {
        c.initial_stability = self.initial_stability;
        c.default_difficulty = self.default_difficulty;
        c.phase_thresholds = self.phase_thresholds;
        c.permastore_threshold = self.permastore_threshold;
        c.desired_retention = self.desired_retention;
        c.decay_rate_multiplier = self.decay_rate_multiplier;
    }
    pub(crate) fn validate(&self) -> Result<(), StorageError> {
        let mut config = NamespaceConfig::default_namespace(0);
        self.apply(&mut config);
        crate::decay::lifecycle::resolve_policy(
            &config,
            &crate::decay::DecayConfig::default(),
            1.0,
        )
        .map_err(|e| StorageError::InvalidMutation(e.to_string()))?;
        if !self.desired_retention.is_finite() || !(0.0..=1.0).contains(&self.desired_retention) {
            return Err(StorageError::InvalidMutation(
                "desired retention must be finite and in [0,1]".into(),
            ));
        }
        Ok(())
    }
}
pub(crate) fn validate_create(config: &NamespaceConfig) -> Result<(), StorageError> {
    crate::model::validation::validate_namespace_name(&config.name).map_err(|e| {
        StorageError::InvalidNamespaceName {
            name: config.name.clone(),
            reason: e.to_string(),
        }
    })?;
    if !(1..=8192).contains(&config.embedding_dim)
        || config.schema.is_some()
        || chrono::DateTime::from_timestamp_millis(config.created_at).is_none()
    {
        return Err(StorageError::InvalidMutation(
            "invalid namespace dimension, creation time or unsupported schema".into(),
        ));
    }
    NamespacePolicy::from(config).validate()
}

// Process exits and injected I/O errors are confined to synthetic tests.
#[cfg(test)]
thread_local! { pub(crate) static FAILURE: std::cell::Cell<Option<&'static str>> = const { std::cell::Cell::new(None) }; }
#[cfg(test)]
pub(crate) fn injected(point: &str) -> bool {
    FAILURE.with(|f| f.get() == Some(point))
}
pub(crate) fn boundary(_point: &str) -> Result<(), StorageError> {
    #[cfg(test)]
    {
        if std::env::var("RECALLD_CATALOG_CRASH").as_deref() == Ok(_point) {
            std::process::exit(89);
        }
        if injected(_point) {
            return Err(std::io::Error::other(format!("injected catalog fault: {_point}")).into());
        }
    }
    Ok(())
}
