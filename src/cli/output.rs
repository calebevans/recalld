//! Output formatting for the Recalld CLI.
//!
//! Defines CLI-owned view types that mirror API response shapes but are
//! decoupled from the wire format, plus the [`OutputFormatter`] trait
//! with [`JsonFormatter`] and [`HumanFormatter`] implementations.

use comfy_table::{presets, Attribute, Cell, ContentArrangement, Table};
use colored::Colorize;

use crate::cli::CliError;

// ── View Types ────────────────────────────────────────────────────

/// A memory as seen by the CLI. Mirrors the API response shape but
/// is CLI-owned so the CLI can evolve independently of the wire format.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MemoryView {
    /// UUID string of the memory.
    pub id: String,
    /// Namespace the memory belongs to.
    pub namespace: String,
    /// Unix millis when the memory was created.
    pub created_at: i64,
    /// Unix millis when the memory was last accessed.
    pub last_accessed_at: i64,
    /// Human-readable summary text.
    pub summary: String,
    /// Optional full text content.
    pub full_text: Option<String>,
    /// Attached tags.
    pub tags: Vec<String>,
    /// Current decay phase name (full, summary, ghost).
    pub decay_phase: String,
    /// Current retrievability score (0.0-1.0).
    pub retrievability: f32,
    /// Current decay strength (0.0-1.0).
    pub decay_strength: f32,
    /// FSRS stability in days.
    pub stability: f32,
    /// FSRS difficulty parameter.
    pub difficulty: f32,
    /// Whether this memory is in the permastore.
    pub is_permastore: bool,
    /// Number of graph edges connected to this memory.
    pub edge_count: u16,
}

/// Search results returned by the recall command.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchResult {
    /// Matching memories with scores.
    pub memories: Vec<SearchHit>,
    /// Total number of matches (may exceed returned count).
    pub total_matches: u32,
    /// The original search query.
    pub query: String,
}

/// A single search hit with relevance score.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchHit {
    /// The matched memory.
    pub memory: MemoryView,
    /// Relevance score (higher is better).
    pub score: f32,
    /// Spreading activation score for graph-discovered results.
    /// None for direct matches.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub activation_score: Option<f32>,
}

/// Full debug view of a memory including history and connections.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InspectView {
    /// The memory being inspected.
    pub memory: MemoryView,
    /// Recent access events.
    pub access_history: Vec<AccessEventView>,
    /// Graph connections to other memories.
    pub connections: Vec<ConnectionView>,
    /// First 8 embedding dimensions (for debugging).
    pub embedding_preview: Option<Vec<f32>>,
}

/// A single access event in a memory's history.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccessEventView {
    /// Unix millis when the access occurred.
    pub timestamp: i64,
    /// Kind of access (e.g., "direct_retrieval", "manual_reinforcement").
    pub kind: String,
}

/// A graph connection to another memory.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectionView {
    /// UUID of the connected memory.
    pub target_id: String,
    /// Summary of the connected memory.
    pub target_summary: String,
    /// Type of edge (e.g., "associative", "parent_child").
    pub edge_type: String,
    /// Edge weight (0.0-1.0).
    pub weight: f32,
}

/// A namespace as seen by the CLI.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NamespaceView {
    /// Namespace name.
    pub name: String,
    /// Numeric namespace ID.
    pub id: u32,
    /// Embedding dimensionality.
    pub embedding_dim: u32,
    /// Number of memories in the namespace.
    pub memory_count: u64,
    /// Unix millis when the namespace was created.
    pub created_at: i64,
}

/// Statistics for a namespace.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NamespaceStatsView {
    /// Namespace name.
    pub name: String,
    /// Total memory count.
    pub memory_count: u64,
    /// Counts broken down by decay phase.
    pub phase_counts: PhaseCounts,
    /// Number of permastore memories.
    pub permastore_count: u64,
    /// Total disk usage in bytes.
    pub disk_usage_bytes: u64,
}

/// Memory counts by decay phase.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PhaseCounts {
    /// Memories in full phase.
    pub full: u64,
    /// Memories in summary phase.
    pub summary: u64,
    /// Memories in ghost phase.
    pub ghost: u64,
}

/// Result of a decay sweep.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SweepResult {
    /// Whether this was a dry run.
    pub dry_run: bool,
    /// Number of records scanned.
    pub records_scanned: u64,
    /// Phase transitions that occurred (or would occur in dry run).
    pub phase_transitions: Vec<PhaseTransition>,
    /// Number of memories deleted (or that would be deleted).
    pub deletions: u64,
    /// Duration of the sweep in milliseconds.
    pub duration_ms: u64,
}

