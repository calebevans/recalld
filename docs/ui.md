# recalld local web UI

`recalld ui` is a browser UI for looking at what your agents remember: which memories exist, how strong they are, how they link, and what each recall actually touched. It runs on your machine, talks only to your recalld daemon, and changes nothing unless you press **Reinforce** or **Forget**.

## Getting it

The UI comes with the standard install (`install.sh` and the release archives). It is on by default; turn it off with

```toml
[ui]
enabled = false
```

or `RECALLD_UI_ENABLED=false`. Agents and the MCP server are unaffected either way.

- **From source:** `make release` or `make install` (Node 22.17+ and pnpm), or `pnpm -C ui install && pnpm -C ui build && cargo build --release`. A plain `cargo build` without building the UI first still works; `recalld ui` then shows a page saying the UI wasn't built.
- **Without the UI:** `cargo build --release --no-default-features`. The Docker image is built this way, since a container can't usefully serve a `127.0.0.1` page.

The UI is embedded in the binary, so its version always matches recalld's. Health → System shows it.

## Running it

```sh
recalld ui                 # random free port, opens your browser
recalld ui --port 7690     # a fixed port
recalld ui --no-open       # print the link instead of opening it
```

`recalld ui` connects to the same daemon `recalld mcp` uses and starts it if it isn't running, so it works alongside Claude Code and other agents. The daemon keeps running until you stop it (`recalld daemon stop`); it no longer shuts itself down when idle, so the UI never loses it. `recalld daemon start --idle-timeout <minutes>` brings an idle shutdown back. If a daemon started by a recalld build *without* the UI is already running, the UI says so. Run `recalld daemon stop`, then `recalld ui` again, and the UI-enabled binary starts the daemon.

Press Ctrl-C to stop the UI. The daemon keeps running for your agents.

## What you see

- **Memory map:** every memory as a node, sized by its links and its current strength, and colored by stage: deep green while Full and strong, lighter as it fades, pale sage as a Summary, an outline as a Ghost. When an agent recalls, the map shows recalld's real spreading-activation path: seeds bloom, light travels along the links that were followed, results keep a ring, and memories suppressed by retrieval-induced forgetting cool briefly. Click a memory to bring its links forward, drag nodes, double-click for a 2-hop neighborhood, and use the **Decay** scrubber to project every memory forward up to a year with recalld's FSRS formula. Only associative links show by default; the **Links** menu adds the other types. The map starts with memories active (created or accessed) in the last 90 days, which keeps large stores calm; **Active in** widens it to a year or all time, and opening a memory from Explore or Search that falls outside the window shows all time.
- **Explore:** a fast table of every memory with phase, strength (R), next phase change, namespace and last access. Filter by phase, tag or entity; sort, and share the URL. Click a row to open it in place: full text, labels, the decay curve, activity and its links (click one to open it).
- **Timeline:** memories stored per week (by their phase today) and recalls per week, plus a day-by-day activity stream.
- **Health:** counts by phase, the decay forecast, memories fading soon, storage, and the daemon's system status (version, uptime, memory, embedding latency, subsystem health, connected agents, next decay sweep).
- **Search (⌘K or /):** recalld's own hybrid search (vector, text, entities and graph links), with the score breakdown for each result. **↵** opens a result where you are (on the map it is selected there; in Explore its row opens) and **⌘↵** shows it on the map.

The namespace scope (sidebar or the first toolbar chip) applies to every view and is remembered. It starts on `default`, which is lighter than the whole store once your memories are split into project namespaces; choose **All namespaces** to see everything.

The UI has a light and a dark theme. It follows your system's setting; the moon button at the bottom of the sidebar switches to the other theme and remembers it.

## Looking never changes memory

Every read the UI makes uses recalld's read-only paths (`recordAccess:false` and the daemon's `*_read_only` methods), so browsing, searching or opening a memory never reinforces it, never resets its decay clock and never triggers retrieval-induced forgetting. An integration test checks this on every change. Only **Reinforce** (with a rating) and **Forget** (after a confirmation) write, through the same tool path your agents use.

## Security

- The server binds `127.0.0.1` only.
- Each launch makes a random token. It is in the link `recalld ui` opens or prints, and is swapped for an `HttpOnly`, `SameSite=Strict` cookie, so the URL in your address bar doesn't carry it.
- Requests whose `Host` isn't `127.0.0.1:<port>` or `localhost:<port>` are refused, which blocks DNS-rebinding attacks. There is no CORS.
- Writes also need an `X-Recalld-UI: 1` header, which other sites cannot send.
- The page loads nothing from the network: fonts are bundled, and the content security policy allows connections only to the UI itself.

## Agent names

Activity is labeled with the agent's MCP client name (for example `claude-code`) when the agent's `recalld mcp` process is the UI-enabled build. recalld doesn't store who accessed a memory, so older activity and memories from other clients show "unknown agent".

## Performance

The map renders on the GPU (Sigma.js on WebGL2), and its physics runs in a Web Worker. Node positions are cached in your browser (IndexedDB) so the map opens settled, and nothing redraws while nothing changes. The design budgets and their measurements are in [docs/ui/SPEC.md](ui/SPEC.md) §6 and the pull request that introduced the UI.

## For contributors

The UI lives in `ui/` (SvelteKit, Svelte 5, TypeScript). Its rules are in `ui/CLAUDE.md`; the design and spec are in `docs/ui/`.

- `pnpm -C ui dev` serves the UI with hot reload and proxies the API to `recalld ui --port 7690`. Open the printed link once, then use `http://127.0.0.1:5173`.
- `ui/src/lib/api/types/` is generated from the Rust response types. After changing one, run `UPDATE_UI_TYPES=1 cargo test --features ui ui::wire`. CI fails when the generated types are stale.
- `pnpm -C ui test` (unit), `pnpm -C ui test:e2e` and `pnpm -C ui perf` (Playwright; set `RECALLD_BIN` to a binary built with `--features ui,bench`).
- `recalld bench synthetic` (feature `bench`) generates the 10k-memory / 100k-edge perf store, and `recalld bench embed-stub` provides the deterministic embedder the tests use. Neither touches `~/.recalld` when run from the harness.
