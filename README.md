# recalld

recalld is an AI memory system written in Rust that gives language models persistent, long-term memory. It runs as an MCP server for AI coding tools, an HTTP API, a Unix-socket daemon, or a standalone CLI.

The core is built on three subsystems: FSRS v4.5 spaced repetition to model memory decay across phases (full, summary, ghost, tombstone), a graph layer with ACT-R spreading activation for associative recall, and a hybrid search pipeline combining vector similarity, full-text search, and graph expansion.

## Quick start

### 1. Install

**Binary:**

```sh
curl -fsSL https://raw.githubusercontent.com/calebevans/recalld/main/install.sh | bash
```

**Docker / Podman:**

```sh
docker run -d -p 7680:7680 \
  -e RECALLD_EMBEDDING_BASE_URL=http://host.docker.internal:11434 \
  -v recalld-data:/data \
  ghcr.io/calebevans/recalld
```

Or with Docker Compose (includes Ollama):

```sh
docker compose up -d
docker compose exec ollama ollama pull embeddinggemma:300m
```

See [Docker deployment](#docker) for details.

### 2. Set up a local embedding model

Install [Ollama](https://ollama.com), then pull an embedding model:

```sh
ollama pull embeddinggemma:300m
```

Create `~/.recalld/config.toml`:

```toml
[embedding]
provider = "ollama"
model_name = "embeddinggemma:300m"
base_url = "http://localhost:11434"
dimensions = 768
```

See [docs/guide.md](docs/guide.md#2-embedding-setup) for OpenAI and other provider options.

### 3. Connect to Claude Code

**Option A: stdio transport** (recalld launches as a subprocess):

```sh
claude mcp add --scope user recalld -- recalld mcp
```

**Option B: HTTP transport** (connect to a running server or Docker container):

```sh
claude mcp add --scope user --transport http recalld http://localhost:7680/mcp
```

Then allow the MCP tools so Claude can use them without prompting each time. Add to your `~/.claude/settings.local.json` (global) or project `.claude/settings.local.json`:

```json
{
  "permissions": {
    "allow": [
      "mcp__recalld__store_memory",
      "mcp__recalld__store_memories",
      "mcp__recalld__recall_memories",
      "mcp__recalld__get_memory",
      "mcp__recalld__reinforce_memory",
      "mcp__recalld__forget_memory",
      "mcp__recalld__find_similar_memories",
      "mcp__recalld__create_namespace",
      "mcp__recalld__list_memories"
    ]
  }
}
```

### 4. Add memory instructions to your prompt

Add the following to your `CLAUDE.md` (or equivalent prompt file) so your AI assistant uses recalld proactively. A minimal version is shown here; see [docs/mcp.md](docs/mcp.md#ready-to-use-prompt-block) for the full prompt with detailed guidance.

````markdown
# Memory

Use recalld MCP tools for persistent memory across sessions. Recall at
conversation start and store as things happen — don't wait to be asked.

**Recall** when: starting a conversation, making recommendations, or the
user references a prior session. **Store** when: a decision is made, a
preference is expressed, feedback is given, or project context is learned.
When unsure, store it — memories decay naturally if unused.

Do NOT store: ephemeral task details, code snippets, or anything derivable
from the codebase.

**Writing good memories:** `summary` should be specific and searchable
(include names, dates, key terms). Always include `entities` (canonical
names), `topics` (1-5 keywords), and `tags` (hierarchical:
`type/feedback`, `project/<name>`, `tech/<name>`). Use `supersedes` when
correcting an existing memory.

**Reinforce** useful memories (quality 3-4). Weaken wrong ones (quality 1)
and store a corrected version.
````

### 5. Improve memory usage with hooks (optional)

Prompt instructions alone may not be enough to get consistent memory usage. Claude Code [hooks](https://docs.anthropic.com/en/docs/claude-code/hooks) can inject gentle reminders on every turn so your assistant doesn't forget that memory tools are available.

Add to your `~/.claude/settings.json`:

```json
{
  "hooks": {
    "UserPromptSubmit": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "echo 'Could any stored memories be useful here? recalld tools: recall_memories, store_memory, reinforce_memory.'"
          }
        ]
      }
    ],
    "Stop": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "echo 'Anything from this exchange worth remembering for next time? (store_memory)'"
          }
        ]
      }
    ]
  }
}
```

These hooks are suggestive, not mandatory -- the model decides whether memories are relevant on each turn. The `UserPromptSubmit` hook nudges recall before responding, and the `Stop` hook nudges storage after finishing a task.

## Features

- **Spaced repetition decay** -- FSRS v4.5 governs retrievability over time; memories transition through full, summary, and ghost phases based on retrievability thresholds. Explicit deletion moves memories to tombstone.
- **Graph relationships** -- 7 edge types (parent/child, associative, causal, contradicts, entity, temporal, supersedes) with automatic linking based on similarity
- **Hybrid search** -- SIMD-accelerated vector similarity, FTS5 full-text search, and graph expansion with score fusion
- **Namespaces** -- isolated embedding spaces with independent decay configuration
- **Retrieval-induced forgetting** -- accessing one memory suppresses competing memories
- **Permastore** -- memories with stability above 1500 days are exempt from decay
- **Backup and restore** -- full data export and import

## Benchmark

recalld is evaluated on the [LoCoMo](https://aclanthology.org/2024.acl-long.747/) benchmark (1,986 questions across 5 categories including adversarial). All results use a single prompt for all categories, with no category routing or per-category prompt selection.

| Model | Accuracy | Categories |
|-------|----------|-----------|
| Claude Sonnet 4.6 | 84.5% | All 5 (including adversarial) |
| Gemini 2.5 Flash | 78.3% | All 5 (including adversarial) |

See [docs/benchmark.md](docs/benchmark.md) for full methodology, per-category breakdowns, and reproducibility instructions.

## Usage modes

**MCP server (stdio)** -- Runs as a Model Context Protocol server for AI tools like Claude Code. Exposes 9 tools: `store_memory`, `store_memories`, `recall_memories`, `get_memory`, `reinforce_memory`, `forget_memory`, `find_similar_memories`, `create_namespace`, `list_memories`.

```sh
recalld mcp
```

**HTTP API** -- Runs a standalone HTTP server (default `127.0.0.1:7680`). Also exposes an MCP endpoint at `/mcp` using the streamable HTTP transport, so MCP clients can connect via URL.

```sh
recalld serve
```

**Daemon** -- Runs in the background with a Unix socket at `~/.recalld/socket`, using JSON-RPC 2.0. Auto-shuts down after 30 minutes of idle time.

```sh
recalld daemon
```

**CLI client** -- `recalld-cli` communicates with a running HTTP API server.

```sh
recalld-cli store "The deployment uses Kubernetes with Helm charts"
recalld-cli recall "deployment infrastructure"
recalld-cli status
```

Available CLI commands: `store`, `recall`, `get`, `forget`, `reinforce`, `inspect`, `namespaces`, `sweep`, `status`, `export`, `import`, `list`, `health`.

## Configuration

recalld reads configuration from `recalld.toml` in the working directory or `~/.recalld/config.toml`. Per-directory overrides use `.recalld.toml` (found by walking up from the current directory), which must include a `namespace` field and can override any config section.

```toml
[embedding]
provider = "ollama"          # ollama, openai, or passthrough
model_name = "embeddinggemma:300m"
dimensions = 768

[decay]
sweep_interval_hours = 24.0

[storage]
data_dir = "~/.recalld/data"

[server]
bind_address = "127.0.0.1"
port = 7680

[graph]
auto_link_threshold = 0.50
max_auto_links = 15

[rif]
enabled = true
max_suppression = 0.15
```

Additional sections: `[cache]`, `[log]`.

See [docs/guide.md](docs/guide.md) for the full configuration reference, [docs/architecture.md](docs/architecture.md) for design details, and [docs/mcp.md](docs/mcp.md) for MCP integration including a ready-to-use prompt block for your `CLAUDE.md` or system prompt.

## Docker

Official images are published to GHCR at `ghcr.io/calebevans/recalld` for `linux/amd64` and `linux/arm64`. All configuration is done via environment variables (no config file needed).

### With your host's Ollama

```sh
docker run -d -p 7680:7680 \
  -e RECALLD_EMBEDDING_BASE_URL=http://host.docker.internal:11434 \
  -v recalld-data:/data \
  ghcr.io/calebevans/recalld
```

On Linux with Podman, add `--add-host=host.docker.internal:host-gateway`.

### With Docker Compose

The included `compose.yaml` runs recalld alongside Ollama:

```sh
docker compose up -d
docker compose exec ollama ollama pull embeddinggemma:300m
```

Then connect Claude Code to the MCP endpoint:

```sh
claude mcp add --scope user --transport http recalld http://localhost:7680/mcp
```

### Podman

The image and `compose.yaml` are compatible with Podman:

```sh
podman run -d -p 7680:7680 \
  -e RECALLD_EMBEDDING_BASE_URL=http://host.docker.internal:11434 \
  --add-host=host.docker.internal:host-gateway \
  -v recalld-data:/data \
  ghcr.io/calebevans/recalld
```

Or with Podman Compose:

```sh
podman compose up -d
podman compose exec ollama ollama pull embeddinggemma:300m
```

## Building from source

Requires Rust 1.87 or later.

```sh
git clone https://github.com/calebevans/recalld.git
cd recalld
make build          # debug build
make release        # optimized build
make install        # install to ~/.cargo/bin
make test           # run tests
make lint           # fmt check + clippy
```

Or directly with Cargo:

```sh
cargo build --release
cargo install --path .
```

## Supported platforms

- macOS (x86_64, aarch64)
- Linux (x86_64, aarch64)

Windows is not supported.

## License
AGPL-3.0
