# recalld UI — Build spec and plan

Companion to `DESIGN.md` (how it looks and behaves) and `tokens.css` (the values). This file covers
how it's built: architecture, backend changes, frontend stack, and a phased plan with acceptance
criteria. Build **one phase per PR**.

## 0. Ground rules

- The UI is **optional**: behind a `ui` cargo feature. Default builds and the Docker image without
  the feature must be byte-for-byte unaffected in behavior.
- **Rust backend, TypeScript frontend.** No Python anywhere in the shipped app.
- **No UI-side data stores or search indexes.** The UI reads everything through recalld's own APIs,
  so it can never drift from what agents see.
- **Read-only by default.** Every read uses `recordAccess:false` or a daemon `*_read_only` method.
  Browsing must never reinforce a memory or trigger retrieval-induced forgetting. Only Reinforce and
  Forget write, and Forget asks for confirmation.
- Follow the repo's `AGENTS.md`, including its scratch-data cleanup policy and keeping tests isolated
  from personal recalld data, sockets and backups.
- Performance budgets (§6) are acceptance criteria, not aspirations.

## 1. Architecture

```
 browser (Svelte SPA, served from the binary)
   │  plain HTTP on loopback: http://127.0.0.1:<port>/?t=<token>
   ▼
 `recalld ui`  ── axum: static assets (rust-embed) + /api/ui/* JSON + /api/ui/events (SSE)
   │  existing daemon client (Unix socket, JSON-RPC 2.0), auto-starts the daemon like `recalld mcp`
   ▼
 recalld daemon  ── owns the store (exclusive lock); new read-only methods + event bus
```

Why go through the daemon: most users run `recalld mcp`, which auto-starts the daemon, and the store
is exclusively locked, so the UI must be another daemon client and must never open the store itself.
For `recalld serve` users (Docker), mount the same UI router on the existing axum app under `/ui`,
calling the in-process system directly; it uses the same handlers.

### 1.1 Security (required)
- Bind `127.0.0.1` only. Random port by default (`--port` to pin one).
- A per-launch random token is passed in the opened URL (`?t=…`), exchanged for an `HttpOnly;
  SameSite=Strict` cookie, and required on every `/api/ui/*` call.
- Reject requests whose `Host` header isn't `127.0.0.1:<port>` or `localhost:<port>`, to block DNS
  rebinding. No CORS.
- Mutations (Reinforce, Forget) also require a header `X-Recalld-UI: 1`.

## 2. Backend changes (Rust)

### 2.1 New CLI command
`recalld ui [--port N] [--no-open]` (feature `ui`): connect to the daemon (auto-start it, same as the
MCP path in `src/main.rs`), start the axum server, open the browser. Logs the URL. Exits on Ctrl-C.
New module `src/ui/` holds the server, routes, asset embedding and SSE bridge.

### 2.2 Asset embedding
`rust-embed` on `ui/dist/`. Debug builds read from disk (enables `pnpm dev` + a proxy); release
builds embed. SPA fallback: unknown non-`/api` paths serve `index.html`. Long-cache hashed assets;
`no-store` for `index.html`. Add `make ui` (runs `pnpm -C ui install --frozen-lockfile && pnpm -C ui
build`) and make `make release` and `make install` depend on it (the UI is a default feature). CI: build the UI once, then `cargo build --features
ui`.

### 2.3 New read-only daemon methods
Add to the daemon dispatcher (`src/daemon/server.rs`) and the client (`src/daemon/client.rs`). All
are read-only and never record access.

| Method | Returns | Notes |
|---|---|---|
| `graph_snapshot { namespace? }` | `{ nodes: { ids[], ns[], phase[], strength[], decayStrength[], stability[], createdAt[], lastAccess[], degree[], permastore[] }, edges: { src[], dst[], type[], weight[] } }` | Column arrays (indices into `nodes`) for compact transfer. Read from the in-memory `RelationshipGraph` plus the cache. All namespaces when `namespace` is omitted. Gzip over HTTP. Must handle 50k nodes / 500k edges in < 300ms server time. |
| `health_report { namespace? }` | the existing `HealthReport` (`src/api/models.rs`) | Port the HTTP handler (`src/api/handlers.rs` `health_report`) so the daemon can serve it; reuse its cache. |
| `system_status` | version (`CARGO_PKG_VERSION`), protocol version, uptime, data dir, bind/socket, subsystem health (existing `check_health`), cache hits/misses/budget (from the metrics registry), embedding provider/model/latency, **process RSS** (new: `proc_pidinfo` / `/proc/self`), **connected clients** (new: name + since) | Powers Health → System. |
| `memory_neighbors { id, depth ≤ 2 }` | local subgraph | Detail panel "Local graph". |
| `access_history { id, limit }` | recent access events with kind and agent | Detail panel activity. |
| `subscribe { kinds[] }` | stream of event notifications (below) | A long-lived connection that receives server-initiated JSON-RPC notifications. The daemon protocol doesn't have notifications yet; add them for this connection type only. |

