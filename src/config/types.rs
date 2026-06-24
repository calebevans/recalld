//! Configuration sub-types for the Recalld memory system.
//!
//! Each struct represents a section of the TOML configuration file.
//! All structs derive `Deserialize`, `Serialize`, `Clone`, `Debug` and use
//! `#[serde(default)]` so partial TOML files are valid.

use serde::{Deserialize, Serialize};

/// HTTP server settings for the axum API.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ServerConfig {
    /// IP address to bind to.
    pub bind_address: String,

    /// TCP port to listen on.
    pub port: u16,

    /// Maximum time (ms) before a request is aborted.
    pub request_timeout_ms: u64,

    /// Maximum request body size in bytes (prevents OOM from large payloads).
    pub max_body_bytes: usize,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind_address: "127.0.0.1".to_string(),
            port: 7680,
            request_timeout_ms: 30_000,
            max_body_bytes: 10 * 1024 * 1024, // 10 MB
        }
    }
}

/// Disk storage paths and compaction tuning.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct StorageConfig {
    /// Root directory for all storage files.
    pub data_dir: String,

    /// Maximum size (bytes) of a single vectors.dat file before warning.
    pub max_vector_file_size: u64,

    /// Fraction of dead space in fulltext.dat that triggers compaction (0.0-1.0).
    pub compaction_threshold: f64,

    /// Batch fsync interval in milliseconds.
    pub fsync_interval_ms: u64,
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            data_dir: dirs::home_dir()
                .map(|h| {
                    h.join(".recalld")
                        .join("data")
                        .to_string_lossy()
                        .into_owned()
                })
                .unwrap_or_else(|| ".recalld/data".to_string()),
            max_vector_file_size: 2 * 1024 * 1024 * 1024, // 2 GB
            compaction_threshold: 0.20,
            fsync_interval_ms: 5_000,
        }
    }
}

/// FSRS decay engine tuning.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct DecayConfig {
    /// Hours between decay sweep runs.
    pub sweep_interval_hours: f64,

    /// Phase transition thresholds for retrievability.
    pub phase_thresholds: PhaseThresholds,

    /// Stability (days) above which a memory is considered permanent.
    pub permastore_threshold_days: f64,

    /// Skip starting the decay sweep runner entirely (used by benchmarks).
    #[serde(default)]
    pub disable_sweep: bool,

    /// Global decay rate multiplier.
    /// - 1.0 (default) = normal FSRS decay
    /// - > 1.0 = slower decay (memories last longer)
    /// - < 1.0 = faster decay (memories forgotten sooner)
    /// - 0.0 = decay disabled (infinite stability, no transitions)
    #[serde(default = "default_decay_rate_multiplier")]
    pub decay_rate_multiplier: f64,
}

fn default_decay_rate_multiplier() -> f64 {
    1.0
}

/// Phase boundary thresholds for FSRS decay.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct PhaseThresholds {
    /// R below this triggers Full -> Summary transition.
    pub full_to_summary: f64,
    /// R below this triggers Summary -> Ghost transition.
    pub summary_to_ghost: f64,
    /// R below this triggers Ghost -> deletion.
    pub ghost_to_delete: f64,
}

impl Default for PhaseThresholds {
    fn default() -> Self {
        Self {
            full_to_summary: 0.7,
            summary_to_ghost: 0.3,
            ghost_to_delete: 0.05,
        }
    }
}

impl Default for DecayConfig {
    fn default() -> Self {
        Self {
            sweep_interval_hours: 24.0,
            phase_thresholds: PhaseThresholds::default(),
            permastore_threshold_days: 1500.0,
            disable_sweep: false,
            decay_rate_multiplier: 1.0,
        }
    }
}

/// RAM cache sizing and eviction.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct CacheConfig {
    /// Maximum cache size in bytes.
    pub max_capacity_bytes: u64,

    /// Seconds of idle time before eviction eligibility.
    pub time_to_idle_secs: u64,

    /// Absolute maximum seconds a record can live in cache.
    pub time_to_live_secs: u64,

    /// Whether to write and read the warm.bin cache snapshot.
    pub warm_file_enabled: bool,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            max_capacity_bytes: 1024 * 1024 * 1024, // 1 GB
            time_to_idle_secs: 3600,                // 1 hour
            time_to_live_secs: 86400,               // 24 hours
            warm_file_enabled: true,
        }
    }
}