/// A single phase transition during a sweep.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PhaseTransition {
    /// UUID of the affected memory.
    pub memory_id: String,
    /// Previous phase.
    pub from_phase: String,
    /// New phase.
    pub to_phase: String,
    /// Current strength at transition time.
    pub strength: f32,
}

/// System status and health information.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StatusView {
    /// Server uptime in seconds.
    pub uptime_seconds: u64,
    /// Total memory count across all namespaces.
    pub total_memories: u64,
    /// Counts by decay phase.
    pub phase_counts: PhaseCounts,
    /// Number of permastore memories.
    pub permastore_count: u64,
    /// Number of namespaces.
    pub namespace_count: u32,
    /// Cache hit rate (0.0-1.0).
    pub cache_hit_rate: f32,
    /// Number of entries in the cache.
    pub cache_entries: u64,
    /// Unix millis of the last sweep, if any.
    pub last_sweep_at: Option<i64>,
}

/// Result of storing a new memory.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StoreResult {
    /// UUID of the newly created memory.
    pub id: String,
    /// Namespace the memory was stored in.
    pub namespace: String,
    /// Number of automatic graph links created.
    pub auto_links: u32,
}

/// Result of deleting (forgetting) a memory.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ForgetResult {
    /// UUID of the deleted memory.
    pub id: String,
    /// Number of graph edges removed.
    pub edges_removed: u32,
}

/// Result of reinforcing a memory.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReinforceResult {
    /// UUID of the reinforced memory.
    pub id: String,
    /// Stability before reinforcement (days).
    pub old_stability: f32,
    /// Stability after reinforcement (days).
    pub new_stability: f32,
    /// Strength before reinforcement (0.0-1.0).
    pub old_strength: f32,
    /// Strength after reinforcement (0.0-1.0).
    pub new_strength: f32,
}

// ── Formatter Trait ────────────────────────────────────────────────

/// Trait for formatting CLI output. Implementations decide how to
/// render each response type (JSON vs human-readable tables).
pub trait OutputFormatter {
    /// Format a store result.
    fn store(&self, result: &StoreResult) -> String;
    /// Format search/recall results.
    fn recall(&self, result: &SearchResult) -> String;
    /// Format a single memory (get command).
    fn get(&self, memory: &MemoryView) -> String;
    /// Format a forget/delete result.
    fn forget(&self, result: &ForgetResult) -> String;
    /// Format a reinforce result.
    fn reinforce(&self, result: &ReinforceResult) -> String;
    /// Format an inspect (debug) view.
    fn inspect(&self, view: &InspectView) -> String;
    /// Format a list of namespaces.
    fn namespaces_list(&self, namespaces: &[NamespaceView]) -> String;
    /// Format namespace statistics.
    fn namespace_stats(&self, stats: &[NamespaceStatsView]) -> String;
    /// Format a sweep result.
    fn sweep(&self, result: &SweepResult) -> String;
    /// Format system status.
    fn status(&self, status: &StatusView) -> String;
    /// Format a single exported memory record.
    fn export_record(&self, memory: &MemoryView) -> String;
    /// Format an error message.
    fn error(&self, err: &CliError) -> String;
}

// ── JsonFormatter ─────────────────────────────────────────────────

/// JSON output — the default. Every method serializes to a single
/// JSON object on one line, suitable for piping to jq or LLM tool-use
/// consumption.
pub struct JsonFormatter;

impl OutputFormatter for JsonFormatter {
    fn store(&self, result: &StoreResult) -> String {
        serde_json::to_string(result).unwrap()
    }

    fn recall(&self, result: &SearchResult) -> String {
        serde_json::to_string(result).unwrap()
    }

    fn get(&self, memory: &MemoryView) -> String {
        serde_json::to_string(memory).unwrap()
    }

    fn forget(&self, result: &ForgetResult) -> String {
        serde_json::to_string(result).unwrap()
    }

    fn reinforce(&self, result: &ReinforceResult) -> String {
        serde_json::to_string(result).unwrap()
    }

    fn inspect(&self, view: &InspectView) -> String {
        serde_json::to_string(view).unwrap()
    }

    fn namespaces_list(&self, namespaces: &[NamespaceView]) -> String {
        serde_json::to_string(namespaces).unwrap()
    }

    fn namespace_stats(&self, stats: &[NamespaceStatsView]) -> String {
        serde_json::to_string(stats).unwrap()
    }

    fn sweep(&self, result: &SweepResult) -> String {
        serde_json::to_string(result).unwrap()
    }

    fn status(&self, status: &StatusView) -> String {
        serde_json::to_string(status).unwrap()
    }

