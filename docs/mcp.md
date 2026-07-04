# MCP Integration Guide

## Setup

### Claude Code

Two transport options: stdio (recalld launches as a subprocess) or HTTP (connect to a running server or Docker container).

**Stdio transport** (default, launches recalld as a subprocess):

```sh
# Global (available in all projects)
claude mcp add --scope user recalld -- recalld mcp

# Or project-only
claude mcp add --scope project recalld -- recalld mcp
```

If recalld is not on your PATH, use the full path:

```sh
claude mcp add --scope user recalld -- /path/to/recalld mcp
```

**HTTP transport** (connect to a running `recalld serve` instance or Docker container):

```sh
claude mcp add --scope user --transport http recalld http://localhost:7680/mcp
```

Use the HTTP transport when running recalld in Docker, on a remote server, or as a shared service.

#### Allowing MCP tool permissions

By default, Claude Code will prompt you for approval each time an MCP tool is called. To allow recalld tools automatically, add them to your `~/.claude/settings.local.json` (global) or project `.claude/settings.local.json`:

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

The `permissions.allow` array merges across global and project-level settings files, so you can allow the MCP tools globally and they'll work in every project.

To set a custom default namespace for a project, create a `.recalld.toml` file in the project root:

```toml
namespace = "my-project"
```

When recalld starts in MCP mode, it walks up from the current working directory to find the nearest `.recalld.toml`. The `namespace` field sets the default namespace for all MCP operations in that directory tree. If no `.recalld.toml` is found, the default namespace is `"default"`.

### Other MCP-compatible tools

Any MCP client can use recalld via either transport.

**Stdio transport** (client launches recalld as a subprocess):

```json
{
  "command": "recalld",
  "args": ["mcp"],
  "transport": "stdio"
}
```

Optional: add `--log-level <level>` to `args` for debug logging (logs go to stderr).

**HTTP transport** (connect to a running server):

```json
{
  "url": "http://localhost:7680/mcp"
}
```

The HTTP endpoint is available whenever `recalld serve` is running. It implements the MCP streamable HTTP transport with session management via the `Mcp-Session-Id` header.

Configure per-project namespace defaults in a `.recalld.toml` file (see above), not CLI flags.

## Available tools

### store_memory

Store a new observation, fact, or piece of context. The system automatically generates an embedding for semantic search. Memories decay over time unless reinforced.

#### Parameters

| Parameter | Type | Required | Default | Description |
|-----------|------|----------|---------|-------------|
| `summary` | string | yes | -- | Short description (max 2000 chars) |
| `fullText` | string | no | -- | Detailed content. Dropped when memory decays to ghost phase. Max 1 MB. |
| `tags` | string[] | no | `[]` | Categorization tags, e.g. `["topic/rust", "type/observation"]`. Max 64. |
| `entities` | string[] | no | `[]` | Named entities (people, places, orgs). Used for search indexing and graph linking. Max 32. |
| `topics` | string[] | no | `[]` | Topic keywords, e.g. `["rust", "cooking"]`. Max 32. |
| `emotions` | string[] | no | `[]` | Emotional tone, e.g. `["happy", "anxious"]`. Max 32. |
| `namespace` | string | no | `"default"` | Memory partition |
| `parentId` | string | no | -- | UUID of parent memory for hierarchical linking |
| `supersedes` | string | no | -- | UUID of an older memory this one replaces. The old memory is deprioritized in search. |

#### Example

Request:

```json
{
  "summary": "User prefers snake_case for Rust code and camelCase for TypeScript",
  "tags": ["type/user-profile", "tech/rust", "tech/typescript"],
  "topics": ["coding-style"]
}
```

Response:

```json
{
  "id": "a1b2c3d4-e5f6-7890-abcd-ef1234567890",
  "namespace": "default",
  "phase": "Full",
  "strength": 1.0,
  "stability": 3.7145,
  "createdAt": 1719187200000,
  "createdAtFormatted": "2025-06-24 00:00:00 UTC"
}
```

---

### store_memories

Store multiple memories in a single call. Each item has the same schema as `store_memory`. Returns an array of results, one per input memory.

#### Parameters

| Parameter | Type | Required | Default | Description |
|-----------|------|----------|---------|-------------|
| `memories` | array | yes | -- | Array of memory objects (max 100 per call). Each object has the same fields as `store_memory`. |

