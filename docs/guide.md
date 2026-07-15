# recalld User's Guide

recalld is an AI memory system with spaced-repetition decay. It stores observations and facts as memories, embeds them for semantic search, and lets them decay over time unless reinforced.

---

## Table of Contents

1. [Installation](#1-installation)
2. [Embedding Setup](#2-embedding-setup)
3. [Configuration Reference](#3-configuration-reference)
4. [Running recalld](#4-running-recalld)
5. [Using the CLI](#5-using-the-cli-recalld-cli)
6. [MCP Tools Reference](#6-mcp-tools-reference)
7. [Namespaces](#7-namespaces)
8. [Memory Decay](#8-memory-decay)
9. [Backup and Restore](#9-backup-and-restore)
10. [Troubleshooting](#10-troubleshooting)

---

## 1. Installation

Supported platforms: macOS (x86_64, aarch64), Linux (x86_64, aarch64). Windows is not supported. Docker images support `linux/amd64` and `linux/arm64`.

### One-liner install

```bash
curl -fsSL https://raw.githubusercontent.com/calebevans/recalld/main/install.sh | bash
```

This downloads the latest release binary for your platform and places it on your PATH.

### Build from source

Requires Rust 1.94 or later. For local embeddings (no API key needed), you will also need [Ollama](https://ollama.com) -- see [Section 2](#2-embedding-setup).

```bash
git clone https://github.com/calebevans/recalld.git
cd recalld
cargo install --path .
```

Or using Make:

```bash
make install
```

### Verify installation

```bash
recalld --version
recalld-cli health
```

---

## 2. Embedding Setup

recalld needs an embedding provider to convert text into vectors for semantic search. You must configure one before storing memories. Three options: OpenAI (remote API), Ollama (local), or AWS Bedrock.

### Option A: OpenAI (remote)

1. Get an API key from [platform.openai.com](https://platform.openai.com).

2. Export it in your shell:

```bash
export OPENAI_API_KEY="sk-..."
```

3. Configure `~/.recalld/config.toml`:

```toml
[embedding]
provider = "openai"
model_name = "text-embedding-3-small"
api_key_env = "OPENAI_API_KEY"
base_url = "https://api.openai.com/v1"
dimensions = 1536
```

The `api_key_env` field is the **name of the environment variable** holding your key, not the key itself.

### Option B: Ollama (local)

1. Install Ollama from [ollama.com](https://ollama.com).

2. Pull an embedding model:

```bash
# Recommended general-purpose model (768 dims)
ollama pull nomic-embed-text

# Higher quality, larger (1024 dims)
ollama pull mxbai-embed-large

# Google's embedding model (768 dims)
ollama pull embeddinggemma
```

3. Make sure Ollama is running:

```bash
ollama serve
```

4. Configure `~/.recalld/config.toml`:

```toml
[embedding]
provider = "ollama"
model_name = "nomic-embed-text"
base_url = "http://localhost:11434"
dimensions = 768
```

### Option C: AWS Bedrock

Requires the `bedrock` cargo feature (`cargo build --features bedrock`). Pre-built release binaries include this feature. Authentication uses the standard AWS credential chain (environment variables, `~/.aws/credentials`, IAM roles).

1. Enable the embedding model in your AWS account via the [Bedrock console](https://console.aws.amazon.com/bedrock/).

2. Configure your AWS credentials:

```bash
export AWS_ACCESS_KEY_ID="AKIA..."
export AWS_SECRET_ACCESS_KEY="..."
export AWS_REGION="us-east-1"
```

3. Configure `~/.recalld/config.toml`:

```toml
[embedding]
provider = "bedrock"
model_name = "amazon.titan-embed-text-v2:0"
dimensions = 1024
region = "us-east-1"
```

Supported model families:

- **Amazon Titan Text Embeddings V2** (`amazon.titan-embed-text-v2:0`): dimensions 256, 512, or 1024. Single text per API call; batch requests are parallelized automatically.
- **Cohere Embed v3** (`cohere.embed-english-v3`, `cohere.embed-multilingual-v3`): 1024 dimensions. Native batching up to 96 texts per call with asymmetric retrieval support.

### Choosing dimensions and models

| Model | Provider | Dimensions | Notes |
|---|---|---|---|
| `text-embedding-3-small` | OpenAI | 1536 | Default. Good balance of quality and cost. |
| `text-embedding-3-large` | OpenAI | 3072 | Higher quality, higher cost and storage. |
| `nomic-embed-text` | Ollama | 768 | Good local option. No API costs. |
| `mxbai-embed-large` | Ollama | 1024 | Better quality, more RAM. |
| `embeddinggemma` | Ollama | 768 | Google's model via Ollama. |
| `amazon.titan-embed-text-v2:0` | Bedrock | 256/512/1024 | AWS managed, no infra to run. |
| `cohere.embed-english-v3` | Bedrock | 1024 | Native batching, asymmetric retrieval. |

Tradeoffs:

- **Higher dimensions** = better semantic discrimination, but more disk/RAM and slower searches.
- **OpenAI** = no local compute, requires internet, costs per token.
- **Ollama** = free, private, works offline, but requires local GPU/CPU resources.

Embedding dimensions are **fixed per namespace after creation**. If you change dimensions, you must create a new namespace -- existing memories cannot be re-embedded in place.

---

## 3. Configuration Reference

recalld loads configuration in layers (highest priority wins):

1. Compiled defaults
2. Global TOML file (`~/.recalld/config.toml` or `./recalld.toml`)
3. Per-directory TOML (`.recalld.toml` in nearest ancestor directory)
4. Environment variables (`RECALLD_<SECTION>_<FIELD>`)
5. CLI flags

You can also pass `--config /path/to/file.toml` to use a specific file.

### `[server]`

HTTP API server settings.

| Field | Type | Default | Description |
|---|---|---|---|
| `bind_address` | string | `"127.0.0.1"` | IP address to bind to. |
| `port` | u16 | `7680` | TCP port to listen on. |
| `request_timeout_ms` | u64 | `30000` | Maximum request time in milliseconds before abort. |
| `max_body_bytes` | usize | `10485760` | Maximum request body size in bytes (10 MB). |

Env vars: `RECALLD_SERVER_BIND_ADDRESS`, `RECALLD_SERVER_PORT`, `RECALLD_SERVER_REQUEST_TIMEOUT_MS`, `RECALLD_SERVER_MAX_BODY_BYTES`

### `[storage]`

Disk storage paths and compaction.

| Field | Type | Default | Description |
|---|---|---|---|
| `data_dir` | string | `"~/.recalld/data"` | Root directory for all storage files (meta.db, ns_\<name\>.dat per namespace, fulltext.dat, edges.db). |
| `max_vector_file_size` | u64 | `2147483648` | Warning threshold for vectors.dat size (2 GB). |
| `compaction_threshold` | f64 | `0.20` | Fraction of dead space in fulltext.dat that triggers compaction (0.0-1.0). |
| `fsync_interval_ms` | u64 | `5000` | Batch fsync interval in milliseconds. |

Env vars: `RECALLD_STORAGE_DATA_DIR`, `RECALLD_STORAGE_MAX_VECTOR_FILE_SIZE`, `RECALLD_STORAGE_COMPACTION_THRESHOLD`, `RECALLD_STORAGE_FSYNC_INTERVAL_MS`

### `[embedding]`

Embedding provider and model settings.

| Field | Type | Default | Description |
|---|---|---|---|
| `provider` | string | `"ollama"` | Embedding provider: `"openai"`, `"ollama"`, `"bedrock"`, or `"passthrough"`. |
| `model_name` | string | `"embeddinggemma:300m"` | Model identifier (provider-specific). |
| `api_key_env` | string | `"OPENAI_API_KEY"` | Name of the env var holding the API key (OpenAI provider only). |
| `base_url` | string | `"http://localhost:11434"` | Base URL for the embedding API (not used by Bedrock). |
| `dimensions` | usize | `768` | Embedding vector dimensionality. Fixed per namespace after creation. |
| `batch_size` | usize | `64` | Maximum texts per API call. |
| `region` | string | `"us-east-1"` | AWS region for the Bedrock provider. Ignored by other providers. |
| `document_prefix` | string | `"title: none \| text: "` | Prefix prepended to text during memory storage. Set to `""` to disable. |
| `query_prefix` | string | `"task: search result \| query: "` | Prefix prepended to text during search queries. Set to `""` to disable. |

Env vars: `RECALLD_EMBEDDING_PROVIDER`, `RECALLD_EMBEDDING_MODEL_NAME`, `RECALLD_EMBEDDING_API_KEY_ENV`, `RECALLD_EMBEDDING_BASE_URL`, `RECALLD_EMBEDDING_DIMENSIONS`, `RECALLD_EMBEDDING_BATCH_SIZE`, `RECALLD_EMBEDDING_REGION`

> **Note:** `document_prefix` and `query_prefix` can only be set via the TOML config file, not via environment variables.

### `[decay]`

FSRS decay engine tuning. Controls how memories fade over time.

| Field | Type | Default | Description |
|---|---|---|---|
| `sweep_interval_hours` | f64 | `24.0` | Hours between automatic decay sweep runs. |
| `permastore_threshold_days` | f64 | `1500.0` | Stability (days) above which a memory is permanent and exempt from decay. |
| `decay_rate_multiplier` | f64 | `1.0` | Global decay rate multiplier. `1.0` = normal FSRS decay. `>1.0` = slower decay. `<1.0` = faster decay. `0.0` = decay disabled entirely. |
| `disable_sweep` | bool | `false` | Skip starting the decay sweep runner (used by benchmarks). |

Env vars: `RECALLD_DECAY_SWEEP_INTERVAL_HOURS`, `RECALLD_DECAY_PERMASTORE_THRESHOLD_DAYS`, `RECALLD_DECAY_RATE_MULTIPLIER`

> **Note:** `disable_sweep` is a benchmark-only option with no environment variable override. It can only be set via the TOML config file.

### `[decay.phase_thresholds]`

Retrievability thresholds that trigger phase transitions.

| Field | Type | Default | Description |
|---|---|---|---|
| `full_to_summary` | f64 | `0.7` | R below this triggers Full -> Summary (full_text dropped). |
| `summary_to_ghost` | f64 | `0.3` | R below this triggers Summary -> Ghost (summary dropped). |
| `ghost_to_delete` | f64 | `0.05` | R below this triggers Ghost -> deletion. |

Constraints: `full_to_summary > summary_to_ghost > ghost_to_delete`, all values in `(0.0, 1.0)`.

Env vars: `RECALLD_DECAY_FULL_TO_SUMMARY`, `RECALLD_DECAY_SUMMARY_TO_GHOST`, `RECALLD_DECAY_GHOST_TO_DELETE`

### `[cache]`

In-memory cache sizing and eviction.

| Field | Type | Default | Description |
|---|---|---|---|
| `max_capacity_bytes` | u64 | `1073741824` | Maximum cache size (1 GB). |
| `time_to_idle_secs` | u64 | `3600` | Seconds of idle time before eviction eligibility (1 hour). |
| `time_to_live_secs` | u64 | `86400` | Absolute maximum seconds a record lives in cache (24 hours). |
| `warm_file_enabled` | bool | `true` | Write and read the warm.bin cache snapshot on startup/shutdown. |

Constraint: `time_to_idle_secs` must be `<=` `time_to_live_secs`.

Env vars: `RECALLD_CACHE_MAX_CAPACITY_BYTES`, `RECALLD_CACHE_TIME_TO_IDLE_SECS`, `RECALLD_CACHE_TIME_TO_LIVE_SECS`, `RECALLD_CACHE_WARM_FILE_ENABLED`

### `[graph]`

Relationship graph and auto-linking settings.

| Field | Type | Default | Description |
|---|---|---|---|
| `autolink_enabled` | bool | `true` | Whether auto-linking is enabled during memory ingestion. |
| `max_auto_links` | usize | `15` | Maximum auto-created edges per memory. |
| `auto_link_threshold` | f64 | `0.50` | Cosine similarity threshold for auto-link creation (0.0-1.0). |
| `max_entity_links` | usize | `10` | Maximum entity-based edges per memory. |
| `temporal_window_ms` | u64 | `3600000` | Time window (ms) for Temporal edge creation (1 hour). |
| `max_temporal_links` | usize | `20` | Maximum Temporal edges per memory. |
| `spreading_activation_s_max` | f64 | `2.0` | ACT-R spreading activation S_max parameter. |
| `max_bonus` | f64 | `0.15` | Maximum decay resistance bonus from spreading activation (0.0-1.0). |

Env vars: `RECALLD_GRAPH_MAX_AUTO_LINKS`, `RECALLD_GRAPH_AUTO_LINK_THRESHOLD`, `RECALLD_GRAPH_SPREADING_ACTIVATION_S_MAX`, `RECALLD_GRAPH_MAX_BONUS`

> **Note:** `autolink_enabled`, `max_entity_links`, `temporal_window_ms`, and `max_temporal_links` can only be set via the TOML config file, not via environment variables.

### `[rif]`

Retrieval-Induced Forgetting. When you recall one memory, similar competing memories are slightly suppressed -- like how remembering one password makes you forget the old one.

| Field | Type | Default | Description |
|---|---|---|---|
| `enabled` | bool | `true` | Master switch for RIF. |
| `max_suppression` | f64 | `0.15` | Maximum fractional stability reduction per retrieval event (0.0-1.0). |
| `activation_threshold_low` | f64 | `0.1` | Below this activation level, no suppression occurs. |
| `activation_threshold_high` | f64 | `0.45` | Above this activation level, strengthening occurs instead. |
| `propagation_depth` | u32 | `2` | How many hops of neighbors to consider for RIF effects. |

Constraint: `activation_threshold_low < activation_threshold_high`.

Env vars: `RECALLD_RIF_ENABLED`, `RECALLD_RIF_MAX_SUPPRESSION`, `RECALLD_RIF_ACTIVATION_THRESHOLD_LOW`, `RECALLD_RIF_ACTIVATION_THRESHOLD_HIGH`, `RECALLD_RIF_PROPAGATION_DEPTH`

### `[log]`

Logging output settings.

| Field | Type | Default | Description |
|---|---|---|---|
| `level` | string | `"info"` | Minimum log level: `"trace"`, `"debug"`, `"info"`, `"warn"`, `"error"`. |
| `format` | string | `"pretty"` | Output format: `"pretty"` (human-readable, colored) or `"json"` (structured). |
| `file` | string | *none* | Optional file path for log output. Logs go to stderr if omitted. |

Env vars: `RECALLD_LOG_LEVEL`, `RECALLD_LOG_FORMAT`, `RECALLD_LOG_FILE`

### Top-level fields

| Field | Type | Default | Description |
|---|---|---|---|
| `timezone` | string | `"UTC"` | Display timezone for formatted timestamps. IANA names (e.g. `"America/New_York"`), `"UTC"`, or `"local"`. |

Env var: `RECALLD_TIMEZONE`

### Per-directory config (`.recalld.toml`)

Place a `.recalld.toml` in your project root to set a default namespace and override any config section for that project. recalld walks up from the current directory to find the nearest one.

```toml
namespace = "my-project"

[embedding]
provider = "ollama"
model_name = "nomic-embed-text"
dimensions = 768
```

The `namespace` field is required. All other sections are optional and replace the corresponding global section entirely.

---

## 4. Running recalld

recalld has four run modes: MCP server (stdio), HTTP API server, daemon, and Docker container.

### As MCP server (for Claude Code / Cursor)

This is the most common setup. recalld runs as an MCP server over stdio, launched by your AI tool.

**Claude Code:**

```sh
# Global (all projects)
claude mcp add --scope user recalld -- recalld mcp

# Or project-only
claude mcp add --scope project recalld -- recalld mcp
```

**Cursor** -- add to `.cursor/mcp.json`:

```json
{
  "mcpServers": {
    "recalld": {
      "command": "recalld",
      "args": ["mcp"]
    }
  }
}
```

When running recalld in Docker or on a remote server, use the URL transport instead:

```json
{
  "mcpServers": {
    "recalld": {
      "url": "http://localhost:7680/mcp"
    }
  }
}
```

When launched in MCP mode, recalld automatically connects to a running daemon. If no daemon is running, it auto-starts one. If the daemon cannot be started, it falls back to direct mode (in-process storage).

Logs in MCP mode go to stderr (stdout is the protocol channel). Set log level with:

```bash
recalld mcp --log-level debug
```

To enable debug logging in Claude Code:

```sh
claude mcp add --scope user recalld -- recalld mcp --log-level debug
```

### As HTTP API server

Run recalld as a standalone HTTP server:

```bash
# Default: 127.0.0.1:7680
recalld serve

# Custom bind address and port
recalld serve --bind 0.0.0.0:8080

# Override just the port
recalld serve --port 9000

# With structured JSON logs
recalld serve --log-json

# With debug logging
recalld serve --log-level debug
```

The `--bind` flag sets the full address:port (e.g. `0.0.0.0:8080`). The `--port` flag overrides only the port portion of the bind address. You can also use the `RECALLD_BIND` and `RECALLD_PORT` environment variables.

#### MCP over HTTP

`recalld serve` also exposes an MCP endpoint at `/mcp` using the streamable HTTP transport. MCP clients can connect via URL instead of stdio:

```sh
claude mcp add --scope user --transport http recalld http://localhost:7680/mcp
```

Or in any MCP client config:

```json
{
  "mcpServers": {
    "recalld": {
      "url": "http://localhost:7680/mcp"
    }
  }
}
```

This is the recommended transport when running recalld in a Docker container, on a remote server, or as a shared service.

### As daemon

The daemon runs in the background, listening on a Unix socket (`~/.recalld/socket`). MCP clients connect to it via JSON-RPC 2.0. The daemon auto-shuts down after 30 minutes of inactivity by default.

```bash
# Start in background (default)
recalld daemon start

# Start with custom idle timeout (minutes, 0 = no timeout)
recalld daemon start --idle-timeout 60

# Start in foreground (useful for debugging)
recalld daemon start --foreground

# Check status
recalld daemon status

# Stop
recalld daemon stop
```

### Docker / Podman

Official images are published to GHCR at `ghcr.io/calebevans/recalld` for `linux/amd64` and `linux/arm64`. All configuration is done via `RECALLD_*` environment variables (no config file needed).

**With your host's Ollama:**

```bash
docker run -d -p 7680:7680 \
  -e RECALLD_EMBEDDING_BASE_URL=http://host.docker.internal:11434 \
  -v recalld-data:/data \
  ghcr.io/calebevans/recalld
```

On Linux with Podman, add `--add-host=host.docker.internal:host-gateway` to reach the host network.

**With Docker Compose** (includes Ollama):

```bash
docker compose up -d
docker compose exec ollama ollama pull embeddinggemma:300m
```

The `compose.yaml` in the repo root starts both recalld and Ollama with persistent volumes. To use your host's Ollama instead of a containerized one, comment out the `ollama` service and set `RECALLD_EMBEDDING_BASE_URL` to `http://host.docker.internal:11434` (see the comments in `compose.yaml`).

**Connect an MCP client:**

```bash
claude mcp add --scope user --transport http recalld http://localhost:7680/mcp
```

**Building locally:**

```bash
docker build -t recalld .
```

**Key environment variables for Docker:**

| Variable | Default | Purpose |
|---|---|---|
| `RECALLD_BIND` | `0.0.0.0:7680` | Bind address (set by Dockerfile) |
| `RECALLD_STORAGE_DATA_DIR` | `/data` | Data volume mount point (set by Dockerfile) |
| `RECALLD_EMBEDDING_BASE_URL` | `http://localhost:11434` | Ollama URL (override for Docker networking) |
| `RECALLD_EMBEDDING_PROVIDER` | `ollama` | Embedding provider (`ollama`, `openai`, `bedrock`) |
| `RECALLD_EMBEDDING_MODEL_NAME` | `embeddinggemma:300m` | Embedding model |
| `RECALLD_EMBEDDING_DIMENSIONS` | `768` | Embedding dimensions |
| `RECALLD_EMBEDDING_REGION` | `us-east-1` | AWS region (Bedrock only) |

All other `RECALLD_*` env vars from the [Configuration Reference](#3-configuration-reference) work in Docker too.

---

## 5. Using the CLI (`recalld-cli`)

The CLI client talks to a running recalld server over HTTP. Output is JSON by default (for LLM tool-use). Use `--format human` or `-F human` for colored tables.

The CLI's compiled default server URL is `http://localhost:7878`. The HTTP server's default port is `7680`. You must ensure these match -- either configure the CLI to point at the server's port, or start the server on the CLI's expected port. See [CLI Configuration](#cli-configuration) below.

Global flags:

- `-F, --format <json|human>` -- output format (overrides config)
- `--server <URL>` -- API server URL (overrides config and `$RECALLD_URL`)

### CLI Configuration

The CLI reads settings from `~/.recalld/config.toml`. These fields are under the top level (not inside a `[section]`):

```toml
# ~/.recalld/config.toml (CLI settings)
server_url = "http://localhost:7680"   # default: http://localhost:7878
default_namespace = "default"
default_format = "json"                # or "human"
```

Set `server_url` to match the HTTP server's bind address and port (default `http://127.0.0.1:7680`). CLI flags and the `$RECALLD_URL` environment variable override `server_url`.

### `store` -- Store a memory

```bash
# Basic store
recalld-cli store "Caleb prefers Rust for systems programming"

# With tags
recalld-cli store "Project deadline is March 15" --tags "project/acme,type/deadline"

# In a specific namespace
recalld-cli store "API uses OAuth2 bearer tokens" --namespace work-project

# With a parent memory
recalld-cli store "The auth endpoint is /api/v2/token" --parent-id a1b2c3d4-...
```

If the text exceeds 2,000 bytes, it becomes the `full_text` and the server generates a summary.

> **Note:** The CLI `store` command does not support `--entities`, `--topics`, `--emotions`, or `--supersedes` flags. These parameters are only available through the MCP `store_memory` tool.

### `recall` -- Search memories

```bash
# Basic search
recalld-cli recall "what programming languages does Caleb like"

# Limit results
recalld-cli recall "project deadlines" --limit 5

# Filter by namespace
recalld-cli recall "API authentication" --namespace work-project

# Filter by tags (must have ALL specified tags)
recalld-cli recall "preferences" --tags "type/user-profile"

# Include graph-connected memories (1-3 hops)
recalld-cli recall "deployment process" --depth 2

# Minimum strength threshold
recalld-cli recall "old decisions" --min-strength 0.5

# Include ghost-phase memories
recalld-cli recall "ancient history" --include-ghosts
```

### `get` -- Retrieve a memory by ID

```bash
recalld-cli get a1b2c3d4-e5f6-7890-abcd-ef1234567890
```

### `forget` -- Delete a memory

```bash
# Interactive (prompts for confirmation)
recalld-cli forget a1b2c3d4-e5f6-7890-abcd-ef1234567890

# Skip confirmation (for scripting)
recalld-cli forget a1b2c3d4-e5f6-7890-abcd-ef1234567890 --yes
```

### `reinforce` -- Strengthen a memory

```bash
recalld-cli reinforce a1b2c3d4-e5f6-7890-abcd-ef1234567890
```

Reinforcement increases stability so the memory decays more slowly. The CLI always uses the default quality rating of 3 (good). Quality ratings 1-4 are available through the MCP `reinforce_memory` tool.

### `inspect` -- Full debug view

```bash
recalld-cli inspect a1b2c3d4-e5f6-7890-abcd-ef1234567890
```

Shows stability, phase, graph connections, access history, and all metadata.

### `list` -- List memories with filtering

```bash
# List all (default: 50 results, newest first)
recalld-cli list

# Filter by namespace
recalld-cli list --namespace my-project

# Filter by phase
recalld-cli list --phase ghost

# Filter by tags
recalld-cli list --tags "type/user-profile,project/acme"

# Sort by strength (weakest first)
recalld-cli list --sort strength --order asc

# Pagination
recalld-cli list --limit 20 --offset 40
```

Sort fields: `created`, `accessed`, `strength`, `stability`. Sort orders: `asc`, `desc`.

### `namespaces` -- Manage namespaces

```bash
# List all namespaces
recalld-cli namespaces list

# Create a namespace
recalld-cli namespaces create my-project --dim 1536

# Create with custom initial stability
recalld-cli namespaces create experiments --dim 768 --initial-stability 1.0

# Show stats for a namespace
recalld-cli namespaces stats my-project

# Show stats for all namespaces
recalld-cli namespaces stats
```

### `sweep` -- Trigger a decay sweep

```bash
# Dry run (show what would change)
recalld-cli sweep --dry-run

# Run sweep
recalld-cli sweep

# Sweep a specific namespace
recalld-cli sweep --namespace my-project
```

### `export` -- Bulk export

```bash
# Export all as JSON array
recalld-cli export

# Export as JSONL (one record per line)
recalld-cli export --export-format jsonl

# Export a specific namespace
recalld-cli export --namespace my-project

# Include full text
recalld-cli export --include-text

# Include embeddings (large output)
recalld-cli export --include-embeddings
```

### `import` -- Bulk import

```bash
# Import from JSON file
recalld-cli import memories.json

# Import from JSONL
recalld-cli import memories.jsonl --import-format jsonl

# Import from stdin
cat memories.json | recalld-cli import -

# Dry run
recalld-cli import memories.json --dry-run

# Skip duplicates
recalld-cli import memories.json --skip-duplicates

# Continue on errors
recalld-cli import memories.json --continue-on-error

# Override namespace
recalld-cli import memories.json --namespace imported
```

> **Note:** The `--batch-size` flag is accepted but currently reserved for future use and does not affect import behavior.

### `status` -- System health

```bash
recalld-cli status
```

Shows counts per phase, cache stats, and uptime.

### `health` -- Health report

```bash
# Full health report
recalld-cli health

# For a specific namespace
recalld-cli health --namespace my-project

# Human-readable output
recalld-cli health --format human
```

Shows decay forecast, at-risk memories, and storage breakdown.

---

## 6. MCP Tools Reference

recalld exposes 10 MCP tools. These are available to any MCP client (Claude Code, Cursor, etc.) when recalld is configured as an MCP server.

### `store_memory`

Store a new memory.

| Parameter | Type | Required | Description |
|---|---|---|---|
| `summary` | string | Yes | Short description (max 2000 chars). |
| `fullText` | string | No | Detailed content (max 1 MB). Dropped as memory decays to ghost phase. |
| `tags` | string[] | No | Categorization tags, e.g. `["topic/rust", "type/observation"]`. Max 64. |
| `entities` | string[] | No | Named entities (people, places, orgs). Used for search indexing and graph linking. Max 32. |
| `topics` | string[] | No | Topic keywords, e.g. `["rust", "cooking"]`. Max 32. |
| `emotions` | string[] | No | Emotional tone, e.g. `["happy", "anxious"]`. Max 32. |
| `namespace` | string | No | Target namespace (default: `"default"`). |
| `parentId` | string | No | UUID of parent memory to create a hierarchical link. |
| `supersedes` | string | No | UUID of an older memory this one replaces. The old memory is deprioritized in search. |

### `store_memories`

Store multiple memories in a single call. Each item has the same schema as `store_memory`. Returns an array of results, one per input memory.

| Parameter | Type | Required | Description |
|---|---|---|---|
| `memories` | array | Yes | Array of memory objects (max 100 per call). Each object has the same fields as `store_memory`. |

### `recall_memories`

Search memories by semantic similarity.

| Parameter | Type | Required | Description |
|---|---|---|---|
| `query` | string | Yes | Natural language search query. |
| `limit` | integer | No | Maximum results (default: 10, max: 100). |
| `namespace` | string | No | Namespace to search (default: `"default"`). |
| `tags` | string[] | No | Only return memories with ALL of these tags. |
| `entities` | string[] | No | Filter to memories mentioning these entities. |
| `topics` | string[] | No | Filter to memories about these topics. |
| `emotions` | string[] | No | Filter to memories with these emotional tones. |
| `minStrength` | number | No | Minimum memory strength threshold (0.0-1.0). |
| `depth` | integer | No | Graph hops to include related memories (default: 0, max: 3). |
| `timeRangeStart` | integer or string | No | Lower bound: epoch ms (integer) or ISO 8601 string. Memories at or after this time get a relevance boost. |
| `timeRangeEnd` | integer or string | No | Upper bound: epoch ms (integer) or ISO 8601 string. Memories at or before this time get a relevance boost. |
| `compact` | boolean | No | If true (default), returns only id, summary, fullText, entities, and topics per memory. Set to false to include full metadata (tags, score, phase, strength, timestamps, related edges). |

### `get_memory`

Retrieve a specific memory by ID.

| Parameter | Type | Required | Description |
|---|---|---|---|
| `id` | string | Yes | Memory UUID. |

### `reinforce_memory`

Strengthen a memory so it decays more slowly.

| Parameter | Type | Required | Description |
|---|---|---|---|
| `id` | string | Yes | Memory UUID to reinforce. |
| `quality` | integer | No | Rating 1-4: 1=forgot (minimal stability growth), 2=hard, 3=good (default), 4=easy (strongest). |

### `forget_memory`

Permanently delete a memory.

| Parameter | Type | Required | Description |
|---|---|---|---|
| `id` | string | Yes | Memory UUID to delete. |

### `find_similar_memories`

Find memories semantically similar to a given memory, or scan a namespace for duplicate clusters. Two modes: `single` (default) finds memories similar to one id; `scan` detects duplicate clusters across a namespace.

| Parameter | Type | Required | Description |
|---|---|---|---|
| `mode` | string | No | `"single"` (default) or `"scan"`. |
| `id` | string | Single mode | Source memory UUID. Required for single mode. |
| `namespace` | string | Scan mode | Namespace to scan. Required for scan mode; defaults to session default for single mode. |
| `limit` | integer | No | Maximum results per source memory in single mode (default: 10, max: 100). |
| `minScore` | number | No | Minimum similarity threshold for single mode (0.0-1.0). |
| `threshold` | number | No | Similarity threshold for scan mode duplicate detection (0.0-1.0, default: 0.85). |
| `sameNamespace` | boolean | No | Restrict to same namespace in single mode (default: true). Ignored in scan mode. |

### `create_namespace`

Create a new memory namespace.

| Parameter | Type | Required | Description |
|---|---|---|---|
| `name` | string | Yes | Namespace name (alphanumeric, hyphens, underscores; 1-64 chars). |
| `embeddingDim` | integer | No | Embedding dimensions, fixed after creation. Defaults to the default namespace's dimensions. |
| `initialStability` | number | No | Starting stability in days for new memories (default: 3.7145). |
| `desiredRetention` | number | No | Target retention rate 0.0-1.0 (default: 0.9). |
| `decayRateMultiplier` | number | No | Per-namespace decay rate multiplier. 1.0 = normal, 2.0 = 2x slower, 0.0 = disabled. Omit to inherit global setting. |

### `namespace_stats`

Get statistics for a memory namespace including total memory count, phase breakdown (full/summary/ghost), permastore count, average strength, edge count, and vector storage size.

| Parameter | Type | Required | Description |
|---|---|---|---|
| `namespace` | string | No | Namespace name to get stats for (default: `"default"`). |

### `list_memories`

List memories in a namespace with pagination and optional filters. Unlike `recall_memories`, this does not require a search query -- it returns memories sorted by creation date (newest first).

| Parameter | Type | Required | Description |
|---|---|---|---|
| `namespace` | string | No | Namespace to list from (default: `"default"`). |
| `limit` | integer | No | Maximum results per page (default: 50, max: 200). |
| `offset` | integer | No | Number of results to skip for pagination (default: 0). |
| `tags` | string[] | No | Only return memories with ALL of these tags. |
| `entities` | string[] | No | Only return memories mentioning ALL of these entities. |
| `timeRangeStart` | integer or string | No | Lower bound: epoch ms (integer) or ISO 8601 string. Only memories created at or after this time are returned. |
| `timeRangeEnd` | integer or string | No | Upper bound: epoch ms (integer) or ISO 8601 string. Only memories created at or before this time are returned. |

---

## 7. Namespaces

Namespaces partition memories into separate spaces, each with its own embedding index and optional decay configuration.

### Why namespaces

- **Project isolation**: Keep work-project memories separate from personal ones.
- **Different embedding models**: Use different dimensions per namespace.
- **Different decay rates**: Critical knowledge can have slower decay; ephemeral notes faster.
- **Per-directory defaults**: A `.recalld.toml` in a project root auto-selects a namespace.

### Creating namespaces

Via MCP (from AI tool):

```json
{
  "name": "work-project",
  "embeddingDim": 1536,
  "initialStability": 3.7145,
  "desiredRetention": 0.9,
  "decayRateMultiplier": 2.0
}
```

Via CLI:

```bash
recalld-cli namespaces create work-project --dim 1536 --initial-stability 3.7145
```

### Per-directory namespace binding

Create `.recalld.toml` in your project root:

```toml
namespace = "work-project"
```

When recalld starts in MCP mode from that directory (or any subdirectory), it uses `work-project` as the default namespace. Memories stored without an explicit namespace go there.

### Namespace constraints

- Names: 1-64 characters, alphanumeric plus hyphens and underscores.
- Embedding dimensions are fixed after creation.
- The `default` namespace is created automatically on first use.

---

## 8. Memory Decay

recalld uses FSRS v4.5 (Free Spaced Repetition Scheduler) to model memory decay.

### How it works

Each memory has a **stability** value (in days) and a **retrievability** score (0.0-1.0). Retrievability decays over time according to:

```
R(t, S) = (1 + 19/81 * t/S) ^ (-0.5)
```

Where `t` is elapsed days and `S` is stability. New memories start with stability of 3.7145 days by default.

### Decay phases

As retrievability drops, memories transition through phases:

1. **Full** (R >= 0.7) -- complete memory with summary and full text.
2. **Summary** (0.3 <= R < 0.7) -- full text is dropped, only summary remains.
3. **Ghost** (0.05 <= R < 0.3) -- summary is also dropped. Only metadata and graph edges remain.
4. **Deleted** (R < 0.05) -- memory is removed.

Tombstone is a separate phase for user-deleted memories (via `forget_memory`). Graph edges are preserved for tombstones.

### Reinforcement

Each time a memory is reinforced, its stability increases. The FSRS algorithm calculates new stability based on the quality rating:

- **1 (forgot)**: Minimal stability growth (20% of normal increase).
- **2 (hard)**: Small stability increase.
- **3 (good)**: Normal stability increase (default).
- **4 (easy)**: Largest stability increase.

### Permastore

Memories with stability above 1,500 days are exempt from decay. They effectively never fade.

### Tuning decay

**Global multiplier** (`decay.decay_rate_multiplier` in config):

```toml
[decay]
# Slower decay (memories last 2x longer)
decay_rate_multiplier = 2.0

# Faster decay (memories fade 2x faster)
decay_rate_multiplier = 0.5

# Disable decay entirely
decay_rate_multiplier = 0.0
```

**Per-namespace multiplier** (set when creating a namespace via the `decayRateMultiplier` parameter).

**Sweep interval** (`decay.sweep_interval_hours`):

```toml
[decay]
# Check for phase transitions every 6 hours instead of 24
sweep_interval_hours = 6.0
```

**Phase thresholds**:

```toml
[decay.phase_thresholds]
# More aggressive: drop full text sooner
full_to_summary = 0.8

# Keep summaries longer
summary_to_ghost = 0.2
```

### Manual sweep

```bash
# See what would change without applying
recalld-cli sweep --dry-run

# Run decay sweep now
recalld-cli sweep
```

---

## 9. Backup and Restore

### Backup

```bash
# Back up to a directory (auto-generates timestamped filename)
recalld backup --destination /path/to/backups/

# Back up to a specific file
recalld backup --destination /path/to/backup.zip

# Back up a custom data directory
recalld backup --destination ./backup.zip --source-data-dir /custom/data

# Force backup even if files are locked (not recommended)
recalld backup --destination ./backup.zip --force
```

Stop the daemon before backing up for a clean snapshot:

```bash
recalld daemon stop
recalld backup --destination ./backup.zip
recalld daemon start
```

### What's backed up

The backup archive (zip) contains all files from the data directory:

- `meta.db` -- memory metadata (redb)
- `ns_<name>.dat` -- embedding vectors (one file per namespace, e.g. `ns_default.dat`)
- `fulltext.dat` -- full text content (append-only)
- `edges.db` -- relationship graph (redb)

### Restore

```bash
# Restore from backup (prompts for confirmation)
recalld restore --from /path/to/backup.zip

# Skip confirmation
recalld restore --from /path/to/backup.zip --force

# Don't try to stop the daemon (if managing it yourself)
recalld restore --from /path/to/backup.zip --no-stop-daemon
```

After restore:

```bash
recalld daemon start
recalld daemon status
```

---

## 10. Troubleshooting

### Embedding provider not reachable

**Symptom**: Storing or recalling memories fails with connection errors.

For OpenAI:
```bash
# Verify API key is set
echo $OPENAI_API_KEY

# Test connectivity
curl -s https://api.openai.com/v1/models \
  -H "Authorization: Bearer $OPENAI_API_KEY" | head -c 200
```

For Ollama:
```bash
# Check Ollama is running
curl http://localhost:11434/api/tags

# Make sure your model is pulled
ollama list
```

### Wrong embedding dimensions

**Symptom**: `dimension mismatch` errors when storing memories.

Embedding dimensions are fixed per namespace at creation time. If you change `embedding.dimensions` in the config after creating a namespace, new memories will fail.

Fix: Create a new namespace with the correct dimensions, or reset and re-embed:

```bash
# Check current namespace dimensions
recalld-cli namespaces stats

# Create namespace with correct dimensions
recalld-cli namespaces create new-ns --dim 768
```

### Data directory permissions

**Symptom**: `Permission denied` errors on startup.

```bash
# Check ownership
ls -la ~/.recalld/

# Fix permissions
chmod -R 755 ~/.recalld/
```

The data directory (default `~/.recalld/data`) must be readable and writable by the user running recalld.

### Daemon already running

**Symptom**: `daemon already running` on start.

```bash
# Check if it's actually running
recalld daemon status

# If stale socket, stop and restart
recalld daemon stop
recalld daemon start
```

If `daemon stop` fails, remove the stale socket manually:

```bash
rm ~/.recalld/socket
recalld daemon start
```

### CLI cannot connect to server

**Symptom**: `connection refused` from recalld-cli.

The CLI talks to the HTTP API server over HTTP, not the daemon's Unix socket. Make sure the HTTP server is running:

```bash
# Start the HTTP server (default: 127.0.0.1:7680)
recalld serve
```

The CLI's compiled default server URL is `http://localhost:7878`, but the HTTP server defaults to port `7680`. You must ensure they match. Either configure the CLI or pass the correct URL:

```bash
# Override on command line to match the server's default port
recalld-cli --server http://127.0.0.1:7680 status

# Or set environment variable
export RECALLD_URL=http://127.0.0.1:7680

# Or configure in ~/.recalld/config.toml
# server_url = "http://localhost:7680"
```

### High memory usage

The in-memory cache defaults to 1 GB max. Reduce it if needed:

```toml
[cache]
max_capacity_bytes = 268435456  # 256 MB
time_to_idle_secs = 1800       # 30 minutes
```

### Logs

Check daemon logs:

```bash
cat ~/.recalld/daemon.log
```

Increase log verbosity:

```toml
[log]
level = "debug"
```

Or via environment:

```bash
RECALLD_LOG_LEVEL=debug recalld serve
```
