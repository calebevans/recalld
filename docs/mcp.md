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
      "mcp__recalld__namespace_stats",
      "mcp__recalld__list_memories",
      "mcp__recalld__list_namespaces",
      "mcp__recalld__list_tags"
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

The HTTP endpoint is available whenever `recalld serve` is running. It is a **dual-era** MCP endpoint: MCP 2026-07-28 (stateless) and MCP 2025-06-18 and earlier (the `initialize` handshake) are served on the same URL, and the era is chosen per message.

**Modern clients (MCP 2026-07-28)** send no `initialize` and get no session. Every request carries its own identity in `params._meta`:

```json
"_meta": {
  "io.modelcontextprotocol/protocolVersion": "2026-07-28",
  "io.modelcontextprotocol/clientInfo": { "name": "ExampleClient", "version": "1.0.0" },
  "io.modelcontextprotocol/clientCapabilities": {}
}
```

`protocolVersion` and `clientCapabilities` are required (`-32602` naming the missing key if absent; `-32022` with `data.supported` if the version is one we do not serve). Every request must also mirror three SEP-2243 headers, which are validated against the body — a mismatch is HTTP 400 with JSON-RPC `-32020`:

| Header | Value | Required for |
|---|---|---|
| `Mcp-Method` | the `method` field | every request |
| `MCP-Protocol-Version` | `_meta`'s protocol version | every request |
| `Mcp-Name` | `params.name`, or `params.uri` for `resources/read` | `tools/call`, `prompts/get`, `resources/read` |

Values that are not safe plain ASCII may use the `=?base64?{value}?=` sentinel, which is decoded before comparison. Notification POSTs skip header validation — the revision leaves that case undefined.

Modern responses never carry `Mcp-Session-Id`, and any session id sent on a modern request is ignored. Every result carries `resultType: "complete"` and `_meta["io.modelcontextprotocol/serverInfo"]`; `server/discover`, `tools/list`, `resources/list` and `resources/templates/list` also carry `ttlMs: 3600000` / `cacheScope: "public"`, and `resources/read` carries `ttlMs: 0` / `cacheScope: "private"`.

Start with `server/discover`, which is answered on either era and needs no `_meta`. It returns `supportedVersions`, `capabilities` and `instructions`.

**Legacy clients** work exactly as before: send `initialize`, get an `Mcp-Session-Id`, send it on every subsequent request. `initialize` echoes the version you asked for (`2025-06-18`, `2025-03-26` or `2024-11-05`; anything else falls back to `2025-06-18`) and never `2026-07-28`, since that revision has no `initialize`. Three behaviours are new: sessions expire after 30 minutes idle (the resulting 404 tells you to `initialize` again), at most 1024 may be live at once, and a *failed* `initialize` no longer leaves an unusable session behind.

**Origin validation.** `/mcp` rejects browser requests from unexpected origins with HTTP 403 (DNS-rebinding protection for a loopback-bound server). Requests with no `Origin` header — curl, SDKs, anything non-browser — are always allowed, as is any loopback origin on any scheme or port. `Origin: null` (a `file://` page or sandboxed iframe) is rejected. To allow a real browser origin, list it in `server.mcp_allowed_origins` (or `RECALLD_SERVER_MCP_ALLOWED_ORIGINS`, comma-separated).

Request bodies over `server.max_body_bytes` (10 MB by default) are rejected with HTTP 413; `/mcp` previously had no body limit at all. `GET /mcp`, and `DELETE` without a session id, return 405.

Configure per-project namespace defaults in a `.recalld.toml` file (see above), not CLI flags.

## Available tools

### A note on limits

Every length limit below is measured in **UTF-8 bytes, not characters**. For plain ASCII the two are the same, so a 2000-byte `summary` holds 2000 characters. Non-ASCII characters cost more: accented letters and curly quotes are 2-3 bytes, em dashes (`—`) are 3 bytes, and emoji are 4 bytes. A summary of 1900 em dashes is 5700 bytes and will be rejected. Rejection messages report both numbers, e.g. `summary is 5700 bytes (1900 characters), which exceeds the 2000-byte limit.`