Each object in the `memories` array accepts:

| Field | Type | Required | Default | Description |
|-------|------|----------|---------|-------------|
| `summary` | string | yes | -- | Short description (max 2000 chars) |
| `fullText` | string | no | -- | Detailed content. Max 1 MB. |
| `tags` | string[] | no | `[]` | Categorization tags. Max 64. |
| `entities` | string[] | no | `[]` | Named entities. Max 32. |
| `topics` | string[] | no | `[]` | Topic keywords. Max 32. |
| `emotions` | string[] | no | `[]` | Emotional tone. Max 32. |
| `namespace` | string | no | `"default"` | Memory partition |
| `parentId` | string | no | -- | UUID of parent memory |
| `supersedes` | string | no | -- | UUID of an older memory this one replaces |

#### Example

Request:

```json
{
  "memories": [
    {
      "summary": "User prefers early returns over nested match blocks in Rust",
      "tags": ["type/user-profile", "tech/rust"],
      "topics": ["code-style"]
    },
    {
      "summary": "Project uses PostgreSQL 15 with pgvector extension",
      "tags": ["type/project", "tech/postgresql"],
      "entities": ["PostgreSQL", "pgvector"],
      "topics": ["database"]
    }
  ]
}
```

Response:

```json
{
  "results": [
    {
      "index": 0,
      "id": "a1b2c3d4-e5f6-7890-abcd-ef1234567890",
      "namespace": "default",
      "phase": "Full",
      "strength": 1.0,
      "stability": 3.7145,
      "createdAt": 1719187200000
    },
    {
      "index": 1,
      "id": "b2c3d4e5-f6a7-8901-bcde-f12345678901",
      "namespace": "default",
      "phase": "Full",
      "strength": 1.0,
      "stability": 3.7145,
      "createdAt": 1719187200000
    }
  ],
  "total": 2,
  "stored": 2,
  "errors": 0
}
```

If individual memories fail validation (e.g., missing summary), their entry in the `results` array contains an `error` field instead of an `id`. Other memories in the batch are still stored.

---

### recall_memories

Search memories using natural language. Results are ranked by semantic similarity and memory strength.

#### Parameters

| Parameter | Type | Required | Default | Description |
|-----------|------|----------|---------|-------------|
| `query` | string | yes | -- | Natural language search query |
| `limit` | integer | no | `10` | Maximum results (1-100) |
| `namespace` | string | no | `"default"` | Which namespace to search |
| `tags` | string[] | no | `[]` | Only return memories with ALL of these tags |
| `entities` | string[] | no | `[]` | Filter to memories mentioning these entities |
| `topics` | string[] | no | `[]` | Filter to memories about these topics |
| `emotions` | string[] | no | `[]` | Filter to memories with these emotional tones |
| `minStrength` | number | no | -- | Minimum memory strength threshold (0.0-1.0) |
| `depth` | integer | no | `0` | Graph hops to include related memories (0-3) |
| `timeRangeStart` | integer or string | no | -- | Memories created at or after this time get a relevance boost (soft ranking signal, not a hard filter). Epoch millis or ISO 8601 string |
| `timeRangeEnd` | integer or string | no | -- | Memories created at or before this time get a relevance boost (soft ranking signal, not a hard filter). Epoch millis or ISO 8601 string |
| `compact` | boolean | no | `true` | If true, returns only `id`, `summary`, `fullText`, `entities`, and `topics` per memory for token efficiency. Set to false to include full metadata (tags, score, phase, strength, timestamps, related edges) and graph neighbor context. |

#### Example

Request:

```json
{
  "query": "user's coding style preferences",
  "limit": 5,
  "depth": 1
}
```

Response (compact mode, the default):

```json
{
  "memories": [
    {
      "id": "a1b2c3d4-e5f6-7890-abcd-ef1234567890",
      "summary": "User prefers snake_case for Rust code and camelCase for TypeScript",
      "entities": [],
      "topics": ["coding-style"]
    }
  ],
  "count": 1
}
```

Response (compact=false):

