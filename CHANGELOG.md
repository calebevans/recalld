# Changelog

## Unreleased

### Local web UI

- The UI has a dark theme. It follows the system's light or dark setting, and a moon button at the bottom of the sidebar switches to the other theme and remembers the choice; picking the system's own theme again goes back to following the system. The Memory map, charts and search recolor with it, and the sidebar mark matches the browser tab icon's dark variant.
- The memory map always lays memories out 3× further apart, as the old Spacing slider's top setting did, and the slider is gone. Nodes are drawn slightly larger, and the lightest Summary and Ghost shades are darker so they stand out from the background.

## 2.2.0 — 2026-11-20

### Changed

- MCP server instructions now lead with when to use memory: recall relevant context at the start of a task, and before finishing substantial work store durable decisions, preferences and non-obvious gotchas that are not already in code, PRs or issues. `recall_memories` and `store_memory` carry `_meta["anthropic/alwaysLoad"]`, so Claude Code loads them without a tool search first.
- New namespaces, including `default` on a new store, use the explicit-feedback lifecycle: a recall records that it returned a memory but no longer restarts its decay clock, and only `reinforce_memory` makes a memory last longer. A memory that keeps being recalled but is never reinforced now fades instead of staying at full strength. Existing namespaces keep their lifecycle until migrated with `recalld lifecycle migrate`. The `reinforce_memory` description now says that it, not recall, strengthens a memory.
- MCP tools no longer advertise `contractVersion`, and `store_memories`/`forget_memories` no longer advertise `batchResultVersion`; the server instructions and recovery hints drop their contract-version wording. Only version 1 exists for agents, so the labels only caused confusion. Older clients may still send `contractVersion: 1` or `batchResultVersion: 1`, and other values are refused as before. HTTP contract selection is unchanged.
- Memory is used silently: recalld's instructions and the documented prompt and hook templates tell agents not to report memory stores, recalls or status to the user, and to mention remembered facts only when they matter to the answer.

### Added