Array fields (`tags`, `entities`, `topics`, `emotions`) carry two limits: a plain item count, and a maximum byte length per element. A `tags` element may be up to 128 bytes. `entities`, `topics`, and `emotions` labels are slightly shorter — 121, 122, and 120 bytes — because each label is merged into `tags` as `entity/<label>`, `topic/<label>`, or `emotion/<label>`, and the derived tag has to fit the same 128 bytes. A rejection names the offending element by index, e.g. `entities[3] is 183 bytes (61 characters), which exceeds the 121-byte limit.`

Before this limit existed there was no per-element bound at all: an over-long entity was accepted, merged into an over-long tag, and then dropped by the storage layer while the call still reported success.

> **Known limitation:** `entities`, `topics`, and `emotions` are converted into `entity/…`, `topic/…`, and `emotion/…` tags *after* validation runs, and the `tags` limit of 64 is checked against the tags you supplied. A memory that supplies 64 tags plus 32 entities, 32 topics, and 32 emotions can therefore end up with up to 160 tags stored. This is accepted today and is not treated as an error.

### Argument validation

`store_memory`, `store_memories`, `list_memories`, `list_tags`, `namespace_stats`, `recall_memories` and `find_similar_memories` validate their arguments strictly. A wrong-typed or unparseable argument is an error naming the field; it is never coerced, defaulted, or dropped. Earlier versions guessed silently — a `tags` array sent as a comma-joined string stored zero tags, a wrong-typed `namespace` fell back to the default partition, an unparseable `supersedes` stored the memory with no link, and a wrong-typed `timeRangeStart` was dropped, silently widening a range query to the whole namespace — so a client with a broken serializer could write a long run of quietly wrong memories, or read a long run of quietly wrong results, without a single error reaching the agent.

**An out-of-range `limit` is rejected, not clamped.** `list_memories` used to clamp a `limit` above 200 down to 200, which told a paginating caller it had seen the whole namespace when it had seen the first page. The schema already declares the maximum, so only a schema-violating client is affected, and it now gets one clean error instead of a quietly short page.

**Explicit `null` still means "absent."** `{"fullText": null}` is treated exactly like omitting `fullText`, for every optional field. SDKs that serialize optional fields as `null` need no changes.

Example messages:

| Situation | Error |
|---|---|
| `"summary"` missing or `null` | `Missing required parameter: summary` |
| `"fullText": ["a"]` | `Parameter 'fullText' must be a string (got array)` |
| `"tags": "a,b"` | `Parameter 'tags' must be an array of strings (got string)` |
| `"tags": ["a", 3]` | `Parameter 'tags' must be an array of strings (element at index 2 is a number)` |
| `"supersedes": 123` | `Parameter 'supersedes' must be a string containing a UUID (got number)` |
| `"limit": "50"` | `Parameter 'limit' must be an integer (got string)` |
| `"limit": 1000` | `Parameter 'limit' must be between 1 and 200 (got 1000)` |
| `"limit": -1` | `Parameter 'limit' must be a non-negative whole number (got -1)` |
| `"namespace": ["work"]` | `Parameter 'namespace' must be a string (got array)` |
| `"allNamespaces": "true"` | `Parameter 'allNamespaces' must be a boolean (got string)` |
| `"allNamespaces": true` with `"namespace"` | `Parameter 'allNamespaces' cannot be combined with an explicit 'namespace'; pass one or the other` |
| `"timeRangeStart": ["x"]` | `Parameter 'timeRangeStart' must be an integer (epoch millis) or an ISO 8601 string (got array)` |
| `"supersedes": "mem-1234"` | `Invalid UUID in parameter 'supersedes': "mem-1234"` |
| a `memories` entry that is not an object | `Item at index 3 must be an object (got string)` |

`parentId` and `supersedes` accept any spelling `Uuid::parse_str` accepts: canonical, hyphenless, braced, and `urn:uuid:` forms.

In `store_memories`, a rejected item does not abort the batch: its entry in `results` carries `error` plus a `field` naming the offending parameter (`""` when the item as a whole is at fault), and the remaining items are still stored. The `field` member is additive; the rest of the response shape is unchanged.

