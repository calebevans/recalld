//! Configuration system for the Recalld memory engine.
//!
//! Provides layered configuration loading: compiled defaults -> TOML file ->
//! environment variables -> CLI flags, with validation after merge.

pub mod loader;
pub mod types;

pub use loader::{
    CliOverrides, ConfigSource, LoadedConfig, PerDirConfig, generate_default_config, load_config,
};
pub use types::{
    CacheConfig, DecayConfig, EmbeddingConfig, EmbeddingProvider, GraphConfig, LogConfig,
    LogFormat, PhaseThresholds, RifConfig, ServerConfig, StorageConfig,
};

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Errors that can occur during configuration loading or validation.
#[derive(Debug, Error)]
pub enum ConfigError {
    /// The specified config file does not exist.
    #[error("config file not found: {path}")]
    FileNotFound { path: PathBuf },

    /// Failed to read the config file from disk.
    #[error("failed to read config file {path}: {source}")]
    ReadError {
        path: PathBuf,
        source: std::io::Error,
    },

    /// The TOML file contains syntax or type errors.
    #[error("TOML parse error in {path}: {message}")]
    ParseError { path: PathBuf, message: String },

    /// A configuration value failed validation.
    #[error("validation error: {field}: {message}")]
    Validation { field: String, message: String },

    /// An environment variable has a value that cannot be parsed.
    #[error("environment variable {var} has invalid value: {message}")]
    EnvVarInvalid { var: String, message: String },

    /// The filesystem watcher encountered an error.
    #[error("file watcher error: {0}")]
    WatcherError(String),
}

/// Convenience alias for config operations.
pub type Result<T> = std::result::Result<T, ConfigError>;

/// Top-level configuration for Recalld.
///
/// Aggregates all sub-configs. Loading priority (highest wins):
/// 1. CLI flags
/// 2. Environment variables (RECALLD_<SECTION>_<FIELD>)
/// 3. TOML config file (recalld.toml)
/// 4. Compiled-in defaults
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct RecalldConfig {
    /// HTTP server settings.
    pub server: ServerConfig,
    /// Disk storage paths and tuning.
    pub storage: StorageConfig,
    /// FSRS decay engine tuning.
    pub decay: DecayConfig,
    /// RAM cache sizing and eviction.
    pub cache: CacheConfig,
    /// Embedding provider configuration.
    pub embedding: EmbeddingConfig,
    /// Relationship graph tuning.
    pub graph: GraphConfig,
    /// Retrieval-induced forgetting parameters.
    pub rif: RifConfig,
    /// Logging configuration.
    pub log: LogConfig,

    /// Display timezone for formatted timestamps.
    ///
    /// Accepts IANA timezone names (e.g. `"America/New_York"`), `"UTC"`, or
    /// `"local"` (falls back to UTC). Default: `"UTC"`.
    #[serde(default = "default_timezone")]
    pub timezone: String,
}

/// Default timezone value for serde deserialization.
fn default_timezone() -> String {
    "UTC".to_string()
}

impl Default for RecalldConfig {
    fn default() -> Self {
        Self {
            server: ServerConfig::default(),
            storage: StorageConfig::default(),
            decay: DecayConfig::default(),
            cache: CacheConfig::default(),
            embedding: EmbeddingConfig::default(),
            graph: GraphConfig::default(),
            rif: RifConfig::default(),
            log: LogConfig::default(),
            timezone: default_timezone(),
        }
    }
}