- `recalld lifecycle preview` and `recalld lifecycle migrate --destination DIR` (each with `--namespace NAME` or `--all`) move existing namespaces to the explicit-feedback lifecycle. With the daemon stopped, `preview` shows how many memories the next decay sweep moves to Summary or Ghost or deletes; `migrate` writes a validated, migrated copy of the store to a new directory, leaving the store unchanged, and prints how to swap it in. See "Moving to explicit feedback" in docs/guide.md. The raw `recalld lifecycle --source --request` form still works.
- Change how fast memories decay in an existing namespace, `default` included (#237). The MCP tool `update_namespace_policy` and `recalld-cli namespaces policy NAME` take only the settings to change (multiplier, initial stability, phase thresholds, permastore threshold) and keep the rest; over HTTP, `PUT /namespaces/{id}/policy` now accepts contract 2, the only request that does. `namespace_stats` and `list_namespaces` report each namespace's `policy`, including the multiplier the sweep uses and `daysUntilSummary`/`daysUntilGhost` for a memory that is never recalled. Returning a namespace to the inherited multiplier now records the current `[decay]` value for it at once, instead of leaving operations blocked until an offline adoption.
- Single-namespace mode for MCP: `[namespaces] mode = "single"` (or `RECALLD_NAMESPACES_MODE=single`) keeps every agent's memories in `default`. Agents are not told to choose or create namespaces, `create_namespace`, `delete_namespace` and `move_memories` are hidden, and a namespace other than `default` is rejected. The default, `"multi"`, is unchanged; the HTTP API and CLI are unaffected.

### Local web UI

- Summary and Ghost memories on the map are lighter solid shades that stay clearly visible: the faintest Summary shade is stronger, and Ghosts get a pale fill and a solid outline instead of an almost invisible one.
- The memory map has a **Spacing** slider (1× to 3×) that spreads memories further apart. It scales link length, repulsion and the no-overlap distance together; each spacing keeps its own saved layout, and the choice is remembered.
- Selecting a memory on the map opens the same card as an opened Explore row, popping up at the bottom of the map instead of a side panel. Esc or its close button dismisses it, and the selected memory stays in view above it.
- Memories on the map are always solid, with stage and fading shown as lighter shades instead of transparency, so links no longer show through nodes.
- Decay forecasts use the namespace's effective decay multiplier (its own, else the global one), as the decay sweep does: Explore's strength and **Next change**, the memory panel's decay curve, the Health forecast and at-risk list, and the map's Decay scrubber previously assumed a multiplier of 1. (#235)
- The namespace menu in search (⌘K) opens above the search popup instead of behind it. Search starts from the page's namespace (All when none is set) and changing it there filters only the search, not every page.

### Fixed

- `move_memories` no longer refuses namespaces on different lifecycles or policy revisions (for example after `update_namespace_policy`, an adopted `decay_rate_multiplier`, or between a namespace from an earlier release and a new one). Moved memories take on the destination's lifecycle and policy revision and keep their decay clock and stability.
- `reinforce_memory` straight after a `recall_memories` that returned the memory now grows its stability, on new namespaces and on namespaces migrated with `recalld lifecycle migrate`. Before, the recall restarted the memory's decay clock, so the reinforcement measured no idle time and added nothing, and stability on long-used stores stayed at its initial value. Namespaces left on the legacy lifecycle behave as before. (#250)
- docs/architecture.md said retrieval strengthens a memory and that `reinforce_memory` quality 1 grows stability a little; neither was true. It now describes both lifecycles.
- Changing `decay_rate_multiplier` no longer makes `recall_memories`, `store_memory`, `reinforce_memory` and `get_memory` fail, or skips every decay sweep, on a store created by 2.x. Startup now adopts the new value for each namespace that inherits it, with a receipt, and logs a `WARN` per namespace with the old and new value and how many memories move to Summary or Ghost or are deleted at the next sweep. If a namespace cannot be adopted (for example, it has more than 100,000 memories, the preview limit), startup continues, the namespace keeps its previous value, and a `WARN` says the change is not yet applied. A sweep that cannot resolve one namespace's policy now skips that namespace with a `WARN` instead of stopping the pass for all of them. (#232)
- `get_memory` and the web UI's memory view now return memories that older releases left in the Summary phase with their full text, or in the Ghost phase with tags, instead of failing with "Prepared remote response is unavailable" after an upgrade. The record is returned as stored. When a daemon reply is still rejected, the error message and the MCP or UI process log name the failed check. (#231)
- Recall collapses a whole `supersedes` chain to its newest version that the query may return. When the newest version could not be returned (deleted, a ghost the query did not ask for, or excluded by a filter), every older version came back instead, often side by side; now the next newest takes its place. A superseded memory is still available by ID.
- docs/architecture.md gave the default decay timeline as about 36 days to Summary and 280 to Ghost; the formula gives about 16.5 and 160 (and about 6,300 days to removal, not 13,300). (#234)
- A decay sweep wrote every memory on every pass, because strength changes a little each time, and each write keeps a receipt in `meta.db` for the life of the store: about 5 MB and 5 s of daemon start time per sweep on a 1,200-memory store. A sweep now writes a memory only when it changes phase or is removed. Strength is computed when it is read: recall and its `minStrength` filter, `get_memory`, `namespace_stats` and the HTTP API report the strength at the time of the request, as namespaces migrated to explicit feedback already did, instead of the value stored at the last write (while the relationship graph is being rebuilt, `get_memory` on a namespace that hasn't been migrated to explicit feedback leaves out the connection bonus instead of failing). Phase changes still happen on time, since the sweep decides them from computed strength. Part of #239.
- Daemon start no longer gets several seconds slower after every decay sweep. Startup checked the stored operation history about ten times, decoding each retained record about 190 times; it now checks it once on a copy and once on the opened store, and decodes each record once. Every check still runs, and the same stores are accepted or refused. Listing memories and reading the namespace and tag catalogs no longer recheck that history on each call. The store still grows with each sweep. (part of #239)

### Deprecated

- `[decay.phase_thresholds]` and `decay.permastore_threshold_days` (and `RECALLD_DECAY_FULL_TO_SUMMARY`, `RECALLD_DECAY_SUMMARY_TO_GHOST`, `RECALLD_DECAY_GHOST_TO_DELETE`, `RECALLD_DECAY_PERMASTORE_THRESHOLD_DAYS`) never had an effect: thresholds are set per namespace. They are still accepted, startup logs a `WARN` when they differ from the defaults, and they are gone from the sample config and the guide. (#233)

### Upgrade notes

- **Lifecycle:** existing namespaces keep the legacy lifecycle until you migrate them. To move them to explicit feedback, stop the daemon and close agent sessions (they restart it), then run `recalld lifecycle preview --all` and `recalld lifecycle migrate --all --destination <new-dir>`, and swap the directories as printed. Expect the first sweeps afterwards to move more long-unused memories to Summary and Ghost; that is the intended effect. Keep the old directory until you are satisfied; moving it back undoes the change.
- **Downgrades:** once 2.2.0 creates a namespace (including `default` on a new store) or clears a namespace's explicit decay multiplier, the store holds receipts that 2.1.x and earlier refuse to open. Restore a pre-upgrade backup to go back.
- **Decay multiplier:** a changed `decay_rate_multiplier` is adopted at daemon start for namespaces that inherit it, with a warning listing how many memories the next sweep will move. Check the daemon log after changing it.
- **Deprecated settings:** remove `[decay.phase_thresholds]` and `decay.permastore_threshold_days` from your config; they never had an effect and now log a warning. Use `recalld-cli namespaces policy` or `update_namespace_policy` per namespace instead.
- **Clients:** restart MCP clients after upgrading so they pick up the new tool list (`update_namespace_policy`) and instructions.

## 2.1.2 — 2026-11-20

### Local web UI

- The UI starts on the `default` namespace instead of All namespaces, which is lighter once memories live in project namespaces. **All namespaces** stays in the sidebar and scope menu, and a chosen scope is still remembered.
- Long summaries in the map's detail panel show at a smaller size and clamp to four lines, with **Show whole summary** to expand.
- Opening a row in Explore shows its whole summary instead of one truncated line.

### Fixed

- `delete_namespace` no longer refuses a small namespace as "operation too large" because another namespace is large. The admission check counted every memory and edge in the store against its 100,000-row bound; it now counts only the namespace being deleted and the edges that touch it, so an empty or tombstone-only namespace deletes without `force` and reports `tombstonesPurged`. A namespace that is itself too large is still refused.

## 2.1.1 — 2026-11-20

### Local web UI

- The memory map starts with memories active in the last 90 days, so large, densely linked stores lay out calmly. **Active in** widens it to 7 days, 30 days, 1 year or all time; memories outside the window leave the layout, not just the view.

### Fixed

- Decay sweeps on 2.x stores no longer stall for minutes. A startup or timer pass publishes derived views in batches of `write_batch_size` instead of after every memory (#220), and each commit reads only the memory's graph neighbourhood instead of every record and edge. Reads and writes are still refused while a pass runs, now for seconds rather than minutes on a store of about 1,200 memories. (#215)
- `recalld backup` no longer claims `-d` for `--destination`, so the global `-d/--data-dir` works with it. Spell out `--destination`. (#219)
- The guide no longer says the daemon shuts down after 30 idle minutes; since 2.1.0 it keeps running by default. (#218)
- A failed automatic 2.x upgrade no longer leaves another full copy of the database on every start attempt. When validation fails and the original is unchanged, the failure is recorded in `<data-directory>.recalld-upgrade-failed`; later starts on the same unchanged database with the same binary report the same error immediately without copying anything. Repairing or replacing the database, changing the recalld version or deleting that file retries the upgrade. (#216)

### Upgrade

- The legacy vector ownership repair now ships in the release binary as `recalld --config CONFIG repair-legacy-vectors plan|apply ...`, with the same arguments and checks as the former `repair_legacy_vectors` source example, which is removed. Release-binary users no longer need a Rust toolchain to repair a 1.0 database that fails the 2.0 upgrade (#217).
- When the automatic upgrade stops on a vector ownership mismatch (duplicate owner, freed slot still referenced, or missing slot), the startup error now ends by naming `recalld repair-legacy-vectors plan` and the "Repairing legacy vector ownership" section of `docs/guide.md`.

## 2.1.0 — 2026-11-20

### Local web UI

- Added `recalld ui`, a browser UI over your memories. It comes with the standard install and opens on `127.0.0.1`:
  - **Memory map:** every memory as a node, sized by its links and current strength and colored by stage. When an agent recalls, the map replays recalld's real spreading-activation path: seeds bloom, light travels along the links that were followed, results keep a ring, and memories suppressed by retrieval-induced forgetting cool briefly. New and forgotten memories animate in and out, and clicking a memory brings its links forward. A **Decay** scrubber projects every memory up to a year ahead with recalld's FSRS formula.
  - **Explore:** a fast, virtualized table of every memory with phase, strength, next phase change, namespace and last access. Filter by phase, tag, entity or agent, sort, and share the URL. Clicking a row opens it in place with its full text, labels, decay curve, activity and links.
  - **Timeline**, **Health** (counts by phase, decay forecast, storage, daemon status, connected agents) and **Search** (⌘K, recalld's own hybrid search with a score breakdown).
- Browsing never changes memory: every read uses recalld's read-only paths, so opening, searching or viewing a memory never reinforces it, resets its decay clock or triggers retrieval-induced forgetting. Only **Reinforce** and **Forget** (after a confirmation) write, through the same tool path agents use.
- Security: the UI binds `127.0.0.1` only, checks the `Host` header (`127.0.0.1`, `localhost`, `recalld.localhost`), exchanges a per-launch token for an `HttpOnly`, `SameSite=Strict` cookie, sends no CORS headers and requires a custom header on writes.
- The map pauses its layout and animation when the tab is hidden and stops entirely on other pages.
- Activity entries name the agent and query behind each recall. The daemon keeps a bounded in-memory journal of the last 1,000 recalls and reinforcements for this.

### Daemon

- The daemon no longer shuts itself down when idle. `recalld daemon start` defaults to `--idle-timeout 0` (never); pass minutes to bring an idle shutdown back.
- Fixed an overflow that made "no idle timeout" shut the daemon down at its first idle check whenever no client was connected. Auto-started daemons, which request no timeout, could exit about 30 seconds after the last client disconnected.
- Idle MCP sessions no longer exhaust the daemon's 32 connection slots. Connections without a complete next request within 60 seconds are reclaimed, clients reconnect before sending after 30 seconds idle, and a full daemon returns a bounded "unavailable" response instead of starting another daemon or falling back to the locked store.

### MCP

- `list_tags` now reports entities, topics and emotions for native memories. They were previously missing because native records keep structured labels separately from tags.

### Upgrade notes

- The standard release archives and `install.sh` now include the UI. The separate `recalld-ui-*` archives and `RECALLD_UI=1` are gone.
- Turn the UI off with `[ui] enabled = false` in the config or `RECALLD_UI_ENABLED=false`. Agents and the MCP server are unaffected.
- The Docker image is built without the UI (`--no-default-features`) and is otherwise unchanged. To build from source without it, use `cargo build --release --no-default-features`.
- From source, `make release` and `make install` now build the UI first, which needs Node 22.17+ and pnpm. A plain `cargo build` still works without Node; `recalld ui` then shows a page saying the UI wasn't built.
- Restart the daemon and MCP server processes after upgrading. A daemon started by an older binary keeps its old idle timeout and has no UI support until restarted (`recalld daemon stop`).
- Existing 2.0 databases need no migration.

## 2.0.3 — 2026-11-19

### Namespaces and memory moves

- Fixed namespace creation through daemon-backed MCP on native storage. The daemon now uses the existing bounded creation handler, checks response capacity before mutation, and preserves commit evidence on failure.
- Added `move_memories` for atomic moves of 1–100 Full or Summary memories between existing namespaces. Moves preserve IDs, retained text, labels, timestamps, lifecycle/access history, and graph edges. Embeddings are regenerated with the destination's configured provider and profile; compatible lifecycle revisions are required.
- Moves use an explicit store ID and stable idempotency key. Retrying the identical ordered request reconciles the original operation without copying/deleting records or repeating the move. Ghost/Tombstone memories and batches exceeding 16 MiB of retained full text are rejected.
- Added move-aware recovery, vector certification, history validation, and backup/restore handling. Preserved cross-namespace edges do not make namespace-scoped searches global.

### Upgrade notes

- Restart both the daemon and MCP server processes after upgrading, then refresh the client's tool list. Older daemons reject the new operations rather than falling back to unsafe behavior.
- Existing 2.0 databases need no additional startup schema migration. However, **after a move commits, older binaries cannot read its new durable move history**. Keep a compatible backup before using moves; downgrade requires restoring that backup and loses subsequent writes.
- Upgrading does not automatically move memories or create project namespaces. See the [move tool guide](docs/guide.md#move_memories) for requirements and retry behavior.

## 2.0.2 — 2026-11-18

### MCP usability and namespace guidance

- Added the read-only `get_session` tool, exposing the same connection defaults and namespace context as `recalld://session` to clients that primarily discover tools.
- Improved tool descriptions, agent prompts, and optional hooks: keep shared facts in `default`, reuse project namespaces for project-specific knowledge, and use topic namespaces when useful. Scope relevant recalls explicitly and avoid per-agent namespaces or unnecessary memory writes.
- Clarified access recording and duplicate detection: use `recordAccess:false` for inspection, give reinforcement deliberately, and treat similarity warnings as advisory rather than instructions to delete memories.
- Added recovery guidance for known transient read failures, uncertain writes, and old-client capability errors. Unsupported single-write retry keys are rejected before effects; durable batch resumption retains the original identity and unchanged full payload. Automatic retry policy is unchanged.

### Upgrade notes

- Existing 2.0 databases require no additional schema migration. Keep the existing database and embedding configuration.
- Restart the actual MCP server process using the updated binary, then reconnect the client and refresh its tool list to discover `get_session` and the updated descriptions. Reconnecting to an old process is insufficient; successful reads do not prove that an old client can write.

## 2.0.1 — 2026-11-18

### Legacy database recovery

- Startup errors now distinguish duplicate vector owners, references to freed slots, and missing vector slots, including the affected record and namespace identifiers.
- Added an opt-in, source-built offline repair example for legacy databases with invalid vector ownership. It requires an exact snapshot and explicit record-ID approval, regenerates recoverable embeddings through the configured local Ollama provider, and removes only approved records without retained text. It preserves the source, creates a backup and separate validated candidate, and clears derived full-text indexes for rebuilding. It never activates the candidate automatically.
- Existing 2.0 databases require no additional schema migration. Automatic upgrades remain strict: startup does not silently repair or discard records. See the [legacy vector recovery instructions](docs/guide.md#repairing-legacy-vector-ownership).

### Build and test fixes

- Fixed macOS path-alias assumptions in container-layout tests and updated the SQLite compatibility assertion for the bundled SQLite version.
- Pinned CI and release jobs to Rust 1.94.1 so a rolling `stable` update cannot give parallel jobs different compilers. Compiler provenance and test coverage checks remain enabled.

## 2.0.0 — 2026-11-18

### Database upgrades

- New databases use schema v2. Supported existing databases migrate automatically before the server starts accepting requests.
- Startup backs up the stopped database, migrates a separate copy, and commits only after runtime initialization succeeds. Failed or interrupted upgrades restore the original data when recovery can establish its identity and stop startup with an error. Committed upgrades are never rolled back over subsequent writes.
- Migration preserves existing memory IDs, namespaces, text, vectors and relationships. Older vectors retain unknown model provenance; migration neither guesses their model nor re-embeds them. Keep the existing embedding configuration.
- Schema conversion preserves the existing lifecycle policy. Retrieval, reinforcement and decay metadata are represented separately in the new schema.

- Docker startup relocates existing volume-root databases into a reserved child directory with persistent recovery state, keeping schema-upgrade backups inside the mounted volume.

### Correctness and interoperability

- Improved durable write receipts, retry handling, namespace operations, deletion and recovery across local MCP and HTTP adapters.
- Tightened validation for embedding profiles, structured labels, protocol fields and bounded responses.
- Improved coordination between canonical storage and derived search indexes, including startup recovery and concurrent operations.

### Dependencies and build checks

- Updated ZIP, bundled SQLite/zlib, and selected runtime/network dependencies to address known security advisories. ZIP archive validation and existing backup fixtures remain covered by compatibility tests.
- CI tests are grouped by function: Storage & recovery, API & protocols, Search & embeddings, and Runtime & lifecycle, on both Linux and macOS. Aggregate checks verify complete, non-overlapping coverage.
- Release and container builds use the committed dependency lockfile.

### Upgrade notes

- Stop all processes using the database before upgrading. Allow additional disk space for backup, staging and recovery, and keep the automatic backup until the downgrade window has closed.
- Do not open an upgraded database with a 1.x binary. Downgrading requires restoring the complete pre-upgrade backup directory with the older binary and loses writes made after the upgrade.
- Backup ZIP restore requires matching major/minor versions. A 2.0 binary does not directly restore a 1.0 backup ZIP; JSON/JSONL import is not a ZIP restore bridge.
- The upgrade was exercised with a database created by the released v1.0.0 binary. This does not establish compatibility with every earlier release or damaged database.

See the [upgrade and recovery guide](docs/guide.md#upgrading-to-20) before upgrading. No retrieval-quality or general performance improvement is claimed for this release.
