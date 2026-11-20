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

`delete_namespace` is deliberately left out of the allowlist below. It
destroys every memory in a namespace and removes its vector file from
disk, with no undo and no backup — the one tool worth a confirmation
prompt each time. Add it only if you want that prompt gone.

```json
{
  "permissions": {
    "allow": [
      "mcp__recalld__get_session",
      "mcp__recalld__store_memory",
      "mcp__recalld__store_memories",
      "mcp__recalld__recall_memories",
      "mcp__recalld__get_memory",
      "mcp__recalld__reinforce_memory",
      "mcp__recalld__forget_memory",
      "mcp__recalld__forget_memories",
      "mcp__recalld__find_similar_memories",
      "mcp__recalld__create_namespace",
      "mcp__recalld__namespace_stats",
      "mcp__recalld__list_memories",
      "mcp__recalld__list_namespaces",
      "mcp__recalld__list_tags"
    ]
  }
}
```

### 4. Add memory instructions to your prompt

Add the following to your `CLAUDE.md` (or equivalent prompt file) so your AI assistant uses recalld proactively. A minimal version is shown here; see [docs/mcp.md](docs/mcp.md#ready-to-use-prompt-block) for the full prompt with detailed guidance.

````markdown
# Memory

Start with `get_session` (fallback: `recalld://session`) to check the connection's
namespace and access defaults, then `list_namespaces` to see what exists; never
assume the list. Reuse existing namespaces: `default` for shared
preferences and cross-project facts, `project-<name>` for project-specific
knowledge, and optionally `topic-<name>` for useful general topics. A project
default does not make every fact project-local. Namespaces are not access controls.

Recall relevant context from `default` and the current project or relevant topic
explicitly; don't search every unrelated namespace. Store durable decisions,
preferences, feedback, and context when useful, not every turn. Avoid ephemeral
task details, facts derivable from source, duplicate copies across stores, and
namespaces per agent or session.

Write specific, searchable summaries with canonical `entities`, a few `topics`,
and meaningful `tags`. Use same-namespace `supersedes` for a real correction.
Reinforce only after deliberately judging usefulness; don't delete a memory
just because a similarity warning calls it a possible duplicate.

Single-record by-ID tools address the record, not the session default; an explicit namespace
asserts its scope. Retrieval records access by default; use `recordAccess:false`
for inspection. After an unclear write, preserve the response and IDs and inspect
before acting: no blind replay or automatic duplicate deletion. Only resume a
durable batch with its documented receipt/key and unchanged full payload.
````

### 5. Improve memory usage with hooks (optional)

Prompt instructions alone may not be enough to get consistent memory usage. Claude Code [hooks](https://code.claude.com/docs/en/hooks) can inject gentle reminders so your assistant checks which namespaces exist when a session starts and doesn't forget that memory tools are available on each turn.

Add to your `~/.claude/settings.json`:

```json
{
  "hooks": {
    "SessionStart": [
      {
        "matcher": "startup|resume",
        "hooks": [
          {
            "type": "command",
            "command": "echo 'Start with get_session, then list_namespaces. Use default for shared facts and the existing project namespace that matches this repository for project knowledge; create one only when none fits.'"
          }
        ]
      }
    ],
    "UserPromptSubmit": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "echo 'Would shared or current-project memory help this task? Recall only relevant context; store durable learning and give feedback only when warranted. Select the appropriate namespace explicitly.'"
          }
        ]
      }
    ]
  }
}
```

This hook is suggestive, not mandatory -- the model decides whether memories are relevant on each turn. It nudges recall before responding without adding another model pass after the task is finished. Keep its output static and trusted: never interpolate user or recalled content into shell commands or injected instructions, and do not execute memory writes from the hook.

## Upgrading to 2.0

Recalld 2.0 creates new databases in the native schema and automatically upgrades supported older databases at startup. It backs up the stopped database, migrates a separate copy, and commits only after runtime initialization succeeds. Allow extra disk space and retain the pre-upgrade backup for rollback to an older binary. See [upgrade and recovery requirements](docs/guide.md#upgrading-to-20), including automatic layout conversion for existing Docker volumes.

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

**MCP server (stdio)** -- Runs as a Model Context Protocol server for AI tools like Claude Code. Exposes 16 tools: `get_session`, `store_memory`, `store_memories`, `recall_memories`, `get_memory`, `reinforce_memory`, `forget_memory`, `forget_memories`, `find_similar_memories`, `create_namespace`, `delete_namespace`, `namespace_stats`, `list_memories`, `list_namespaces`, `list_tags`, `move_memories`.

```sh
recalld mcp
```

**HTTP API** -- Runs a standalone HTTP server (default `127.0.0.1:7680`). Also exposes an MCP endpoint at `/mcp`, so MCP clients can connect via URL. Both MCP transports are dual-era: stateless MCP 2026-07-28 and the legacy `initialize`/`Mcp-Session-Id` handshake (2025-06-18 and earlier) are served on the same endpoint, chosen per message.

```sh
recalld serve
```

**Daemon** -- Runs in the background with a Unix socket at `$XDG_RUNTIME_DIR/recalld/recalld.sock` on Linux, or `~/.recalld/recalld.sock` where there is no runtime directory (macOS), using JSON-RPC 2.0. It keeps running until stopped (`recalld daemon stop`); `recalld daemon start --idle-timeout <minutes>` adds an idle shutdown.

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

**Local web UI** -- `recalld ui` opens a browser UI over your memories: a Memory map that lights up as agents recall, an Explore table, a Timeline and a Health view, with Search on `⌘K`. It comes with the standard install, is read-only by default (browsing never reinforces or decays anything) and only listens on `127.0.0.1`. Turn it off with `[ui] enabled = false` in the config (or `RECALLD_UI_ENABLED=false`); the Docker image is built without it. See [docs/ui.md](docs/ui.md).

```sh
recalld ui            # starts the daemon if needed and opens the browser
```

## Configuration

recalld reads configuration from `recalld.toml` in the working directory or `~/.recalld/config.toml`. Per-directory overrides use `.recalld.toml` (found by walking up from the current directory), which must include a `namespace` field and can override any config section.

```toml
[embedding]
provider = "ollama"          # ollama, openai, bedrock, or passthrough
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

Additional sections: `[cache]`, `[log]`, `[ui]`.

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

Requires Rust 1.94 or later.

```sh
git clone https://github.com/calebevans/recalld.git
cd recalld
make build          # debug build
make release        # optimized build, with the web UI (needs Node 22.17+ and pnpm)
make install        # install to ~/.cargo/bin, with the web UI
make test           # run tests
make lint           # fmt check + clippy
```

Or directly with Cargo (build the UI first with `make ui`, or `recalld ui` shows a page saying it wasn't built; `--no-default-features` leaves the UI out entirely):

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