```json
{
  "memories": [
    {
      "id": "a1b2c3d4-e5f6-7890-abcd-ef1234567890",
      "summary": "User prefers snake_case for Rust code and camelCase for TypeScript",
      "score": 0.87,
      "namespace": "default",
      "tags": ["type/user-profile", "tech/rust", "tech/typescript"],
      "topics": ["coding-style"],
      "phase": "Full",
      "strength": 0.95,
      "createdAt": 1719187200000,
      "createdAtFormatted": "2025-06-24 00:00:00 UTC",
      "lastAccessedAt": 1719273600000,
      "lastAccessedAtFormatted": "2025-06-25 00:00:00 UTC"
    }
  ],
  "count": 1,
  "graphContext": {
    "neighbors": [
      {
        "id": "b2c3d4e5-f6a7-8901-bcde-f12345678901",
        "summary": "Team uses rustfmt with default settings",
        "namespace": "default",
        "tags": ["type/project", "tech/rust"],
        "edgeType": "Associative",
        "weight": 0.72,
        "connectedTo": "a1b2c3d4-e5f6-7890-abcd-ef1234567890"
      }
    ],
    "neighborCount": 1
  }
}
```

The `graphContext` field only appears when `compact=false`, `depth > 0`, and related memories are found.

---

### get_memory

Retrieve a specific memory by its ID.

#### Parameters

| Parameter | Type | Required | Default | Description |
|-----------|------|----------|---------|-------------|
| `id` | string | yes | -- | Memory UUID |

#### Example

Request:

```json
{
  "id": "a1b2c3d4-e5f6-7890-abcd-ef1234567890"
}
```

Response:

```json
{
  "id": "a1b2c3d4-e5f6-7890-abcd-ef1234567890",
  "namespace": "default",
  "summary": "User prefers snake_case for Rust code and camelCase for TypeScript",
  "fullText": null,
  "tags": ["type/user-profile", "tech/rust", "tech/typescript"],
  "phase": "Full",
  "strength": 0.95,
  "stability": 12.4,
  "createdAt": 1719187200000,
  "createdAtFormatted": "2025-06-24 00:00:00 UTC",
  "lastAccessedAt": 1719273600000,
  "lastAccessedAtFormatted": "2025-06-25 00:00:00 UTC",
  "isPermastore": false,
  "edgeCount": 3
}
```

---

### reinforce_memory

Strengthen a memory. Increases its stability so it decays more slowly.

#### Parameters

| Parameter | Type | Required | Default | Description |
|-----------|------|----------|---------|-------------|
| `id` | string | yes | -- | Memory UUID to reinforce |
| `quality` | integer | no | `3` | Rating 1-4: 1=forgot (weakens), 2=hard, 3=good, 4=easy |

Quality guide:
- **1 (forgot):** The memory was wrong or unhelpful. Weakens it, accelerating decay.
- **2 (hard):** The memory was partially useful but took effort to apply.
- **3 (good):** The memory was accurate and useful. Default.
- **4 (easy):** The memory was immediately and obviously relevant. Strongest reinforcement.

#### Example

Request:

```json
{
  "id": "a1b2c3d4-e5f6-7890-abcd-ef1234567890",
  "quality": 4
}
```

Response:

```json
{
  "id": "a1b2c3d4-e5f6-7890-abcd-ef1234567890",
  "strength": 0.98,
  "stability": 28.7,
  "phase": "Full",
  "isPermastore": false
}
```

---

### forget_memory

Permanently delete a memory. Use for incorrect or outdated information that should be removed immediately rather than allowed to decay. The memory transitions to Tombstone phase (graph edges are preserved).

#### Parameters

| Parameter | Type | Required | Default | Description |
|-----------|------|----------|---------|-------------|
| `id` | string | yes | -- | Memory UUID to delete |

#### Example

Request:

```json
{
  "id": "a1b2c3d4-e5f6-7890-abcd-ef1234567890"
}
```

Response:

```json
{
  "id": "a1b2c3d4-e5f6-7890-abcd-ef1234567890",
  "deleted": true
}
```

---

### find_similar_memories

Find memories semantically similar to a given memory, or scan a namespace for duplicate clusters. Two modes:

- **single** (default): requires an `id`, finds memories similar to it.
- **scan**: requires a `namespace`, detects clusters of near-duplicate memories across the namespace.

#### Parameters