    fn export_record(&self, memory: &MemoryView) -> String {
        serde_json::to_string(memory).unwrap()
    }

    fn error(&self, err: &CliError) -> String {
        serde_json::to_string(&serde_json::json!({
            "error": format!("{err}")
        }))
        .unwrap()
    }
}

// ── HumanFormatter ────────────────────────────────────────────────

/// Human-readable output with tables and colors. For terminal
/// debugging — not intended for machine consumption.
pub struct HumanFormatter;

impl HumanFormatter {
    /// Format a Unix millis timestamp as a human-readable relative string.
    /// Example: "2h ago", "3d ago", "14m ago".
    fn relative_time(millis: i64) -> String {
        let now = chrono::Utc::now().timestamp_millis();
        let delta_secs = (now - millis) / 1000;
        if delta_secs < 0 {
            return "in the future".to_string();
        }
        let delta = delta_secs as u64;
        match delta {
            0..=59 => format!("{}s ago", delta),
            60..=3599 => format!("{}m ago", delta / 60),
            3600..=86399 => format!("{}h ago", delta / 3600),
            86400..=2591999 => format!("{}d ago", delta / 86400),
            _ => format!("{}mo ago", delta / 2592000),
        }
    }

    /// Color a phase string by severity.
    fn phase_colored(phase: &str) -> String {
        match phase {
            "full" => "full".green().to_string(),
            "summary" => "summary".yellow().to_string(),
            "ghost" => "ghost".red().to_string(),
            other => other.dimmed().to_string(),
        }
    }

    /// Format a strength value as a colored percentage.
    fn strength_colored(strength: f32) -> String {
        let pct = format!("{:.0}%", strength * 100.0);
        if strength >= 0.7 {
            pct.green().to_string()
        } else if strength >= 0.3 {
            pct.yellow().to_string()
        } else {
            pct.red().to_string()
        }
    }

    /// Truncate a string to `max_len`, appending "..." if truncated.
    fn truncate(s: &str, max_len: usize) -> String {
        if s.len() <= max_len {
            s.to_string()
        } else {
            format!("{}...", &s[..max_len.saturating_sub(3)])
        }
    }

    /// Format bytes as human-readable size (KB, MB, GB).
    fn format_bytes(bytes: u64) -> String {
        if bytes < 1024 {
            format!("{} B", bytes)
        } else if bytes < 1024 * 1024 {
            format!("{:.1} KB", bytes as f64 / 1024.0)
        } else if bytes < 1024 * 1024 * 1024 {
            format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
        } else {
            format!("{:.2} GB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
        }
    }
}

impl OutputFormatter for HumanFormatter {
    fn store(&self, result: &StoreResult) -> String {
        let mut out = String::new();
        out.push_str(&format!("{} {}\n", "Stored:".green().bold(), result.id));
        out.push_str(&format!("  Namespace: {}\n", result.namespace));
        if result.auto_links > 0 {
            out.push_str(&format!(
                "  Auto-linked to {} existing memories\n",
                result.auto_links
            ));
        }
        out
    }

    fn recall(&self, result: &SearchResult) -> String {
        if result.memories.is_empty() {
            return format!("{}\n", "No results found.".dimmed());
        }

        let mut table = Table::new();
        table.load_preset(presets::UTF8_FULL_CONDENSED);
        table.set_content_arrangement(ContentArrangement::Dynamic);
        table.set_header(vec![
            Cell::new("Score").add_attribute(Attribute::Bold),
            Cell::new("Phase").add_attribute(Attribute::Bold),
            Cell::new("Strength").add_attribute(Attribute::Bold),
            Cell::new("ID").add_attribute(Attribute::Bold),
            Cell::new("Summary").add_attribute(Attribute::Bold),
            Cell::new("Tags").add_attribute(Attribute::Bold),
        ]);

        for hit in &result.memories {
            table.add_row(vec![
                Cell::new(format!("{:.3}", hit.score)),
                Cell::new(Self::phase_colored(&hit.memory.decay_phase)),
                Cell::new(Self::strength_colored(hit.memory.retrievability)),
                Cell::new(&hit.memory.id[..8]), // short ID
                Cell::new(Self::truncate(&hit.memory.summary, 60)),
                Cell::new(hit.memory.tags.join(", ")),
            ]);
        }

        let mut out = format!(
            "{} {} results for \"{}\"\n\n",
            "Found".green().bold(),
            result.total_matches,
            result.query
        );
        out.push_str(&table.to_string());
        out.push('\n');
        out
    }