#### Tool-call markup in text fields

`summary` and `fullText` are additionally scanned for literal tool-call markup — the XML-ish tags a client emits when serializing a tool call, spliced into the argument text instead of being parsed. This happens when the client's serializer breaks; the symptom is a memory whose `fullText` ends in parameter closing tags and whose `tags`, `entities`, and `namespace` are silently wrong. Such a call is rejected outright rather than cleaned up, because stripping the tags cannot recover the fields that never arrived, and a sanitized write becomes a permanent record whose embedding, full-text index, and graph edges were all computed from corrupt text.

The rejection names the matched token and its byte offset, and states plainly that nothing was stored:

```
Parameter 'fullText' contains literal tool-call markup ("</fullText>" at byte offset 2731).
Arguments appear to have been serialized incorrectly; nothing was stored. Re-send with
structured JSON arguments and no XML tool-call tags in text fields.
```

The detector is deliberately narrow, because a memory store is used to record notes *about* markup. It anchors on the client's reserved vendor-namespace tag prefix, on `<function_calls>` literals and the vendor-prefixed attribute-first `<invoke name=` / `<parameter name=` forms, on the exact camelCase parameter tags `<fullText>` and `<parentId>`, on a parameter closing tag followed across whitespace only by another parameter tag or a call terminator, and on text that ends with a call terminator. The **bare** (unprefixed) `<invoke name=` and `<parameter name=` spellings are deliberately *not* anchors: TestNG suite XML is built out of `<parameter name="browser" value="chrome"/>`, and real corruption always carries the vendor prefix that the first rule already catches. Ordinary technical prose is unaffected: C# `/// <summary>` doc comments, HTML `<details><summary>`, Atom entries, DocBook `<parameter>`, TestNG and Spring `<parameter name=…>`, WS-BPEL `<invoke partnerLink=…>`, JSX, Maven POM fragments, Rust generics such as `Vec<String>`, shell redirection, and prose quoting recalld's own parameter names in JSON all store normally.

The one known false positive is a note whose **last line** is a bare `</invoke>` quoted from a BPEL snippet. That rule exists to catch truncated corruption where the parameter closing tag was consumed but the terminator survived; if it ever bites you in practice, end the note with a sentence after the quoted snippet (trailing whitespace alone is ignored) and report it.

### store_memory

Store a new observation, fact, or piece of context. The system automatically generates an embedding for semantic search. Memories decay over time unless reinforced.

#### Parameters