Existing methods the UI uses as they are: `search_read_only`, `get_memory_read_only`,
`find_similar_read_only`, `list_memories`, `list_tags`, `list_namespaces`, `namespace_stats`,
`reinforce_memory`, `delete_memory`.

### 2.4 Event bus (live activity)
- A `tokio::sync::broadcast` channel in the system (capacity ~1,024, lossy). Publishing must never
  block or slow a request. If no receivers are attached (`receiver_count() == 0`), skip building
  event payloads entirely.
- Events:
  - `recall { id, at, agent?, namespace, query, seeds:[{id,score}], trace:[{from,to,edgeType,amount,order}], returned:[id], suppressed:[id] }`
  - `store { id, at, agent?, namespace, edges:[{to,type,weight}] }`
  - `reinforce { id, at, agent?, rating, stabilityBefore, stabilityAfter }`
  - `forget { id, at, agent? }`
  - `phase_change { id, at, from, to }` (from decay sweeps)
  - `sweep { at, moved:{fullToSummary, summaryToGhost, ghostToDeleted} }`
- **Activation trace:** add an optional recorder to `spreading_activation_resolved` in
  `src/graph/activation.rs` that pushes `(source, neighbor, edge_type, spread, order)` wherever it
  accumulates spread. It's bounded by `max_budget` (100 expansions), so it stays small. It's only
  enabled when a subscriber exists.
- **Agent attribution:** the MCP server already reads `clientInfo.name` at initialize
  (`src/mcp/server.rs`). Carry it on daemon requests as an optional `agent` field. Stateless MCP
  connections may lack it, so attribution is best-effort ("unknown agent").
- UI-originated reads are read-only and publish nothing.

### 2.5 Multi-namespace search
recalld's search is per-namespace (`SearchQuery.namespace`). For "All namespaces", phase 2 fans out
from `recalld ui` (parallel `search_read_only` per namespace, then merge). A later optional
`search_multi_read_only` on the daemon can move the merge into Rust. Merge by score when namespaces
share an embedding profile; otherwise group by namespace (the response says which).

### 2.6 Types
Derive TypeScript types from the Rust response structs with `ts-rs` (feature-gated) into
`ui/src/lib/api/types/`. CI fails if the generated types are stale.

## 3. Frontend stack (`ui/`)

| Concern | Choice |
|---|---|
| Language / build | TypeScript (strict), Vite, pnpm |
| Framework | Svelte 5 (runes) with SvelteKit `adapter-static` (SPA mode, `fallback: index.html`), file routes `/map`, `/explore`, `/timeline`, `/health` |
| Styling | `tokens.css` as the only source of values; component styles in Svelte `<style>` blocks using `var(--…)`. (Tailwind is optional; if used, map its theme to the tokens.) |
| Accessible primitives | bits-ui (popover, listbox, dialog, tooltip) |
| Server state | TanStack Query (svelte-query): stale-while-revalidate, query keys include the namespace scope |
| Lists | TanStack Virtual |
| Graph render | **Sigma.js v4** + graphology (WebGL2; labels in WebGL via SDF) |
| Graph physics | d3-force in a dedicated **Web Worker**; positions streamed as transferable `Float32Array`s; cached per memory in IndexedDB (`idb-keyval`) |
| Recall light | custom Sigma v4 WebGL layer: instanced comet quads + per-node glow attributes driven by time uniforms |
| Charts | hand-rolled SVG (decay curves are the FSRS formula; histograms are bars) + `d3-scale`/`d3-shape` helpers only |
| Fonts | `@fontsource/*`, bundled (no network) |
| Tests | Vitest (units: FSRS math, label placement, merge logic), Playwright (flows + screenshot diffs against the reference screens), a perf harness (§6) |

Shared client code:
- `lib/fsrs.ts`: `R(t,S) = (1 + 19/81 · t/S)^-0.5`, phase thresholds 0.70 / 0.30 / 0.05, permastore
  ≥ 1,500 d. Must match `src/decay/fsrs.rs`; unit-test against values computed from it.