/// Embedding provider selection and connection settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct EmbeddingConfig {
    /// Which embedding provider to use.
    pub provider: EmbeddingProvider,

    /// Model identifier (provider-specific).
    pub model_name: String,

    /// Name of the environment variable holding the API key.
    pub api_key_env: String,

    /// Base URL for the embedding API.
    pub base_url: String,

    /// Embedding vector dimensionality.
    pub dimensions: usize,

    /// Maximum number of texts to embed in a single API call.
    pub batch_size: usize,

    /// Prefix prepended to text before embedding during memory ingest.
    /// Applied by PrefixedProvider to every `embed()` / `embed_batch()` call.
    ///
    /// Set to empty string to disable document prefixing.
    /// Default: "title: none | text: "
    pub document_prefix: String,

    /// Prefix prepended to text before embedding during query/search.
    /// Applied by PrefixedProvider to every `embed_query()` call.
    ///
    /// Set to empty string to disable query prefixing.
    /// Default: "task: search result | query: "
    pub query_prefix: String,
}

/// Supported embedding providers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EmbeddingProvider {
    /// OpenAI embedding API.
    OpenAI,
    /// Ollama local embedding server.
    Ollama,
    /// Pre-computed embeddings supplied by the caller.
    Passthrough,
}

impl Default for EmbeddingConfig {
    fn default() -> Self {
        Self {
            provider: EmbeddingProvider::OpenAI,
            model_name: "text-embedding-3-small".to_string(),
            api_key_env: "OPENAI_API_KEY".to_string(),
            base_url: "https://api.openai.com/v1".to_string(),
            dimensions: 1536,
            batch_size: 64,
            document_prefix: "title: none | text: ".to_string(),
            query_prefix: "task: search result | query: ".to_string(),
        }
    }
}

/// Returns `true` for serde defaults.
fn default_true() -> bool {
    true
}

/// Relationship graph tuning.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct GraphConfig {
    /// Whether auto-linking is enabled during memory ingestion.
    #[serde(default = "default_true")]
    pub autolink_enabled: bool,

    /// Maximum number of auto-created edges per memory.
    pub max_auto_links: usize,

    /// Cosine similarity threshold for auto-link creation (0.0-1.0).
    pub auto_link_threshold: f64,

    /// Maximum number of entity-based edges per memory.
    pub max_entity_links: usize,

    /// Time window (ms) within which memories are linked with Temporal edges.
    /// Memories whose `created_at` timestamps differ by less than this value
    /// are candidates for temporal linking.
    /// Default: 3_600_000 (1 hour).
    pub temporal_window_ms: u64,

    /// Maximum number of Temporal edges created per memory.
    /// Prevents hub formation when many memories share the same timestamp.
    /// Default: 20.
    pub max_temporal_links: usize,

    /// ACT-R spreading activation S_max parameter.
    pub spreading_activation_s_max: f64,

    /// Maximum decay resistance bonus from spreading activation (0.0-1.0).
    pub max_bonus: f64,
}

impl Default for GraphConfig {
    fn default() -> Self {
        Self {
            autolink_enabled: true,
            max_auto_links: 15,
            auto_link_threshold: 0.50,
            max_entity_links: 10,
            temporal_window_ms: 3_600_000,
            max_temporal_links: 20,
            spreading_activation_s_max: 2.0,
            max_bonus: 0.15,
        }
    }
}

/// Retrieval-Induced Forgetting configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct RifConfig {
    /// Master switch for RIF.
    pub enabled: bool,

    /// Maximum fractional strength reduction per retrieval event (0.0-1.0).
    pub max_suppression: f64,

    /// Activation threshold below which no suppression occurs.
    pub activation_threshold_low: f64,

    /// Activation threshold above which strengthening occurs instead.
    pub activation_threshold_high: f64,

    /// How many hops of neighbors to consider for RIF effects.
    pub propagation_depth: u32,
}

impl Default for RifConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_suppression: 0.25,
            activation_threshold_low: 0.1,
            activation_threshold_high: 0.45,
            propagation_depth: 2,
        }
    }
}

/// Logging output configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct LogConfig {
    /// Minimum log level: "trace", "debug", "info", "warn", "error".
    pub level: String,

    /// Output format.
    pub format: LogFormat,

    /// Optional file path for log output.
    pub file: Option<String>,
}

/// Log output format.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogFormat {
    /// Human-readable, colored output.
    Pretty,
    /// Structured JSON lines.
    Json,
}

impl Default for LogConfig {
    fn default() -> Self {
        Self {
            level: "info".to_string(),
            format: LogFormat::Pretty,
            file: None,
        }
    }
}