| Parameter | Type | Required | Default | Description |
|-----------|------|----------|---------|-------------|
| `mode` | string | no | `"single"` | `"single"` or `"scan"` |
| `id` | string | single mode | -- | Source memory UUID (required for single mode) |
| `namespace` | string | scan mode | session default | Namespace to scan (required for scan mode; defaults to session default in single mode) |
| `limit` | integer | no | `10` | Maximum results per source memory in single mode (1-100) |
| `minScore` | number | no | -- | Minimum similarity threshold for single mode (0.0-1.0) |
| `threshold` | number | no | `0.85` | Similarity threshold for scan mode duplicate detection (0.0-1.0) |
| `sameNamespace` | boolean | no | `true` | Restrict to same namespace in single mode. Ignored in scan mode. |

#### Example (single mode)

Request:

```json
{
  "id": "a1b2c3d4-e5f6-7890-abcd-ef1234567890",
  "limit": 3,
  "minScore": 0.7
}
```

Response:

```json
{
  "sourceId": "a1b2c3d4-e5f6-7890-abcd-ef1234567890",
  "memories": [
    {
      "id": "c3d4e5f6-a7b8-9012-cdef-123456789012",
      "summary": "Team coding standards document requires snake_case in all Rust modules",
      "score": 0.82,
      "namespace": "default",
      "tags": ["type/project", "tech/rust"],
      "phase": "Full",
      "strength": 0.88,
      "createdAt": 1719100800000,
      "lastAccessedAt": 1719187200000
    }
  ],
  "count": 1
}
```

#### Example (scan mode)

Request:

```json
{
  "mode": "scan",
  "namespace": "default",
  "threshold": 0.9
}
```

Response:

```json
{
  "namespace": "default",
  "threshold": 0.9,
  "clusters": [
    [
      {
        "id": "a1b2c3d4-...",
        "summary": "User prefers snake_case in Rust"
      },
      {
        "id": "c3d4e5f6-...",
        "summary": "User likes snake_case for Rust code"
      }
    ]
  ],
  "clusterCount": 1
}
```

---

### create_namespace

Create a new memory namespace. Each namespace has its own embedding space and decay configuration. Embedding dimensions are fixed after creation.

#### Parameters

| Parameter | Type | Required | Default | Description |
|-----------|------|----------|---------|-------------|
| `name` | string | yes | -- | Alphanumeric, hyphens, underscores; 1-64 chars |
| `embeddingDim` | integer | no | *inherited* | Embedding dimensions. Defaults to the `default` namespace's dimensions. Fixed after creation. |
| `initialStability` | number | no | `3.7145` | Starting stability in days for new memories |
| `desiredRetention` | number | no | `0.9` | Target retention rate (0.0-1.0) |
| `decayRateMultiplier` | number | no | *inherited* | 1.0 = normal, 2.0 = 2x slower decay, 0.5 = 2x faster, 0.0 = disabled |

#### Example

Request:

```json
{
  "name": "work-project",
  "desiredRetention": 0.95,
  "decayRateMultiplier": 2.0
}
```

Response:

```json
{
  "id": 2,
  "name": "work-project",
  "embeddingDim": 1536,
  "memoryCount": 0,
  "createdAt": 1719187200000,
  "createdAtFormatted": "2025-06-24 00:00:00 UTC"
}
```

---

### list_memories

List memories in a namespace with pagination and optional filters. Unlike `recall_memories`, this does not require a search query -- it returns memories sorted by creation date (newest first). Use for browsing, auditing, or enumerating memories.

#### Parameters

| Parameter | Type | Required | Default | Description |
|-----------|------|----------|---------|-------------|
| `namespace` | string | no | `"default"` | Which namespace to list from |
| `limit` | integer | no | `50` | Maximum results per page (1-200) |
| `offset` | integer | no | `0` | Number of results to skip for pagination |
| `tags` | string[] | no | `[]` | Only return memories with ALL of these tags |
| `entities` | string[] | no | `[]` | Only return memories mentioning ALL of these entities |
| `timeRangeStart` | integer or string | no | -- | Lower bound: epoch millis or ISO 8601 string |
| `timeRangeEnd` | integer or string | no | -- | Upper bound: epoch millis or ISO 8601 string |

#### Example

Request:

```json
{
  "namespace": "default",
  "limit": 10,
  "tags": ["type/user-profile"]
}
```

