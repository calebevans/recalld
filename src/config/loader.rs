// Configuration loader: TOML parsing, environment variable overlays, CLI flag overrides.
//
// Implements the layered configuration loading strategy:
// 1. Compiled-in defaults
// 2. TOML config file
// 3. Environment variable overrides
// 4. CLI flag overrides
// 5. Validation

use std::path::{Path, PathBuf};

use crate::config::types::{EmbeddingProvider, LogFormat};
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

/// Load configuration by applying layers in priority order.
///
/// Returns the fully-resolved, validated configuration or all errors found.
pub fn load_config(
    config_path: Option<&Path>,
    cli_overrides: &CliOverrides,
) -> std::result::Result<RecalldConfig, Vec<ConfigError>> {
    // Layer 1: defaults
    let mut config = RecalldConfig::default();

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

        let file_config: RecalldConfig =
            toml::from_str(&contents).map_err(|e| {
                vec![ConfigError::ParseError {
                    path: path.clone(),
                    message: e.to_string(),
                }]
            })?;

        config = merge_config(config, file_config);
    }

    // Layer 3: environment variables
    apply_env_overrides(&mut config)?;

    // Layer 4: CLI flags
    apply_cli_overrides(&mut config, cli_overrides);

    // Validate the fully-resolved config
    config.validate()?;

    Ok(config)
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
fn apply_env_overrides(
    config: &mut RecalldConfig,
) -> std::result::Result<(), Vec<ConfigError>> {
    let mut errors = Vec::new();

    // Helper: read env var, parse, apply. Collects errors.
    macro_rules! env_override {
        ($var:expr, $field:expr, $type:ty) => {
            if let Ok(val) = std::env::var($var) {
                match val.parse::<$type>() {
                    Ok(parsed) => $field = parsed,
                    Err(_) => errors.push(ConfigError::EnvVarInvalid {
                        var: $var.to_string(),
                        message: format!(
                            "cannot parse '{}' as {}",
                            val,
                            stringify!($type)
                        ),
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
    env_override_string!(
        "RECALLD_SERVER_BIND_ADDRESS",
        config.server.bind_address
    );
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
    env_override_string!(
        "RECALLD_STORAGE_DATA_DIR",
        config.storage.data_dir
    );
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
            "passthrough" => {
                config.embedding.provider = EmbeddingProvider::Passthrough
            }
            _ => errors.push(ConfigError::EnvVarInvalid {
                var: "RECALLD_EMBEDDING_PROVIDER".into(),
                message: format!(
                    "'{}' is not a valid provider (openai, ollama, passthrough)",
                    val
                ),
            }),
        }
    }
    env_override_string!(
        "RECALLD_EMBEDDING_MODEL_NAME",
        config.embedding.model_name
    );
    env_override_string!(
        "RECALLD_EMBEDDING_API_KEY_ENV",
        config.embedding.api_key_env
    );
    env_override_string!(
        "RECALLD_EMBEDDING_BASE_URL",
        config.embedding.base_url
    );
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
                message: format!(
                    "'{}' is not a valid format (json, pretty)",
                    val
                ),
            }),
        }
    }
    env_override_opt_string!("RECALLD_LOG_FILE", config.log.file);

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
# provider = "openai"                # "openai", "ollama", or "passthrough"
# model_name = "text-embedding-3-small"
# api_key_env = "OPENAI_API_KEY"     # env var holding the key (NOT the key itself)
# base_url = "https://api.openai.com/v1"
# dimensions = 1536
# batch_size = 64

[graph]
# max_auto_links = 15
# auto_link_threshold = 0.75
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