- `lib/labels.ts`: greedy collision-free label placement (DESIGN §6).
- `lib/scope.svelte.ts`: the global namespace scope (persisted to localStorage).
- `lib/prefs.svelte.ts`: per-viewer conveniences (collapsed sidebar, theme), persisted to localStorage.
- `lib/events.ts`: an SSE client with reconnect and backoff, feeding the light layer and activity lists.

## 4. Phases

Each phase ends with a PR: description, screenshots, perf numbers where relevant, and the cleanup
report `AGENTS.md` requires.

### Phase 1 — Foundations (backend + shell)
- `ui` feature, `src/ui/` module, `recalld ui` command, token/Host security, asset embedding, SSE
  plumbing (heartbeat only).
- Daemon methods: `graph_snapshot`, `health_report`, `system_status` (RSS/clients included).
- `ts-rs` type generation.
- `ui/` scaffold: routes, tokens, fonts, favicon set from `ui/static/` (`<link rel="icon" href="/favicon.svg" type="image/svg+xml">`, `.ico` + apple-touch fallbacks), sidebar with the bulb logo (collapsible, global namespace scope), page header
  with Search field, ⌘K / `/` shortcut wiring (palette stub), daemon-unavailable banner.
- **Accept when:** `cargo build` without the feature is unchanged; `cargo build --features ui`
  produces one binary; `recalld ui` opens a page that lists namespaces and counts from the daemon;
  requests without the token or with a foreign Host get 403; `cargo test` and the UI unit tests pass.

### Phase 2 — Explore, Search, memory detail
- Explore table (virtualized, filters, sort, URL state), detail panel with the decay curve (FSRS
  client math), Reinforce (with rating) and Forget (confirm).
- ⌘K palette on recalld's hybrid search with explain, grouped Best matches / Through links,
  multi-namespace fan-out and merge.
- **Accept when:** results match `recalld-cli recall` for the same query; no read changes any
  memory's `last_accessed_at` (an integration test checks this); keyboard-only operation works; a
  100k-row table scrolls at 60 fps.

### Phase 3 — Memory map
- Sigma v4 renderer, d3-force worker, IndexedDB position cache, collision-free labels, hover tooltip,
  selection/panel, Links menu (temporal off by default), phase chips, Decay scrubber, double-click
  neighborhood focus, zoom controls.
- **Accept when:** §6 budgets hold on the synthetic 10k/100k graph; a reload opens settled from the
  cache; labels never overlap (unit test plus a visual check).

### Phase 4 — Live activity
- Daemon event bus + activation-trace recorder + agent attribution + `subscribe`; SSE bridge.
- Light layer (seed bloom, comets, convergence, result rings, suppression, store/reinforce/phase
  animations), live pill, activity rows, Replay, reduced-motion mode, a dev **simulator** that emits
  synthetic events.
- **Accept when:** a recall by an agent lights the map within 100ms of the daemon finishing it; the
  trace matches the PQSA order; 50 recalls/s keeps 60 fps; with no UI attached, recall latency is
  unchanged (benchmark before and after with the crate's existing `bench` feature and perf tests).

### Phase 5 — Timeline and Health
- Timeline chart (range control incl. All time, weekly/monthly buckets), day-grouped activity stream.
- Health: Memories, Coming up (decayForecast), Fading soon (with Reinforce), Storage, System.
- **Accept when:** every number matches `recalld-cli health` / `status` for the same scope.

### Phase 6 — Polish and release
- Empty/loading/error states everywhere, responsive rails, focus and a11y pass, screenshot-diff tests
  against `reference/`, `install.sh` and release workflow produce a UI-enabled binary variant, docs
  (README section + `docs/ui.md`).

## 5. Out of scope for v1
Editing memory text, multi-user/remote access, a Tauri/desktop wrapper (possible later:
it would wrap the same SPA), and 3D graphs.

## 6. Performance budgets
Measured on an M-series MacBook Air or equivalent, Chrome stable, with a synthetic store of 10k
memories / 100k edges (add a generator under `src/bench/` or `ui/perf/`):

| Metric | Budget |
|---|---|
| `recalld ui` launch → first paint | < 1.0 s (warm daemon) |
| Memory map interactive with cached positions | < 1.5 s |
| Pan / zoom / drag | 60 fps sustained (p95 frame < 16.7ms) |
| Hover → tooltip | ≤ 1 frame after the 120ms delay |
| Search keystroke → results | < 200ms p95 (local Ollama embedding included) |
| Explore 100k rows scroll | 60 fps |
| Recall event → light on screen | < 100ms |
| JS bundle (gzip, initial route) | < 250 KB; graph code split into the /map chunk |
