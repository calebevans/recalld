//! Configuration loader: TOML parsing, environment variable overlays, CLI flag overrides.
//!
//! Implements the layered configuration loading strategy:
//! 1. Compiled-in defaults
//! 2. TOML config file
//! 3. Environment variable overrides
//! 4. CLI flag overrides
//! 5. Validation

use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::config::types::{
    CacheConfig, DecayConfig, EmbeddingConfig, EmbeddingProvider, GraphConfig, LogConfig,
    LogFormat, RifConfig, ServerConfig, StorageConfig,
};
use crate::config::{ConfigError, RecalldConfig};

/// Identifies where a config value originated (for diagnostics).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigSource {
    /// Compiled-in default.
    Default,
    /// Parsed from the TOML config file.
    File(PathBuf),
    /// Overridden by an environment variable.
    EnvVar(String),
    /// Overridden by a CLI flag.
    CliFlag(String),
}

/// CLI flags that can override configuration.
#[derive(Debug, Default)]
pub struct CliOverrides {
    /// Explicit config file path from `--config` flag.
    pub config_path: Option<PathBuf>,
    /// Server port from `--port` flag.
    pub port: Option<u16>,
    /// Bind address from `--bind` flag.
    pub bind_address: Option<String>,
    /// Data directory from `--data-dir` flag.
    pub data_dir: Option<String>,
    /// Log level from `--log-level` flag.
    pub log_level: Option<String>,
    /// Log format from `--log-format` flag.
    pub log_format: Option<String>,
}

/// Per-directory configuration loaded from `.recalld.toml` in a project root.
///
/// Contains a required `namespace` field and optional config section overrides.
/// When present, config sections replace the corresponding global config section
/// entirely (section-level granularity).
#[derive(Debug, Deserialize)]
pub struct PerDirConfig {
    /// Required: default namespace for this directory.
    pub namespace: String,
    /// Optional server config override.
    #[serde(default)]
    pub server: Option<ServerConfig>,
    /// Optional storage config override.
    #[serde(default)]
    pub storage: Option<StorageConfig>,
    /// Optional decay config override.
    #[serde(default)]
    pub decay: Option<DecayConfig>,
    /// Optional cache config override.
    #[serde(default)]
    pub cache: Option<CacheConfig>,
    /// Optional embedding config override.
    #[serde(default)]
    pub embedding: Option<EmbeddingConfig>,
    /// Optional graph config override.
    #[serde(default)]
    pub graph: Option<GraphConfig>,
    /// Optional RIF config override.
    #[serde(default)]
    pub rif: Option<RifConfig>,
    /// Optional log config override.
    #[serde(default)]
    pub log: Option<LogConfig>,
}