| Parameter | Type | Required | Default | Description |
|-----------|------|----------|---------|-------------|
| `summary` | string | yes | -- | Short description (max 2000 bytes of UTF-8; ~2000 ASCII characters, fewer with em dashes/curly quotes/emoji) |
| `fullText` | string | no | -- | Detailed content. Dropped when memory decays to ghost phase. Max 1 MB = 1 048 576 bytes of UTF-8, counted on the raw text before JSON escaping. |
| `tags` | string[] | no | `[]` | Categorization tags, e.g. `["topic/rust", "type/observation"]`. Max 64 items, 128 bytes each. |
| `entities` | string[] | no | `[]` | Named entities (people, places, orgs). Used for search indexing and graph linking. Max 32 items, 121 bytes each. |
| `topics` | string[] | no | `[]` | Topic keywords, e.g. `["rust", "cooking"]`. Max 32 items, 122 bytes each. |
| `emotions` | string[] | no | `[]` | Emotional tone, e.g. `["happy", "anxious"]`. Max 32 items, 120 bytes each. |
| `namespace` | string | no | `"default"` | Memory partition |
| `parentId` | string | no | -- | UUID of parent memory for hierarchical linking |
| `supersedes` | string | no | -- | UUID of an existing memory in the same namespace that this one replaces. Recall returns this memory **in place of** the old one. The store fails if the target does not exist or is in a different namespace. See [supersedes semantics](#supersedes-semantics). |

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
  "createdAt": "2025-06-24 00:00:00 UTC"
}
```

---

#### supersedes semantics

`supersedes` is a *desired end state*, not an event, and the store result always
says what actually happened to the link.

**Preconditions, checked before anything is written.** The target must exist and
must live in the same namespace as the new memory, and a memory cannot supersede
itself. Violating any of these fails the whole store -- no memory is created.
Checking up front is what keeps a rejected link from leaving an orphaned memory
behind. A *tombstoned* (deleted) target is fine: correcting a memory you just
deleted is legitimate.

**Idempotent.** Superseding the same memory twice is not an error; the second
call reports `alreadyApplied`.

**The result reports the outcome.** Every successful store carries a
`supersedes` object:

| Field | Description |
|---|---|
| `target` | The memory that was superseded. |
| `status` | `applied`, `alreadyApplied`, `appliedNotDurable`, or `failed`. |
| `detail` | Explanation, present for every status except `applied`. |

`applied` means the edge was created and written to disk. `alreadyApplied` means
it was already in place. `appliedNotDurable` means the edge is active for the
running process but could not be persisted, so it will be lost on restart.
`failed` means no edge exists; the memory was still stored.

At recall, a superseded memory is replaced by its replacement **only when the
replacement passes the query's own filters** -- namespace above all. If it does
not, the original is returned instead, so a result is never dropped with nothing
in its place.

### store_memories

Store multiple memories in a single call. Each item has the same schema as `store_memory`. Returns an array of results, one per input memory.

#### Parameters

| Parameter | Type | Required | Default | Description |
|-----------|------|----------|---------|-------------|
| `memories` | array | yes | -- | Array of memory objects (max 100 per call). Each object has the same fields as `store_memory`. |

Each object in the `memories` array accepts:

| Field | Type | Required | Default | Description |
|-------|------|----------|---------|-------------|
| `summary` | string | yes | -- | Short description (max 2000 bytes of UTF-8; ~2000 ASCII characters, fewer with em dashes/curly quotes/emoji) |
| `fullText` | string | no | -- | Detailed content. Max 1 MB = 1 048 576 bytes of UTF-8, counted on the raw text before JSON escaping. |
| `tags` | string[] | no | `[]` | Categorization tags. Max 64 items, 128 bytes each. |
| `entities` | string[] | no | `[]` | Named entities. Max 32 items, 121 bytes each. |
| `topics` | string[] | no | `[]` | Topic keywords. Max 32 items, 122 bytes each. |
| `emotions` | string[] | no | `[]` | Emotional tone. Max 32 items, 120 bytes each. |
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
      "createdAt": "2025-06-24 00:00:00 UTC"
    },
    {
      "index": 1,
      "id": "b2c3d4e5-f6a7-8901-bcde-f12345678901",
      "namespace": "default",
      "phase": "Full",
      "strength": 1.0,
      "stability": 3.7145,
      "createdAt": "2025-06-24 00:00:00 UTC"
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
      "createdAt": "2025-06-24 00:00:00 UTC",
      "lastAccessedAt": "2025-06-25 00:00:00 UTC"
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
  "createdAt": "2025-06-24 00:00:00 UTC",
  "lastAccessedAt": "2025-06-25 00:00:00 UTC",
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
      "createdAt": "2025-06-23 00:00:00 UTC",
      "lastAccessedAt": "2025-06-24 00:00:00 UTC"
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
    {
      "memories": [
        {
          "id": "a1b2c3d4-...",
          "summary": "User prefers snake_case in Rust"
        },
        {
          "id": "c3d4e5f6-...",
          "summary": "User likes snake_case for Rust code"
        }
      ],
      "maxSimilarity": 0.92
    }
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
  "createdAt": "2025-06-24 00:00:00 UTC"
}
```

---

### namespace_stats

Get statistics for a memory namespace including the live memory count, tombstone count, phase breakdown (full/summary/ghost), permastore count, average strength, edge count, and vector storage size. Use this to check how many memories exist or monitor namespace health.

`memoryCount` counts **live** memories only and always equals `full + summary + ghost`, so it agrees with `list_memories.total` and `list_namespaces[].memoryCount`. Deleted memories appear separately as `tombstoneCount`: a namespace showing one memory where you stored eleven has ten tombstones, and nothing was lost.