impl RecalldConfig {
    /// Validate the fully-resolved configuration.
    ///
    /// Returns all errors found, not just the first, so operators can fix
    /// everything in a single pass.
    pub fn validate(&self) -> std::result::Result<(), Vec<ConfigError>> {
        let mut errors = Vec::new();

        // --- Server ---
        if self.server.port == 0 {
            errors.push(ConfigError::Validation {
                field: "server.port".into(),
                message: "port must be non-zero".into(),
            });
        }
        if self.server.request_timeout_ms == 0 {
            errors.push(ConfigError::Validation {
                field: "server.request_timeout_ms".into(),
                message: "request timeout must be non-zero".into(),
            });
        }
        if self.server.max_body_bytes == 0 {
            errors.push(ConfigError::Validation {
                field: "server.max_body_bytes".into(),
                message: "max body size must be non-zero".into(),
            });
        }

        // --- Storage ---
        if self.storage.data_dir.is_empty() {
            errors.push(ConfigError::Validation {
                field: "storage.data_dir".into(),
                message: "data_dir must not be empty".into(),
            });
        }
        if self.storage.compaction_threshold <= 0.0 || self.storage.compaction_threshold > 1.0 {
            errors.push(ConfigError::Validation {
                field: "storage.compaction_threshold".into(),
                message: "must be in (0.0, 1.0]".into(),
            });
        }
        if self.storage.fsync_interval_ms == 0 {
            errors.push(ConfigError::Validation {
                field: "storage.fsync_interval_ms".into(),
                message: "fsync interval must be non-zero".into(),
            });
        }

        // --- Decay ---
        if self.decay.sweep_interval_hours <= 0.0 {
            errors.push(ConfigError::Validation {
                field: "decay.sweep_interval_hours".into(),
                message: "must be positive".into(),
            });
        }
        let pt = &self.decay.phase_thresholds;
        if pt.full_to_summary <= pt.summary_to_ghost {
            errors.push(ConfigError::Validation {
                field: "decay.phase_thresholds".into(),
                message: format!(
                    "full_to_summary ({}) must be > summary_to_ghost ({})",
                    pt.full_to_summary, pt.summary_to_ghost
                ),
            });
        }
        if pt.summary_to_ghost <= pt.ghost_to_delete {
            errors.push(ConfigError::Validation {
                field: "decay.phase_thresholds".into(),
                message: format!(
                    "summary_to_ghost ({}) must be > ghost_to_delete ({})",
                    pt.summary_to_ghost, pt.ghost_to_delete
                ),
            });
        }
        if pt.ghost_to_delete <= 0.0 || pt.full_to_summary >= 1.0 {
            errors.push(ConfigError::Validation {
                field: "decay.phase_thresholds".into(),
                message: "thresholds must be in (0.0, 1.0)".into(),
            });
        }
        if self.decay.permastore_threshold_days <= 0.0 {
            errors.push(ConfigError::Validation {
                field: "decay.permastore_threshold_days".into(),
                message: "must be positive".into(),
            });
        }
        if self.decay.decay_rate_multiplier < 0.0 {
            errors.push(ConfigError::Validation {
                field: "decay.decay_rate_multiplier".into(),
                message: format!("must be >= 0.0, got {}", self.decay.decay_rate_multiplier),
            });
        }

        // --- Cache ---
        if self.cache.max_capacity_bytes == 0 {
            errors.push(ConfigError::Validation {
                field: "cache.max_capacity_bytes".into(),
                message: "must be non-zero".into(),
            });
        }
        if self.cache.time_to_idle_secs == 0 {
            errors.push(ConfigError::Validation {
                field: "cache.time_to_idle_secs".into(),
                message: "must be non-zero".into(),
            });
        }
        if self.cache.time_to_live_secs == 0 {
            errors.push(ConfigError::Validation {
                field: "cache.time_to_live_secs".into(),
                message: "must be non-zero".into(),
            });
        }
        if self.cache.time_to_idle_secs > self.cache.time_to_live_secs {
            errors.push(ConfigError::Validation {
                field: "cache.time_to_idle_secs".into(),
                message: format!(
                    "time_to_idle ({}) must be <= time_to_live ({})",
                    self.cache.time_to_idle_secs, self.cache.time_to_live_secs
                ),
            });
        }

        // --- Embedding ---
        if self.embedding.model_name.is_empty()
            && self.embedding.provider != EmbeddingProvider::Passthrough
        {
            errors.push(ConfigError::Validation {
                field: "embedding.model_name".into(),
                message: "must not be empty".into(),
            });
        }
        if self.embedding.dimensions == 0 {
            errors.push(ConfigError::Validation {
                field: "embedding.dimensions".into(),
                message: "must be non-zero".into(),
            });
        }
        if self.embedding.dimensions > u16::MAX as usize {
            errors.push(ConfigError::Validation {
                field: "embedding.dimensions".into(),
                message: format!(
                    "must not exceed {} (u16::MAX, per vectors.dat header)",
                    u16::MAX
                ),
            });
        }
        if self.embedding.batch_size == 0 {
            errors.push(ConfigError::Validation {
                field: "embedding.batch_size".into(),
                message: "must be non-zero".into(),
            });
        }
        if self.embedding.base_url.is_empty()
            && self.embedding.provider != EmbeddingProvider::Passthrough
            && self.embedding.provider != EmbeddingProvider::Bedrock
        {
            errors.push(ConfigError::Validation {
                field: "embedding.base_url".into(),
                message: "must not be empty".into(),
            });
        }
        if self.embedding.provider == EmbeddingProvider::Bedrock && self.embedding.region.is_empty()
        {
            errors.push(ConfigError::Validation {
                field: "embedding.region".into(),
                message: "must not be empty when provider is bedrock".into(),
            });
        }

        // --- Graph ---
        if self.graph.max_auto_links == 0 {
            errors.push(ConfigError::Validation {
                field: "graph.max_auto_links".into(),
                message: "must be non-zero".into(),
            });
        }
        if self.graph.auto_link_threshold <= 0.0 || self.graph.auto_link_threshold >= 1.0 {
            errors.push(ConfigError::Validation {
                field: "graph.auto_link_threshold".into(),
                message: "must be in (0.0, 1.0)".into(),
            });
        }
        if self.graph.spreading_activation_s_max <= 0.0 {
            errors.push(ConfigError::Validation {
                field: "graph.spreading_activation_s_max".into(),
                message: "must be positive".into(),
            });
        }
        if self.graph.max_bonus < 0.0 || self.graph.max_bonus > 1.0 {
            errors.push(ConfigError::Validation {
                field: "graph.max_bonus".into(),
                message: "must be in [0.0, 1.0]".into(),
            });
        }

        // --- RIF ---
        if self.rif.enabled {
            if self.rif.max_suppression < 0.0 || self.rif.max_suppression > 1.0 {
                errors.push(ConfigError::Validation {
                    field: "rif.max_suppression".into(),
                    message: "must be in [0.0, 1.0]".into(),
                });
            }
            if self.rif.activation_threshold_low >= self.rif.activation_threshold_high {
                errors.push(ConfigError::Validation {
                    field: "rif.activation_thresholds".into(),
                    message: format!(
                        "low ({}) must be < high ({})",
                        self.rif.activation_threshold_low, self.rif.activation_threshold_high
                    ),
                });
            }
            if self.rif.propagation_depth == 0 {
                errors.push(ConfigError::Validation {
                    field: "rif.propagation_depth".into(),
                    message: "must be >= 1 when RIF is enabled".into(),
                });
            }
        }

        // --- Log ---
        let valid_levels = ["trace", "debug", "info", "warn", "error"];
        if !valid_levels.contains(&self.log.level.to_lowercase().as_str()) {
            errors.push(ConfigError::Validation {
                field: "log.level".into(),
                message: format!(
                    "'{}' is not a valid level (expected one of: {:?})",
                    self.log.level, valid_levels
                ),
            });
        }

        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }
}

