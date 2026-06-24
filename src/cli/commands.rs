//! clap derive structs for the Recalld CLI.
//!
//! Defines the top-level [`Cli`] parser, all subcommand argument structs,
//! and the [`OutputFormat`] / [`ExportFormat`] enums.

use clap::{Args, Parser, Subcommand, ValueEnum};
use uuid::Uuid;

/// Recalld — AI memory with human-like forgetting.
///
/// A CLI for managing memories stored in a Recalld server.
/// Output is JSON by default (for LLM tool-use). Use --format human
/// for colored tables.
#[derive(Debug, Parser)]
#[command(name = "recalld", version, about, long_about = None)]
#[command(propagate_version = true)]
pub struct Cli {
    /// Output format. Overrides config file default.
    #[arg(long, short = 'F', global = true, value_enum)]
    pub format: Option<OutputFormat>,

    /// API server URL. Overrides config file and $RECALLD_URL.
    #[arg(long, global = true, env = "RECALLD_URL")]
    pub server: Option<String>,

    #[command(subcommand)]
    pub command: Command,
}

/// Output format for CLI responses.
#[derive(Debug, Clone, ValueEnum)]
pub enum OutputFormat {
    /// JSON output (default) — machine-readable for LLM tool-use
    Json,
    /// Human-readable tables with colors — for debugging
    Human,
}

/// Top-level CLI commands.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Store a new memory
    Store(StoreArgs),

    /// Search memories by natural language query
    Recall(RecallArgs),

    /// Get a specific memory by ID
    Get(GetArgs),

    /// Delete a memory (permanent)
    Forget(ForgetArgs),

    /// Manually reinforce a memory (increase stability)
    Reinforce(ReinforceArgs),

    /// Full debug view of a memory (stability, phase, connections, access history)
    Inspect(InspectArgs),

    /// Namespace management (list, create, stats)
    Namespaces(NamespacesArgs),

    /// Trigger a manual decay sweep
    Sweep(SweepArgs),

    /// System health: counts per phase, cache stats, uptime
    Status,

    /// Bulk export memories
    Export(ExportArgs),
}

// ── Store ──────────────────────────────────────────────────────────

/// Arguments for the `store` command.
#[derive(Debug, Args)]
pub struct StoreArgs {
    /// The memory content to store. Used as both summary and full_text.
    /// If the text exceeds 2,000 bytes, it becomes the full_text and
    /// the server generates a summary via the embedding provider.
    pub text: String,

    /// Tags to attach (comma-separated).
    /// Example: --tags "topic/rust,project/recalld"
    #[arg(long, short, value_delimiter = ',')]
    pub tags: Vec<String>,

    /// Target namespace. Defaults to the config file's default_namespace.
    #[arg(long, short)]
    pub namespace: Option<String>,

    /// Parent memory ID — creates a parent-child edge from this
    /// memory to the new one.
    #[arg(long)]
    pub parent_id: Option<Uuid>,
}

// ── Recall ─────────────────────────────────────────────────────────

/// Arguments for the `recall` (search) command.
#[derive(Debug, Args)]
pub struct RecallArgs {
    /// Natural language search query. Embedded by the server and used
    /// for similarity search.
    pub query: String,

    /// Maximum number of results. Default: 10.
    #[arg(long, short, default_value_t = 10)]
    pub limit: u32,

    /// Restrict search to a specific namespace.
    #[arg(long, short)]
    pub namespace: Option<String>,

    /// Include ghost memories (phase 3) in results.
    /// By default, ghosts are excluded because they lack summary text.
    #[arg(long)]
    pub include_ghosts: bool,

    /// Filter results by tag (comma-separated). Results must have ALL
    /// specified tags.
    #[arg(long, value_delimiter = ',')]
    pub tags: Vec<String>,

    /// Number of graph hops to include related memories. Default: 0.
    #[arg(long, default_value_t = 0)]
    pub depth: u32,

    /// Minimum strength threshold (0.0-1.0). Results below this are excluded.
    #[arg(long)]
    pub min_strength: Option<f32>,
}

// ── Get ────────────────────────────────────────────────────────────

/// Arguments for the `get` command.
#[derive(Debug, Args)]
pub struct GetArgs {
    /// Memory UUID to retrieve.
    pub id: Uuid,
}

// ── Forget ─────────────────────────────────────────────────────────

/// Arguments for the `forget` (delete) command.
#[derive(Debug, Args)]
pub struct ForgetArgs {
    /// Memory UUID to delete.
    pub id: Uuid,

    /// Skip confirmation prompt (for scripting).
    #[arg(long, short = 'y')]
    pub yes: bool,
}

// ── Reinforce ──────────────────────────────────────────────────────

/// Arguments for the `reinforce` command.
#[derive(Debug, Args)]
pub struct ReinforceArgs {
    /// Memory UUID to reinforce.
    pub id: Uuid,
}

// ── Inspect ────────────────────────────────────────────────────────

/// Arguments for the `inspect` command.
#[derive(Debug, Args)]
pub struct InspectArgs {
    /// Memory UUID to inspect.
    pub id: Uuid,
}

// ── Namespaces ─────────────────────────────────────────────────────

/// Arguments for the `namespaces` command group.
#[derive(Debug, Args)]
pub struct NamespacesArgs {
    #[command(subcommand)]
    pub action: NamespaceAction,
}

/// Namespace subcommands.
#[derive(Debug, Subcommand)]
pub enum NamespaceAction {
    /// List all namespaces
    List,

    /// Create a new namespace
    Create(CreateNamespaceArgs),

    /// Show statistics for a namespace (memory count, phase distribution, disk usage)
    Stats(NamespaceStatsArgs),
}

/// Arguments for `namespaces create`.
#[derive(Debug, Args)]
pub struct CreateNamespaceArgs {
    /// Namespace name (alphanumeric + hyphens + underscores, 1-64 chars).
    pub name: String,

    /// Embedding dimensionality (fixed after creation).
    #[arg(long, default_value_t = 1536)]
    pub dim: u32,

    /// Initial stability for new memories (days).
    #[arg(long, default_value_t = 3.7145)]
    pub initial_stability: f32,
}

/// Arguments for `namespaces stats`.
#[derive(Debug, Args)]
pub struct NamespaceStatsArgs {
    /// Namespace name. If omitted, shows stats for all namespaces.
    pub name: Option<String>,
}

// ── Sweep ──────────────────────────────────────────────────────────

/// Arguments for the `sweep` command.
#[derive(Debug, Args)]
pub struct SweepArgs {
    /// Show what would change without applying. Reports phase
    /// transitions and deletions that would occur.
    #[arg(long)]
    pub dry_run: bool,

    /// Restrict sweep to a specific namespace.
    #[arg(long, short)]
    pub namespace: Option<String>,
}

// ── Export ──────────────────────────────────────────────────────────

/// Arguments for the `export` command.
#[derive(Debug, Args)]
pub struct ExportArgs {
    /// Restrict export to a specific namespace.
    #[arg(long, short)]
    pub namespace: Option<String>,

    /// Export format.
    #[arg(long, default_value = "json", value_enum)]
    pub export_format: ExportFormat,

    /// Include full_text in export (increases size significantly).
    #[arg(long)]
    pub include_text: bool,

    /// Include embeddings in export (very large).
    #[arg(long)]
    pub include_embeddings: bool,
}

/// Export file format.
#[derive(Debug, Clone, ValueEnum)]
pub enum ExportFormat {
    /// Single JSON array
    Json,
    /// One JSON object per line (streaming-friendly)
    Jsonl,
}