#### Parameters

| Parameter | Type | Required | Default | Description |
|-----------|------|----------|---------|-------------|
| `namespace` | string | no | `"default"` | Namespace name to get stats for |

#### Example

Request:

```json
{
  "namespace": "default"
}
```

Response:

```json
{
  "name": "default",
  "memoryCount": 42,
  "tombstoneCount": 7,
  "phaseCounts": {
    "full": 30,
    "summary": 8,
    "ghost": 4
  },
  "permastoreCount": 3,
  "avgStrength": 0.72,
  "edgeCount": 156,
  "vectorBytes": 258048
}
```

`vectorBytes` is the logical size of the live vector data (`memoryCount` x embedding dimensions x 4). It is not the size of `vectors.dat`, which retains freed slots until compaction.

---

### list_memories

List memories in a namespace with pagination and optional filters. Unlike `recall_memories`, this does not require a search query -- it returns memories sorted by creation date (newest first). Use for browsing, auditing, or enumerating memories.

#### Parameters

| Parameter | Type | Required | Default | Description |
|-----------|------|----------|---------|-------------|
| `namespace` | string | no | `"default"` | Which namespace to list from |
| `limit` | integer | no | `50` | Maximum results per page (1-200). A larger value is **rejected**, not clamped. |
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
      "entities": ["TypeScript"],
      "topics": ["coding-style"],
      "createdAt": "2025-06-24 00:00:00 UTC"
    }
  ],
  "total": 1,
  "limit": 10,
  "offset": 0
}
```

---

### list_namespaces

List every memory namespace with its ID, embedding dimensions, live memory count, and creation date. Call this first when you do not know which namespaces exist, before targeting one with `list_memories`, `list_tags`, or `namespace_stats`.

Counts exclude deleted (tombstoned) memories, so they agree with `list_memories.total` and `namespace_stats.memoryCount` for the same namespace.

#### Parameters

None. The tool takes no arguments.

#### Example

Request:

```json
{}
```

Response:

```json
[
  {
    "id": 1,
    "name": "default",
    "embeddingDim": 1536,
    "memoryCount": 128,
    "createdAt": "2026-08-11 12:00:00 UTC"
  }
]
```

The response is a bare array, deliberately: it is the same payload the `recalld://namespaces` resource returns, and the total is the array length.

---

### list_tags

List the tag vocabulary with per-label memory counts, most common first. Use this to discover what labels already exist before filtering `list_memories` or `recall_memories`, and before storing — so you reuse an existing tag instead of inventing a near-duplicate.

Results come in four buckets: plain `tags`, plus the `entities`, `topics`, and `emotions` labels derived from those fields. **The derived buckets have their prefix stripped**, so a name from `entities` can be passed straight back to `store_memory` or `list_memories`; the prefixed form (`entity/recalld`) cannot, because those tools add the prefix themselves.

A tag that merely contains a slash, such as `project/recalld`, stays in `tags`. Counts cover live memories only.

#### Parameters

| Parameter | Type | Required | Default | Description |
|-----------|------|----------|---------|-------------|
| `namespace` | string | no | `"default"` | Which namespace to count within. Reads that namespace's memory records, costing about one `list_memories` call. |
| `allNamespaces` | boolean | no | `false` | Count across every namespace instead of one. Cannot be combined with an explicit `namespace`. |
| `limit` | integer | no | `50` | Maximum labels returned **per bucket** (1-500). Each bucket also reports its untruncated total. |

Passing `allNamespaces: true` together with an explicit `namespace` is an error, not a precedence rule — neither scope silently wins.

#### Example

Request:

```json
{
  "namespace": "default",
  "limit": 20
}
```

Response:

```json
{
  "namespace": "default",
  "memoriesCounted": 128,
  "tags": {
    "total": 41,
    "truncated": false,
    "items": [
      { "name": "type/decision", "count": 12 },
      { "name": "project/recalld", "count": 9 }
    ]
  },
  "entities": {
    "total": 120,
    "truncated": true,
    "items": [
      { "name": "recalld", "count": 30 }
    ]
  },
  "topics": { "total": 18, "truncated": false, "items": [] },
  "emotions": { "total": 3, "truncated": false, "items": [] }
}
```