impl PerDirConfig {
    /// Validate the per-directory config.
    pub fn validate(&self) -> std::result::Result<(), Vec<ConfigError>> {
        let mut errors = Vec::new();

        if !is_valid_namespace_name(&self.namespace) {
            errors.push(ConfigError::Validation {
                field: "namespace".into(),
                message: format!(
                    "invalid namespace name '{}' (must match ^[a-zA-Z0-9_-]{{1,64}}$)",
                    self.namespace
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

/// Check whether a namespace name is valid: 1-64 chars, alphanumeric plus `_` and `-`.
fn is_valid_namespace_name(name: &str) -> bool {
    if name.is_empty() || name.len() > 64 {
        return false;
    }
    name.chars()
        .all(|c| c.is_alphanumeric() || c == '_' || c == '-')
}

/// Loaded configuration with the resolved default namespace.
///
/// Returned by [`load_config`] so callers receive both the fully-resolved
/// `RecalldConfig` and the default namespace derived from per-directory config.
#[derive(Debug)]
pub struct LoadedConfig {
    /// The fully-resolved configuration.
    pub config: RecalldConfig,
    /// The default namespace for MCP operations from this directory.
    /// Falls back to `"default"` when no `.recalld.toml` is found.
    pub default_namespace: String,
}

/// Walk up from `start_dir` to find the nearest `.recalld.toml`.
///
/// Returns `None` if no per-directory config file is found before reaching
/// the filesystem root.
fn find_per_dir_config(start_dir: &Path) -> Option<PathBuf> {
    let mut current = start_dir.canonicalize().ok()?;
    loop {
        let candidate = current.join(".recalld.toml");
        if candidate.exists() && candidate.is_file() {
            return Some(candidate);
        }
        if !current.pop() {
            // Reached filesystem root
            return None;
        }
    }
}

/// Load and parse a per-directory config file from disk.
fn load_per_dir_config(path: &Path) -> std::result::Result<PerDirConfig, Vec<ConfigError>> {
    let contents = std::fs::read_to_string(path).map_err(|e| {
        vec![ConfigError::ReadError {
            path: path.to_path_buf(),
            source: e,
        }]
    })?;

    let per_dir: PerDirConfig = toml::from_str(&contents).map_err(|e| {
        vec![ConfigError::ParseError {
            path: path.to_path_buf(),
            message: e.to_string(),
        }]
    })?;

    per_dir.validate()?;

    Ok(per_dir)
}

/// Apply per-directory config overrides to a base config (section-level replacement).
fn apply_per_dir_overrides(base: RecalldConfig, per_dir: &PerDirConfig) -> RecalldConfig {
    RecalldConfig {
        server: per_dir.server.clone().unwrap_or(base.server),
        storage: per_dir.storage.clone().unwrap_or(base.storage),
        decay: per_dir.decay.clone().unwrap_or(base.decay),
        cache: per_dir.cache.clone().unwrap_or(base.cache),
        embedding: per_dir.embedding.clone().unwrap_or(base.embedding),
        graph: per_dir.graph.clone().unwrap_or(base.graph),
        rif: per_dir.rif.clone().unwrap_or(base.rif),
        log: per_dir.log.clone().unwrap_or(base.log),
        timezone: base.timezone,
    }
}

/// Load configuration by applying layers in priority order.
///
/// Returns a [`LoadedConfig`] containing the fully-resolved, validated
/// configuration together with the default namespace (from per-directory
/// config or `"default"` as fallback).
///
/// Layer order (highest priority wins):
/// 1. Compiled defaults
/// 2. Global TOML (`~/.recalld/config.toml`)
/// 3. Per-directory TOML (`.recalld.toml` — closest ancestor)
/// 4. Environment variables (`RECALLD_*`)
/// 5. CLI flags
pub fn load_config(
    config_path: Option<&Path>,
    cli_overrides: &CliOverrides,
) -> std::result::Result<LoadedConfig, Vec<ConfigError>> {
    // Layer 1: defaults
    let mut config = RecalldConfig::default();
    let mut default_namespace = "default".to_string();

    // Layer 2: TOML file
    // Search order: explicit --config flag → ./recalld.toml → ~/.recalld/config.toml
    let path = if let Some(p) = config_path {
        let pb = p.to_path_buf();
        if !pb.exists() {
            return Err(vec![ConfigError::FileNotFound { path: pb }]);
        }
        Some(pb)
    } else {
        let local = PathBuf::from("recalld.toml");
        let home = dirs::home_dir().map(|h| h.join(".recalld").join("config.toml"));
        if local.exists() {
            Some(local)
        } else if let Some(ref h) = home {
            if h.exists() { Some(h.clone()) } else { None }
        } else {
            None
        }
    };

    if let Some(ref path) = path {
        let contents = std::fs::read_to_string(path).map_err(|e| {
            vec![ConfigError::ReadError {
                path: path.clone(),
                source: e,
            }]
        })?;

        let file_config: RecalldConfig = toml::from_str(&contents).map_err(|e| {
            vec![ConfigError::ParseError {
                path: path.clone(),
                message: e.to_string(),
            }]
        })?;

        config = merge_config(config, file_config);
    }

    // Layer 3: Per-directory TOML (.recalld.toml — closest ancestor)
    if let Ok(cwd) = std::env::current_dir() {
        if let Some(per_dir_path) = find_per_dir_config(&cwd) {
            match load_per_dir_config(&per_dir_path) {
                Ok(per_dir) => {
                    tracing::debug!(
                        path = %per_dir_path.display(),
                        namespace = %per_dir.namespace,
                        "loaded per-directory config"
                    );
                    default_namespace = per_dir.namespace.clone();
                    config = apply_per_dir_overrides(config, &per_dir);
                }
                Err(errors) => {
                    // Log warning and continue (graceful degradation)
                    for error in &errors {
                        tracing::warn!(%error, "invalid per-directory config, ignoring");
                    }
                }
            }
        }
    }

    // Layer 4: environment variables
    apply_env_overrides(&mut config)?;

    // Layer 5: CLI flags
    apply_cli_overrides(&mut config, cli_overrides);

    // Validate the fully-resolved config
    config.validate()?;

    Ok(LoadedConfig {
        config,
        default_namespace,
    })
}

/// Merge a file-parsed config into the base.
///
/// Because serde fills defaults for omitted fields, the file config is
/// already complete. It fully replaces the base.
fn merge_config(_base: RecalldConfig, file: RecalldConfig) -> RecalldConfig {
    file
}

/// Apply environment variable overrides to the configuration.
///
/// Convention: `RECALLD_<SECTION>_<FIELD>` in SCREAMING_SNAKE_CASE.
fn apply_env_overrides(config: &mut RecalldConfig) -> std::result::Result<(), Vec<ConfigError>> {
    let mut errors = Vec::new();

    // Helper: read env var, parse, apply. Collects errors.
    macro_rules! env_override {
        ($var:expr, $field:expr, $type:ty) => {
            if let Ok(val) = std::env::var($var) {
                match val.parse::<$type>() {
                    Ok(parsed) => $field = parsed,
                    Err(_) => errors.push(ConfigError::EnvVarInvalid {
                        var: $var.to_string(),
                        message: format!("cannot parse '{}' as {}", val, stringify!($type)),
                    }),
                }
            }
        };
    }

    // Helper for String fields (no parse needed).
    macro_rules! env_override_string {
        ($var:expr, $field:expr) => {
            if let Ok(val) = std::env::var($var) {
                $field = val;
            }
        };
    }

    // Helper for Option<String> fields.
    macro_rules! env_override_opt_string {
        ($var:expr, $field:expr) => {
            if let Ok(val) = std::env::var($var) {
                $field = if val.is_empty() { None } else { Some(val) };
            }
        };
    }

    // --- Server ---
    env_override_string!("RECALLD_SERVER_BIND_ADDRESS", config.server.bind_address);
    env_override!("RECALLD_SERVER_PORT", config.server.port, u16);
    env_override!(
        "RECALLD_SERVER_REQUEST_TIMEOUT_MS",
        config.server.request_timeout_ms,
        u64
    );
    env_override!(
        "RECALLD_SERVER_MAX_BODY_BYTES",
        config.server.max_body_bytes,
        usize
    );

    // --- Storage ---
    env_override_string!("RECALLD_STORAGE_DATA_DIR", config.storage.data_dir);
    env_override!(
        "RECALLD_STORAGE_MAX_VECTOR_FILE_SIZE",
        config.storage.max_vector_file_size,
        u64
    );
    env_override!(
        "RECALLD_STORAGE_COMPACTION_THRESHOLD",
        config.storage.compaction_threshold,
        f64
    );
    env_override!(
        "RECALLD_STORAGE_FSYNC_INTERVAL_MS",
        config.storage.fsync_interval_ms,
        u64
    );

    // --- Decay ---
    env_override!(
        "RECALLD_DECAY_SWEEP_INTERVAL_HOURS",
        config.decay.sweep_interval_hours,
        f64
    );
    env_override!(
        "RECALLD_DECAY_FULL_TO_SUMMARY",
        config.decay.phase_thresholds.full_to_summary,
        f64
    );
    env_override!(
        "RECALLD_DECAY_SUMMARY_TO_GHOST",
        config.decay.phase_thresholds.summary_to_ghost,
        f64
    );
    env_override!(
        "RECALLD_DECAY_GHOST_TO_DELETE",
        config.decay.phase_thresholds.ghost_to_delete,
        f64
    );
    env_override!(
        "RECALLD_DECAY_PERMASTORE_THRESHOLD_DAYS",
        config.decay.permastore_threshold_days,
        f64
    );
    env_override!(
        "RECALLD_DECAY_RATE_MULTIPLIER",
        config.decay.decay_rate_multiplier,
        f64
    );

    // --- Cache ---
    env_override!(
        "RECALLD_CACHE_MAX_CAPACITY_BYTES",
        config.cache.max_capacity_bytes,
        u64
    );
    env_override!(
        "RECALLD_CACHE_TIME_TO_IDLE_SECS",
        config.cache.time_to_idle_secs,
        u64
    );
    env_override!(
        "RECALLD_CACHE_TIME_TO_LIVE_SECS",
        config.cache.time_to_live_secs,
        u64
    );
    env_override!(
        "RECALLD_CACHE_WARM_FILE_ENABLED",
        config.cache.warm_file_enabled,
        bool
    );

    // --- Embedding ---
    if let Ok(val) = std::env::var("RECALLD_EMBEDDING_PROVIDER") {
        match val.to_lowercase().as_str() {
            "openai" => config.embedding.provider = EmbeddingProvider::OpenAI,
            "ollama" => config.embedding.provider = EmbeddingProvider::Ollama,
            "passthrough" => config.embedding.provider = EmbeddingProvider::Passthrough,
            "bedrock" => config.embedding.provider = EmbeddingProvider::Bedrock,
            _ => errors.push(ConfigError::EnvVarInvalid {
                var: "RECALLD_EMBEDDING_PROVIDER".into(),
                message: format!(
                    "'{}' is not a valid provider (openai, ollama, bedrock, passthrough)",
                    val
                ),
            }),
        }
    }
    env_override_string!("RECALLD_EMBEDDING_MODEL_NAME", config.embedding.model_name);
    env_override_string!(
        "RECALLD_EMBEDDING_API_KEY_ENV",
        config.embedding.api_key_env
    );
    env_override_string!("RECALLD_EMBEDDING_BASE_URL", config.embedding.base_url);
    env_override!(
        "RECALLD_EMBEDDING_DIMENSIONS",
        config.embedding.dimensions,
        usize
    );
    env_override!(
        "RECALLD_EMBEDDING_BATCH_SIZE",
        config.embedding.batch_size,
        usize
    );
    env_override_string!("RECALLD_EMBEDDING_REGION", config.embedding.region);

    // --- Graph ---
    env_override!(
        "RECALLD_GRAPH_MAX_AUTO_LINKS",
        config.graph.max_auto_links,
        usize
    );
    env_override!(
        "RECALLD_GRAPH_AUTO_LINK_THRESHOLD",
        config.graph.auto_link_threshold,
        f64
    );
    env_override!(
        "RECALLD_GRAPH_SPREADING_ACTIVATION_S_MAX",
        config.graph.spreading_activation_s_max,
        f64
    );
    env_override!("RECALLD_GRAPH_MAX_BONUS", config.graph.max_bonus, f64);

    // --- RIF ---
    env_override!("RECALLD_RIF_ENABLED", config.rif.enabled, bool);
    env_override!(
        "RECALLD_RIF_MAX_SUPPRESSION",
        config.rif.max_suppression,
        f64
    );
    env_override!(
        "RECALLD_RIF_ACTIVATION_THRESHOLD_LOW",
        config.rif.activation_threshold_low,
        f64
    );
    env_override!(
        "RECALLD_RIF_ACTIVATION_THRESHOLD_HIGH",
        config.rif.activation_threshold_high,
        f64
    );
    env_override!(
        "RECALLD_RIF_PROPAGATION_DEPTH",
        config.rif.propagation_depth,
        u32
    );

    // --- Log ---
    env_override_string!("RECALLD_LOG_LEVEL", config.log.level);
    if let Ok(val) = std::env::var("RECALLD_LOG_FORMAT") {
        match val.to_lowercase().as_str() {
            "json" => config.log.format = LogFormat::Json,
            "pretty" => config.log.format = LogFormat::Pretty,
            _ => errors.push(ConfigError::EnvVarInvalid {
                var: "RECALLD_LOG_FORMAT".into(),
                message: format!("'{}' is not a valid format (json, pretty)", val),
            }),
        }
    }
    env_override_opt_string!("RECALLD_LOG_FILE", config.log.file);

    // --- Timezone ---
    env_override_string!("RECALLD_TIMEZONE", config.timezone);

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

/// Apply CLI flag overrides to the configuration.
fn apply_cli_overrides(config: &mut RecalldConfig, cli: &CliOverrides) {
    if let Some(port) = cli.port {
        config.server.port = port;
    }
    if let Some(ref addr) = cli.bind_address {
        config.server.bind_address = addr.clone();
    }
    if let Some(ref dir) = cli.data_dir {
        config.storage.data_dir = dir.clone();
    }
    if let Some(ref level) = cli.log_level {
        config.log.level = level.clone();
    }
    if let Some(ref fmt) = cli.log_format {
        match fmt.to_lowercase().as_str() {
            "json" => config.log.format = LogFormat::Json,
            "pretty" => config.log.format = LogFormat::Pretty,
            _ => {} // validation will catch invalid values
        }
    }
}

/// Generate a default TOML configuration string with all values commented out.
pub fn generate_default_config() -> String {
    r#"# Recalld Configuration
# All values shown are defaults. Uncomment and modify as needed.

[server]
# bind_address = "127.0.0.1"
# port = 7680
# request_timeout_ms = 30000
# max_body_bytes = 10485760          # 10 MB

[storage]
# data_dir = "~/.recalld/data"
# max_vector_file_size = 2147483648  # 2 GB (warning threshold)
# compaction_threshold = 0.20
# fsync_interval_ms = 5000

[decay]
# sweep_interval_hours = 24.0
# permastore_threshold_days = 1500.0
# decay_rate_multiplier = 1.0           # 1.0 = normal, 2.0 = 2x slower, 0.0 = disabled

[decay.phase_thresholds]
# full_to_summary = 0.7
# summary_to_ghost = 0.3
# ghost_to_delete = 0.05

[cache]
# max_capacity_bytes = 1073741824    # 1 GB
# time_to_idle_secs = 3600           # 1 hour
# time_to_live_secs = 86400          # 24 hours
# warm_file_enabled = true

[embedding]
# provider = "openai"                # "openai", "ollama", "bedrock", or "passthrough"
# model_name = "text-embedding-3-small"
# api_key_env = "OPENAI_API_KEY"     # env var holding the key (NOT the key itself)
# base_url = "https://api.openai.com/v1"
# dimensions = 1536
# batch_size = 64
# region = "us-east-1"               # AWS region (bedrock only)

[graph]
# max_auto_links = 15
# auto_link_threshold = 0.50
# spreading_activation_s_max = 2.0
# max_bonus = 0.15

[rif]
# enabled = true
# max_suppression = 0.25
# activation_threshold_low = 0.1
# activation_threshold_high = 0.45
# propagation_depth = 2

[log]
# level = "info"
# format = "pretty"                  # "pretty" or "json"
# file = "/var/log/recalld.log"       # optional, stderr-only if omitted
"#
    .to_string()
}