    fn get(&self, memory: &MemoryView) -> String {
        let mut out = String::new();
        out.push_str(&format!("{}\n", "Memory".bold().underline()));
        out.push_str(&format!("  ID:          {}\n", memory.id));
        out.push_str(&format!("  Namespace:   {}\n", memory.namespace));
        out.push_str(&format!(
            "  Phase:       {}\n",
            Self::phase_colored(&memory.decay_phase)
        ));
        out.push_str(&format!(
            "  Strength:    {}\n",
            Self::strength_colored(memory.retrievability)
        ));
        out.push_str(&format!("  Stability:   {:.1} days\n", memory.stability));
        out.push_str(&format!(
            "  Permastore:  {}\n",
            if memory.is_permastore {
                "yes".green()
            } else {
                "no".dimmed()
            }
        ));
        out.push_str(&format!(
            "  Created:     {}\n",
            Self::relative_time(memory.created_at)
        ));
        out.push_str(&format!(
            "  Last access: {}\n",
            Self::relative_time(memory.last_accessed_at)
        ));
        out.push_str(&format!("  Tags:        {}\n", memory.tags.join(", ")));
        out.push_str(&format!("  Edges:       {}\n", memory.edge_count));
        out.push_str(&format!("\n{}\n", "Summary".bold()));
        out.push_str(&format!("  {}\n", memory.summary));
        if let Some(ref text) = memory.full_text {
            out.push_str(&format!("\n{}\n", "Full Text".bold()));
            out.push_str(&format!("  {}\n", text));
        }
        out
    }

    fn forget(&self, result: &ForgetResult) -> String {
        format!(
            "{} {} (removed {} edges)\n",
            "Deleted:".red().bold(),
            result.id,
            result.edges_removed
        )
    }

    fn reinforce(&self, result: &ReinforceResult) -> String {
        let mut out = String::new();
        out.push_str(&format!(
            "{} {}\n",
            "Reinforced:".green().bold(),
            result.id
        ));
        out.push_str(&format!(
            "  Stability: {:.1} -> {:.1} days\n",
            result.old_stability, result.new_stability
        ));
        out.push_str(&format!(
            "  Strength:  {} -> {}\n",
            Self::strength_colored(result.old_strength),
            Self::strength_colored(result.new_strength)
        ));
        out
    }

    fn inspect(&self, view: &InspectView) -> String {
        let mut out = self.get(&view.memory);

        // Access history
        out.push_str(&format!("\n{}\n", "Access History".bold()));
        if view.access_history.is_empty() {
            out.push_str("  (no accesses recorded)\n");
        } else {
            let mut table = Table::new();
            table.load_preset(presets::UTF8_FULL_CONDENSED);
            table.set_header(vec![
                Cell::new("When").add_attribute(Attribute::Bold),
                Cell::new("Kind").add_attribute(Attribute::Bold),
            ]);
            for event in &view.access_history {
                table.add_row(vec![
                    Cell::new(Self::relative_time(event.timestamp)),
                    Cell::new(&event.kind),
                ]);
            }
            out.push_str(&table.to_string());
            out.push('\n');
        }

        // Connections
        out.push_str(&format!("\n{}\n", "Connections".bold()));
        if view.connections.is_empty() {
            out.push_str("  (no connections)\n");
        } else {
            let mut table = Table::new();
            table.load_preset(presets::UTF8_FULL_CONDENSED);
            table.set_header(vec![
                Cell::new("Type").add_attribute(Attribute::Bold),
                Cell::new("Weight").add_attribute(Attribute::Bold),
                Cell::new("Target ID").add_attribute(Attribute::Bold),
                Cell::new("Summary").add_attribute(Attribute::Bold),
            ]);
            for conn in &view.connections {
                table.add_row(vec![
                    Cell::new(&conn.edge_type),
                    Cell::new(format!("{:.2}", conn.weight)),
                    Cell::new(&conn.target_id[..8]),
                    Cell::new(Self::truncate(&conn.target_summary, 50)),
                ]);
            }
            out.push_str(&table.to_string());
            out.push('\n');
        }

        // Embedding preview
        if let Some(ref dims) = view.embedding_preview {
            out.push_str(&format!("\n{}\n", "Embedding (first 8 dims)".bold()));
            let formatted: Vec<String> = dims.iter().map(|d| format!("{:.4}", d)).collect();
            out.push_str(&format!("  [{}]\n", formatted.join(", ")));
        }

        out
    }