/// Subset of RecalldConfig containing only runtime-safe fields.
///
/// These fields can be updated without restarting the server. Unsafe-to-reload
/// fields (server port, data_dir, embedding dimensions) are excluded.
#[derive(Debug, Clone)]
pub struct RuntimeConfig {
    /// Request timeout from ServerConfig.
    pub request_timeout_ms: u64,
    /// Max body bytes from ServerConfig.
    pub max_body_bytes: usize,
    /// Full decay configuration.
    pub decay: DecayConfig,
    /// Cache idle timeout.
    pub cache_time_to_idle_secs: u64,
    /// Cache TTL.
    pub cache_time_to_live_secs: u64,
    /// Full graph configuration.
    pub graph: GraphConfig,
    /// Full RIF configuration.
    pub rif: RifConfig,
    /// Log level.
    pub log_level: String,
    /// Log format.
    pub log_format: LogFormat,
}

impl RuntimeConfig {
    /// Extract the runtime-safe subset from a full config.
    pub fn from_full(config: &RecalldConfig) -> Self {
        Self {
            request_timeout_ms: config.server.request_timeout_ms,
            max_body_bytes: config.server.max_body_bytes,
            decay: config.decay.clone(),
            cache_time_to_idle_secs: config.cache.time_to_idle_secs,
            cache_time_to_live_secs: config.cache.time_to_live_secs,
            graph: config.graph.clone(),
            rif: config.rif.clone(),
            log_level: config.log.level.clone(),
            log_format: config.log.format.clone(),
        }
    }
}
