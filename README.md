# recalld

recalld is an AI memory system written in Rust that gives language models persistent, long-term memory. It runs as an MCP server for AI coding tools, an HTTP API, a Unix-socket daemon, or a standalone CLI.

The core is built on three subsystems: FSRS v4.5 spaced repetition to model memory decay across phases (full, summary, ghost, tombstone), a graph layer with ACT-R spreading activation for associative recall, and a hybrid search pipeline combining vector similarity, full-text search, and graph expansion.

## Quick start

### 1. Install

```sh
curl -fsSL https://raw.githubusercontent.com/calebevans/recalld/main/install.sh | bash
```

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

Register recalld as an MCP server (global, available in all projects):

```sh
claude mcp add --scope user recalld -- recalld mcp
```

Or for a single project only:

```sh
claude mcp add --scope project recalld -- recalld mcp
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

Use the recalld MCP tools (`store_memory`, `recall_memories`, `get_memory`,
`reinforce_memory`, `forget_memory`, `find_similar_memories`) for persistent
memory across sessions.

## When to recall (proactive)

- At the START of every conversation, recall memories relevant to the current
  project or topic to establish context. Do not wait to be asked.
- Before making recommendations, check for past preferences or decisions.
- When the user references something from a previous conversation.

## When to store (proactive)

- User profile: role, expertise, preferences, communication style
- Feedback on your approach: what worked, what was corrected, and WHY
- Project context: architecture decisions, constraints, conventions not
  obvious from the code
- Important decisions and their rationale

IMPORTANT: Do not wait until the end of a conversation or until asked.
Store memories as they arise. After every significant exchange (a decision
is made, a preference is expressed, a project detail is learned, or a
recommendation is accepted/rejected), store immediately. If you are unsure
whether something is worth storing, store it. Memories that aren't
retrieved or reinforced will naturally decay over time.

Do NOT store: ephemeral task details, code snippets, or anything derivable
from the codebase.

## How to write good memories

- `summary`: Specific and searchable. Include names, dates, and key terms.
  Bad: "User prefers a certain style." Good: "User prefers early returns
  over nested match blocks in Rust."
- `full_text`: Provide for any memory where the summary loses nuance.
  Include reasoning, context, and direct quotes.
- `entities`: ALL people, projects, tools, and proper nouns. Use canonical
  names. These power the graph — missing entities means missing connections.
- `topics`: 1-5 lowercase keywords (e.g., "deployment", "testing").
- `tags`: Hierarchical — `type/feedback`, `type/project`, `project/<name>`,
  `tech/<name>`.
- `supersedes`: When correcting a memory, pass the old memory's ID here.

## When to reinforce

- Recalled memory was useful: reinforce with quality 3-4.
- Recalled memory was wrong: reinforce with quality 1 (weakens it), then
  store the corrected version with `supersedes`.

## Search strategy

- Simple factual lookup: single query, depth 1.
- Inference or combining facts: depth 2, search for underlying facts rather
  than the inference itself.
- Broad context: depth 2-3.
- Specific names or terms: include them in the query — full-text search
  excels at exact matching.
````

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

**MCP server** -- Runs as a Model Context Protocol server for AI tools like Claude Code. Exposes 9 tools: `store_memory`, `store_memories`, `recall_memories`, `get_memory`, `reinforce_memory`, `forget_memory`, `find_similar_memories`, `create_namespace`, `list_memories`.

```sh
recalld mcp
```

**HTTP API** -- Runs a standalone HTTP server (default `127.0.0.1:7680`).

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