    fn namespaces_list(&self, namespaces: &[NamespaceView]) -> String {
        if namespaces.is_empty() {
            return "No namespaces found.\n".to_string();
        }
        let mut table = Table::new();
        table.load_preset(presets::UTF8_FULL_CONDENSED);
        table.set_header(vec![
            Cell::new("Name").add_attribute(Attribute::Bold),
            Cell::new("ID").add_attribute(Attribute::Bold),
            Cell::new("Dim").add_attribute(Attribute::Bold),
            Cell::new("Memories").add_attribute(Attribute::Bold),
            Cell::new("Created").add_attribute(Attribute::Bold),
        ]);
        for ns in namespaces {
            table.add_row(vec![
                Cell::new(&ns.name),
                Cell::new(ns.id),
                Cell::new(ns.embedding_dim),
                Cell::new(ns.memory_count),
                Cell::new(Self::relative_time(ns.created_at)),
            ]);
        }
        table.to_string() + "\n"
    }

    fn namespace_stats(&self, stats: &[NamespaceStatsView]) -> String {
        let mut out = String::new();
        for s in stats {
            out.push_str(&format!("{} {}\n", "Namespace:".bold(), s.name));
            out.push_str(&format!("  Memories:   {}\n", s.memory_count));
            out.push_str(&format!("  Full:       {}\n", s.phase_counts.full));
            out.push_str(&format!("  Summary:    {}\n", s.phase_counts.summary));
            out.push_str(&format!("  Ghost:      {}\n", s.phase_counts.ghost));
            out.push_str(&format!("  Permastore: {}\n", s.permastore_count));
            out.push_str(&format!(
                "  Disk:       {}\n",
                Self::format_bytes(s.disk_usage_bytes)
            ));
            out.push('\n');
        }
        out
    }

    fn sweep(&self, result: &SweepResult) -> String {
        let label = if result.dry_run {
            "Dry run".yellow().bold()
        } else {
            "Sweep complete".green().bold()
        };
        let mut out = format!(
            "{} ({} records scanned in {}ms)\n",
            label, result.records_scanned, result.duration_ms
        );

        if !result.phase_transitions.is_empty() {
            out.push_str(&format!("\n{}\n", "Phase Transitions".bold()));
            let mut table = Table::new();
            table.load_preset(presets::UTF8_FULL_CONDENSED);
            table.set_header(vec![
                Cell::new("ID").add_attribute(Attribute::Bold),
                Cell::new("From").add_attribute(Attribute::Bold),
                Cell::new("To").add_attribute(Attribute::Bold),
                Cell::new("Strength").add_attribute(Attribute::Bold),
            ]);
            for pt in &result.phase_transitions {
                table.add_row(vec![
                    Cell::new(&pt.memory_id[..8]),
                    Cell::new(Self::phase_colored(&pt.from_phase)),
                    Cell::new(Self::phase_colored(&pt.to_phase)),
                    Cell::new(Self::strength_colored(pt.strength)),
                ]);
            }
            out.push_str(&table.to_string());
            out.push('\n');
        }

        if result.deletions > 0 {
            out.push_str(&format!(
                "\n{}: {} memories below R=0.05 threshold\n",
                "Deletions".red().bold(),
                result.deletions
            ));
        }

        out
    }

    fn status(&self, status: &StatusView) -> String {
        let mut out = format!("{}\n\n", "Recalld Status".bold().underline());
        out.push_str(&format!(
            "  Uptime:       {}h {}m\n",
            status.uptime_seconds / 3600,
            (status.uptime_seconds % 3600) / 60
        ));
        out.push_str(&format!("  Memories:     {}\n", status.total_memories));
        out.push_str(&format!("  Namespaces:   {}\n", status.namespace_count));
        out.push_str(&format!("\n{}\n", "Phase Distribution".bold()));
        out.push_str(&format!(
            "  {} Full  {} Summary  {} Ghost  {} Permastore\n",
            status.phase_counts.full,
            status.phase_counts.summary,
            status.phase_counts.ghost,
            status.permastore_count
        ));
        out.push_str(&format!("\n{}\n", "Cache".bold()));
        out.push_str(&format!("  Entries:  {}\n", status.cache_entries));
        out.push_str(&format!(
            "  Hit rate: {:.1}%\n",
            status.cache_hit_rate * 100.0
        ));
        if let Some(last) = status.last_sweep_at {
            out.push_str(&format!("\n  Last sweep: {}\n", Self::relative_time(last)));
        }
        out
    }

    fn export_record(&self, memory: &MemoryView) -> String {
        // In human mode, export still writes JSON (export is a data operation).
        // But prefix each record with a header line.
        format!(
            "--- {} ({}) ---\n{}\n",
            &memory.id[..8],
            memory.decay_phase,
            serde_json::to_string_pretty(memory).unwrap()
        )
    }

    fn error(&self, err: &CliError) -> String {
        format!("{} {}\n", "Error:".red().bold(), err)
    }
}