Response:

```json
{
  "memories": [
    {
      "id": "a1b2c3d4-e5f6-7890-abcd-ef1234567890",
      "summary": "User prefers snake_case for Rust code and camelCase for TypeScript",
      "namespace": "default",
      "tags": ["type/user-profile", "tech/rust", "tech/typescript"],
      "phase": "Full",
      "strength": 0.95,
      "createdAt": 1719187200000,
      "createdAtFormatted": "2025-06-24 00:00:00 UTC"
    }
  ],
  "count": 1,
  "limit": 10,
  "offset": 0
}
```

## Best practices

### What to store

Store durable knowledge that will be useful across sessions:

- User preferences (coding style, communication style, tool choices)
- Project context (architecture decisions, tech stack, conventions)
- Team information (roles, responsibilities, org structure)
- Decisions and their rationale
- Feedback on your approach (what worked, what the user corrected)

Do not store:

- Ephemeral task details ("fix the bug on line 42")
- Code snippets that can be read from the codebase
- Information derivable from project files (package.json contents, etc.)
- Transient state (current branch, open PR numbers)

### Using tags effectively

Use hierarchical tags for consistent categorization:

```
type/user-profile    -- personal preferences, background
type/project         -- project-level context
type/feedback        -- user corrections and preferences
type/decision        -- architectural or design decisions
type/people          -- information about team members
type/achievement     -- milestones reached

project/recalld      -- project-specific
org/acme-corp        -- organization-specific
team/backend         -- team-specific

tech/rust            -- technology-specific
tech/kubernetes
tech/react
```

Filter with tags in recall to narrow results:

```json
{
  "query": "deployment process",
  "tags": ["project/recalld"]
}
```

### Namespace strategies

**Per-project namespaces** -- isolate memories for unrelated projects:

```json
{"name": "work-api", "desiredRetention": 0.95}
{"name": "personal-blog", "desiredRetention": 0.8}
```

**Per-domain namespaces** -- group by knowledge area:

```json
{"name": "coding-preferences"}
{"name": "architecture-decisions"}
```

**Single default namespace** -- works well with fewer than a few hundred memories or when you want cross-project recall.

Use `decayRateMultiplier` to tune per namespace:
- Critical reference material: `2.0` (slower decay)
- Experimental notes: `0.5` (faster decay)
- Permanent reference: `0.0` (no decay)

### Reinforcement patterns

Reinforce memories when they prove useful:

```json
{"id": "<uuid>", "quality": 4}
```

- **After successful recall:** If you searched for something and the result was exactly what you needed, reinforce with quality 3 or 4.
- **After applying context:** If a recalled memory helped you give better advice, reinforce it.
- **Correct wrong memories:** If a recalled memory was inaccurate, either reinforce with quality 1 (weakens it) or use `forget_memory` to delete it outright, then store the corrected version.
- **Use `supersedes`:** When storing a correction, pass the old memory's ID as `supersedes` so the old one is deprioritized rather than deleted.

Memories that reach high stability (>1500 days) enter permastore and stop decaying.

### Graph depth for different query types

- **depth: 0** (default) -- Direct matches only. Use for simple lookups ("what is the user's preferred editor?").
- **depth: 1** -- Include immediate neighbors. Use when you want related context ("tell me about the deployment pipeline" might surface CI/CD configs linked to deployment memories).
- **depth: 2-3** -- Broader exploration. Use for open-ended research ("what do I know about this project's architecture?"). Higher depth returns more results but may include loosely related memories.

## Prompting guide

Add these instructions to your AI assistant's system prompt or project instructions file (e.g., `CLAUDE.md`, `.cursorrules`). Copy the block below as-is or adapt it.

### Ready-to-use prompt block

````markdown
# Memory

Use the recalld MCP tools (`store_memory`, `recall_memories`, `get_memory`,
`reinforce_memory`, `forget_memory`, `find_similar_memories`) for persistent
memory across sessions.

## When to recall (proactive)

- At the START of every conversation, recall memories relevant to the current
  project directory or topic to establish context. You do NOT need to be asked
  "do you remember" — search proactively.
- When the user asks a question that prior knowledge could inform.
- When you are about to make a recommendation — check if there are memories
  about the user's preferences or past decisions on this topic.
