# ui/ — recalld local UI (frontend)

Read before changing anything here: `../docs/ui/DESIGN.md` (look and behavior), `../docs/ui/SPEC.md`
(architecture and phases), `../docs/ui/tokens.css` (values). Reference screens are in
`../docs/ui/reference/`. They're design-tool markup: read them for exact layout, spacing and colors,
and don't copy their structure.

Hard rules:
- **Values come from tokens.** No raw hex, font names or magic pixel sizes in components; use
  `var(--…)` from `tokens.css`. To add a value, add a token first.
- **Colors have two values.** A color token needs a light value (`:root`) and a dark one
  (`:root[data-theme='dark']`), each readable on its grounds; components never name a theme.
- **Type roles are fixed:** Newsreader = titles and display numbers (≥ 18px); Instrument Sans =
  interface (lists, controls, inputs, body); IBM Plex Mono = values (numbers, IDs, timestamps,
  shortcuts, uppercase micro-labels). Never use mono for sentences or serif for controls.
- **Read-only by default.** Every read uses `recordAccess:false` / `*_read_only`. Only Reinforce and
  Forget write; Forget confirms. Add a test whenever you add a read path.
- **No UI-side search or data store.** Search goes through recalld's hybrid search; the IndexedDB
  cache holds map *positions* only.
- **The main thread never runs physics.** d3-force lives in the worker; animation lives in shaders.
- **Keep within the performance budgets** in SPEC §6, and report numbers in the PR for map or light
  changes.
- **Words:** Memory map, Explore, Timeline, Health, Search; recalld's own terms (Full, Summary,
  Ghost, Tombstone, Permastore, strength, stability). Sentence case, no emoji.
- **Fonts are bundled** with `@fontsource`; no network requests except to the local recalld API.

Commands: `pnpm dev` (proxies `/api/ui` to a running `recalld ui --port 7690`), `pnpm build`
(outputs to `dist/`, which the `ui` cargo feature embeds), `pnpm test`, `pnpm test:e2e`, `pnpm perf`.