In global scope (`allNamespaces: true`) both `namespace` and `memoriesCounted` are explicitly `null` rather than omitted, so the response always states its own scope. `memoriesCounted` is `null` there because the global path reads the tag index directly and touches no memory records.

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
- **Use `supersedes`:** When storing a correction, pass the old memory's ID as `supersedes` so recall returns the correction instead of the stale memory. The old memory is kept on disk, not deleted.

Memories that reach high stability (>1500 days) enter permastore and stop decaying.

### Graph depth for different query types

- **depth: 0** (default) -- Direct matches only. Use for simple lookups ("what is the user's preferred editor?").
- **depth: 1** -- Include immediate neighbors. Use when you want related context ("tell me about the deployment pipeline" might surface CI/CD configs linked to deployment memories).
- **depth: 2-3** -- Broader exploration. Use for open-ended research ("what do I know about this project's architecture?"). Higher depth returns more results but may include loosely related memories.

## Prompting guide

Add these instructions to your AI assistant's system prompt or project instructions file (e.g., `CLAUDE.md`, `.cursorrules`). Copy the block below as-is or adapt it.

### Ready-to-use prompt block

A concise version that covers the essential behaviors. Shorter prompts tend to get better compliance from language models.

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

### Extended prompt block

If you want more detailed guidance (at the cost of longer prompt length), use this expanded version.

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
- Feedback on your approach: what worked, what the user corrected, and WHY
- Project context: architecture decisions, tech stack choices, constraints,
  conventions that aren't obvious from the code
- Important decisions and their rationale
- Team and org context: who owns what, reporting structure, stakeholders

**IMPORTANT:** Do not wait until the end of a conversation or until asked.
Store memories as they arise. If you are unsure whether something is worth
storing, store it. Memories decay over time if they are not reinforced.

**What NOT to store:**
- Ephemeral task details (current branch, in-progress work)
- Code snippets or file contents derivable from the codebase
- Information that can be read from project files

**How to write good memories:**
- `summary`: Specific and searchable. Include names, dates, and key terms.
- `full_text`: Provide when the summary alone would lose important nuance.
- `entities`: ALL people, projects, tools, and proper nouns. Use canonical
  names. These power the entity graph.
- `topics`: 1-5 lowercase keywords (e.g., "deployment", "testing").
- `tags`: Hierarchical — `type/feedback`, `project/<name>`, `tech/<name>`.
- `supersedes`: When correcting a memory, pass the old memory's ID here.

## When to reinforce

- Recalled memory was useful: reinforce with quality 3-4.
- Recalled memory was wrong: reinforce with quality 1 (weakens it), then
  store the corrected version with `supersedes`.

## Search strategy

- Simple factual lookup: single query, depth 1.
- Inference or combining facts: depth 2.
- Broad context: depth 2-3.
- Specific names or terms: include them in the query.
````

### Reinforcing memory usage with hooks

Prompt instructions tell the model *how* to use memory, but models don't always follow instructions consistently -- especially long ones competing with other system prompt content. Hooks provide a per-turn nudge that's harder to ignore.

Claude Code [hooks](https://docs.anthropic.com/en/docs/claude-code/hooks) are shell commands that run in response to lifecycle events. Their stdout is injected into the conversation context. Two hooks work well for memory:

- **`UserPromptSubmit`** -- runs before the model responds to each user message. A good place to remind the model that memory tools are available.
- **`Stop`** -- runs after the model finishes a turn. A good place to prompt for storing new memories.

Add to your `~/.claude/settings.json` (or project `.claude/settings.json`):

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

These hooks are deliberately suggestive rather than commanding -- the model decides whether memories are relevant on each turn. This avoids unnecessary memory operations on simple tasks while ensuring the model doesn't forget that memory tools exist.

**Tuning the wording:** If you find the model using memory too aggressively, soften the hooks (e.g., just list available tools). If it's still too sparse, make them more directive (e.g., "You MUST check recall_memories before responding").

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