- When the user references something from a previous conversation.

## When to store (proactive)

Store durable knowledge that will be useful across future sessions. Every
memory needs a `summary` (concise, 1-2 sentences) and should include
`entities`, `topics`, and `tags` for searchability.

**What to store:**
- User profile: role, expertise, preferences, communication style, team
- Feedback on your approach: what worked, what the user corrected, and WHY —
  capture the reasoning so you can apply it to novel situations, not just
  the specific case
- Project context: architecture decisions, tech stack choices, constraints,
  conventions that aren't obvious from the code
- Important decisions and their rationale
- Team and org context: who owns what, reporting structure, stakeholders
- Cross-session context the user shares (deadlines, relationships between
  projects, priorities)

**IMPORTANT:** Do not wait until the end of a conversation or until asked.
Store memories as they arise. After every significant exchange (a decision
is made, a preference is expressed, a project detail is learned, or a
recommendation is accepted/rejected), store immediately. If you are unsure
whether something is worth storing, store it. Memories decay over time if
they are not reinforced.

**What NOT to store:**
- Ephemeral task details (current branch, in-progress work, specific line
  numbers)
- Code snippets or file contents derivable from the codebase
- Information that can be read from project files (package.json, Cargo.toml)
- Things already documented in project README or docs

**How to write good memories:**
- `summary`: Concise but specific. Include names, dates, and key terms that
  make the memory findable via semantic search. Bad: "User prefers a certain
  style." Good: "User prefers single-line error handling with early returns
  over nested match blocks in Rust."
- `full_text`: Provide this for any memory where the summary alone would lose
  important nuance. Include the reasoning, context, and direct quotes when
  relevant. This field is dropped first during memory decay, so the summary
  must stand on its own.
- `entities`: ALL people, projects, tools, and proper nouns mentioned. Use
  canonical names ("Kubernetes" not "k8s", "PostgreSQL" not "postgres").
  These power the entity graph — missing entities means missing connections.
- `topics`: 1-5 lowercase topic keywords. Examples: "deployment", "testing",
  "authentication", "performance", "code-style". These are used for filtering.
- `tags`: Use hierarchical tags for consistent categorization:
  - `type/user-profile`, `type/feedback`, `type/project`, `type/decision`
  - `project/<name>` for project-specific memories
  - `tech/<name>` for technology-specific context

**Supersedes:** When storing a correction or update to an existing memory,
pass the old memory's ID as `supersedes`. This deprioritizes the outdated
memory in search results while preserving the history.

## When to reinforce

- When you recall a memory and it turns out to be accurate and useful,
  reinforce it (quality 3-4). This strengthens it so it persists longer.
- When a recalled memory was partially wrong or hard to find, reinforce with
  quality 2 (hard) — it still gets strengthened, just less.
- When a recalled memory was completely wrong, reinforce with quality 1
  (forgot) to weaken it, then store a corrected version with `supersedes`.

## Search strategy

When searching for memories, think about how the answer might be phrased in a
stored memory, not just how the question is phrased.

- For simple factual lookups ("what editor does the user prefer?"), a single
  query with depth 1 is sufficient.
- For questions requiring inference or combining facts ("would this user
  prefer approach A or B?"), use depth 2 and search for the underlying
  preferences and past decisions rather than the inference itself.
- For broad context gathering ("what do I know about this project?"), use
  depth 2-3 to traverse the memory graph.
- When a query involves specific names or terms, include them in the search
  even if they seem redundant — the full-text search index excels at exact
  name matching.
````

### Adapting the prompt

The block above covers the general case. You may want to add project-specific instructions:

**Per-project namespace binding:**

```markdown
Use the "my-project" namespace for all memory operations in this repository.
```

Or let recalld handle it automatically by placing a `.recalld.toml` in the project root:

```toml
namespace = "my-project"
```

**Domain-specific storage rules:**

```markdown
When working in this repo, also store:
- API contract changes and the reason behind them
- Performance benchmarks and their context (date, hardware, dataset)
- Incident details and root causes
```

**Decay tuning:**

If you find memories disappearing too quickly or persisting too long, adjust `decayRateMultiplier` in your namespace or config. Values above 1.0 slow decay; below 1.0 speed it up; 0.0 disables decay entirely.
