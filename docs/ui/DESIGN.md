# recalld UI — Design spec ("Field Notes")

This is the visual and interaction contract for the optional local recalld UI. It was designed and
approved on a design canvas; the screens are in `reference/` as markup with exact values. Tokens
live in `tokens.css` / `tokens.json`. When this file and a reference screen disagree, **this file
wins**. When something isn't covered here, follow the nearest pattern below rather than inventing a
new one.

## 1. Principles

1. **Calm over clever.** Little prose, few borders, generous space. If a screen feels busy, remove
   things before restyling them.
2. **Honest data.** Every number on screen comes from recalld (or from recalld's own FSRS formula).
   No UI-side modeling, no invented metrics.
3. **Looking never changes memory.** All reads use recalld's read-only paths (`recordAccess:false`,
   daemon `*_read_only` methods). Only explicit buttons (Reinforce, Forget) write.
4. **Local-first.** No network requests beyond the recalld daemon. Fonts are bundled, not fetched.
5. **Fast is a feature.** Interactions respond within one frame; the Memory map holds 60 fps.

## 2. Layout and chrome

```
┌──────────┬──────────────────────────────────────────────┬────────────┐
│ Sidebar  │ Page header: Title (serif 30) ····· Search ⌘K│ Detail     │
│ 208px    │ Toolbar: [scope chip] | filter chips ··· sort│ panel      │
│ (60 rail)│ Content                                      │ 360px      │
│          │                                              │ (56 rail)  │
└──────────┴──────────────────────────────────────────────┴────────────┘
```

- **Page padding** 28px top, 32px sides. Gap between header, toolbar and content 16px (20px on
  Timeline/Health).
- **Sidebar** (`--sidebar-width`, collapses to `--sidebar-rail`): wordmark "recalld" (serif 25,
  weight 500) with a collapse button; views (Memory map, Explore, Timeline, Health); Namespaces list
  ("All namespaces" first, then each namespace with its count); and a status line at the bottom
  (`read-only · daemon connected`, mono 10.5) with the theme button (§4) beside it. Collapsed it
  becomes a rail of 40×40 icon links with tooltips plus an "r" expand button, with the theme button
  at its foot. The collapsed state persists across sessions.
- **Detail panel** (`--panel-width`, min 300): opens when a memory is selected. Memory map: it
  collapses to a 56px rail (green dot plus the vertical short title). Explore: it closes entirely.
  Esc closes it.
- **Namespace scope is global.** One setting, shown in two places: the sidebar list and the first
  chip of every view's toolbar (stacked-layers icon + current scope + chevron). Default is **`default`**
  (lighter once memories live in project namespaces); **All namespaces** is one click away. Changing either updates every view and persists across sessions.
- **Search** sits at the right of every page header: a 44px-tall field with a 1.5px bottom rule, the
  label "Search" in sans 14, and a `⌘K` kbd. ⌘K / Ctrl+K and `/` open the palette from anywhere.
- **Responsive:** below ~900px the sidebar and panel collapse to rails. Below ~600px they stack
  above/below the content. Toolbars wrap. Wide tables scroll horizontally inside their card.

## 3. Typography

Three faces, three jobs. A role never borrows another face. (See `reference/Type.dc.html`.)

| Face | Role | Use for | Never for |
|---|---|---|---|
| **Newsreader** (`--font-serif`) | Voice | Wordmark; page titles (30); the selected memory's title (26 in the panel, 24 elsewhere); section, card and panel sub-titles (18); one large display number per card (44, or 24 in tiles). | Controls, inputs, lists, anything under 18px. |
| **Instrument Sans** (`--font-sans`) | Interface | Everything read in a list or touched: memory summaries in tables, results, map labels and tooltips; body (13.5); captions and meta lines (12.5); chips and buttons (12.5 / 13); inputs (14; palette 17); nav; legends. | — |
| **IBM Plex Mono** (`--font-mono`) | Values | What recalld computes or identifies: R, S, scores, counts, sizes, durations, timestamps, IDs, paths, namespace names *inside data* (table cells, tooltips, result tags), keyboard shortcuts. Also uppercase micro-labels (10.5, +0.06em) and one-line status lines above a title (11). | Sentences. |

Tie-breakers:
- **Same word, same face.** A memory summary is sans everywhere except as the selected memory's title.
- **Numbers in a sentence stay sans** ("16 links"). Numbers on their own — table cells, readouts,
  charts — are mono.
- **At most one serif title and one serif number per card or region.**
- Weights: 400 text, 500 controls and the wordmark, 600 only for the primary action, active nav
  item and seed labels on the map.
- Bundle fonts locally with `@fontsource/newsreader`, `@fontsource/instrument-sans` and
  `@fontsource/ibm-plex-mono` (latin subsets, the weights above only).

## 4. Color

- Ground `--color-bg`; cards, panels and popovers `--color-surface` with a 1px `--color-border`.
  No drop shadows on cards; only the palette and tooltips are elevated.
- **Green is an accent, not a wash.** `--color-accent` marks the primary action, the recall light,
  decay curves, "live" dots and selected-state text. Selected backgrounds use
  `--color-accent-soft` (near-neutral). Small green text uses `--color-accent-text`.
- Text: `--color-text` for primary, `--color-text-2` for secondary, `--color-text-muted` for meta.
  Every text color passes 4.5:1 on both the bg and the surface.
- Phases are never told apart by hue alone — see §6.
- Destructive actions (Forget) live behind the "More actions" (⋯) menu and confirm in a dialog.
  There is no red in the base palette.
- **Two themes, one set of names.** `tokens.css` defines every color twice: the light theme on
  `:root` and the dark theme on `:root[data-theme='dark']`. Nothing but color differs, and components
  never name a theme. In the dark theme each role keeps its meaning with the lightness reversed:
  near-black grounds, eggshell text, a pale sage accent (so the primary button carries dark text),
  and a memory that is lightest while Full and strong and dims toward the canvas as it fades.
- **The theme follows the system** (`prefers-color-scheme`) until the viewer picks the other one with
  the theme button: an icon button with a moon, `aria-label="Dark theme"` and `aria-pressed`, beside
  the sidebar's status line and at the foot of the rail. The choice persists across sessions; picking
  the system's own theme again clears it, so the UI goes back to following the system. `app.html`
  sets `data-theme` on `<html>` before first paint, so a page never flashes the other theme, and the
  Memory map reads the tokens again when the theme changes.

## 5. Components

| Component | Spec |
|---|---|
| **Filter chip** | Height 34, pill, 0 12px. On: `accent-soft` bg, `accent-soft-border`, `accent-strong` text, `aria-pressed="true"`. Off: transparent, `border`, `text-2`. Sans 12.5/500. |
| **Menu chip** | Same as an off filter chip + 14px chevron; opens a popover listbox (Tags, Entities, Agent, Links). |
| **Scope chip** | Like a menu chip but always on a `surface` bg with `accent-soft-border`; layers icon, label = current scope. |
| **Toolbar divider** | 1×20px `border`, 4px side margin, between the scope chip and the filters. |
| **Primary button** | Height 44, radius 6, `accent` bg, `on-accent` text, sans 13/600. One per region. |
| **Secondary button** | Height 44, 1px `border-strong`, transparent, sans 13/500. |
| **Icon button** | 36×36 (rails: 40×40), radius 6, transparent, `aria-label` required. |
| **Search field** | See §2. In the palette: 60px row, search icon, sans 17 input, `Esc` kbd. |
| **kbd** | Mono 11, 1px `border-strong`, radius 4, padding 1px 6px. |
| **Card** | `surface`, 1px `border`, radius 6, padding 22. Title serif 18; optional right-aligned meta (mono 12 or sans 12.5). |
| **Stat tile** | `sunken` bg, radius 6, padding 14 16: label sans 12 muted / value serif 24 / caption sans 12 muted (mono only if the caption is a value or identifier). |
| **Table** | Inside a card. Sticky header: mono 10.5 uppercase muted, 10px vertical padding. Rows: 44px, 1px `divider` between them; the whole first cell is a `<button>`; the selected row gets an `accent-soft` bg. Numbers mono 12. Strength cell: 56×4 bar (`border` track, `accent` fill) + mono value. |
| **Status line** | Mono 11, `accent-text`, with a 6px accent dot, above a title (e.g. "Full · recalled just now"). |
| **Decay curve** | SVG, 96–132px tall. Bands: `band-full` above 0.70, `band-summary` from 0.30–0.70; dashed `grid` lines at 0.70/0.30 with mono 9 labels. Curve 2px `accent`. A "now" vertical line (accent at 45%) and dot. After a recall, also draw the previous curve dashed in `edge`. X labels "now", "+6 mo", "+1 yr". |
| **Tooltip (map)** | `surface`, 1px `border`, radius 7, `--shadow-tooltip`, padding 13. Line 1: full summary, sans 12.5/600. Lines 2–3: mono 11 muted ("Full · R 0.99 · S 2 mo", "default · 4 links · accessed 3d ago"). Shows after `--dur-fast`, flips sides near edges. |
| **⌘K palette** | 680px max width, 104px from the top, radius 10, `--shadow-popover`, over a `--color-scrim`. Sections: input row; scope chip row with "Browsing won't reinforce memories" on the right; results grouped as **Best matches** and **Through links** (uppercase mono labels); footer of key hints plus a result count. |
| **Result row** | Button, radius 6, padding 10. Summary (sans, single line, ellipsis) · namespace tag (mono 11) · optional "contradicted" tag (1px dashed `contradicts`) · score bar 44×4 + mono score. The highlighted row gets `accent-soft` and shows the explain line under it (mono 11.5: "vector 0.86 · text +0.03 · entities +0.02"). Linked results show "via ⟨seed⟩" and use a moss score bar. |
| **Legend** | Sans 12 muted, 8px swatches, 14px gaps, bottom-left of the map. |

Icons: 24px-grid stroke icons at 1.6–1.8 stroke, `currentColor`, round caps (Lucide works). Never emoji.

## 6. Memory map — visual encoding

| Thing | Encoding |
|---|---|
| Node size | `2.4 + 1.9 × ln(1 + degree)` px at zoom 1 |
| Full | filled `--color-node`, opacity 0.82 |
| Summary | filled `--color-node`, opacity 0.38 |
| Ghost | 1px ring, no fill, opacity 0.5 (it has no summary — tooltip says "Summary dropped") |
| Tombstone | hidden by default; when shown, 1.6px dot at 0.35 |
| Permastore | extra 1px `--color-permastore` ring at r + 3.2 |
| Lit by recall | `--color-node-lit` fill + radial glow (accent, 0.3 → 0.12 → 0) |
| Returned result | 1.3px accent ring at r + 4.5 |
| Suppressed (RIF) | `--color-suppressed` fill + soft cool glow |
| Selected | 1.6px `--color-text` ring at r + 7 |
| Hovered | 1.5px `--color-text` ring at r + 5; its edges darken to `text` at 55%; tooltip |
| Edges | all solid 1px except contradicts, dashed (3 3) in `--color-contradicts`; supersedes slightly stronger. Opacity `0.32 × (0.3 + 0.7 × weight)`; selecting a memory darkens its links and fades everything outside its neighborhood. **Only associative edges show by default**; the Links menu adds the other types. The map shows memories active in the last 90 days by default (Active in: 7 days, 30 days, 90 days, 1 year, All time); hidden memories leave the layout entirely, not just the view. Contradicts edges never carry light. |

**Labels:** sans 11.5, 4px `--color-label-halo` stroke. By default only memories returned by the
current or most recent recall are labeled, plus the hovered and selected ones. Truncate at a word
boundary to ≈26 characters with "…". **Labels never overlap:** place greedily in priority order
(seeds, results, selection, hover); try below, right, left, above and two diagonals; skip any spot
that overlaps a placed label (4px margin) or leaves the viewport; prefer the spot covering the fewest
nodes; drop the label if nothing fits. Zooming in progressively labels more nodes under the same rule.

**Feel:** the force simulation runs in a Web Worker (d3-force: many-body with Barnes–Hut, link,
collide, gentle centering), so the main thread never computes physics. Positions are cached per
memory in IndexedDB, so the map opens settled. New memories appear next to their neighbors. Dragging
a node reheats only its neighborhood (alpha target ≈ 0.15, decaying). Hover dimming eases over
`--dur-fast`.

## 7. Live activity — the recall light

The light shows recalld's actual spreading-activation path, from the daemon's event stream:

1. **Seed bloom.** Matched memories bloom: glow attack 80ms, decay 1,200ms, intensity = seed score.
2. **Pulses.** A comet (bright head, 40% tail) travels along each traced edge in firing order,
   ≈350ms per hop, scaled by edge length. Width and brightness = spread amount. Light fades with each hop.
3. **Convergence.** A node reached by several paths sums its glow (clamped).
4. **Results.** Returned memories keep a ring for 4s. Ones found through links get the softer moss tint.
5. **Suppression.** Retrieval-induced-forgetting competitors dim and cool over 600ms, then recover.
6. **Store.** A new node fades in at its neighbors' centroid; its auto-links draw in over 300ms.
   **Reinforce:** a pulse plus a ring that grows with the stability gain. **Phase change:** the node
   eases to its new encoding.
7. **Agent color.** Pulses use the agent's color (`--color-agent-n`, assigned in order of first appearance).

Rules: pulses and glows animate in shaders from start time and duration, so the CPU uploads once per
event, never per frame. Merge events that arrive in the same frame; cap concurrent pulses around
1,500; limit glow brightness when bursts overlap so it stays calm, never flashy. Animate only while
something is lit; otherwise draw on demand. With `prefers-reduced-motion`, nodes flash but nothing
travels. Off-screen activity shows as a small edge indicator. The last ~300 events are kept for
**Replay** (slow-motion, from the activity list).

## 8. Screens

All screens share the chrome in §2. Every list and chart follows the namespace scope. Each screen has
three states: **loading** (skeleton rows/blocks in `divider`, no spinners), **empty** (one sentence
plus one action) and **daemon unavailable** (a banner with "Start daemon" and the socket path).

### Memory map (`/map`, default route) — `reference/FieldNotes.dc.html`
Toolbar: scope chip | Full · Summary · Ghost chips | "Links · 3 types" menu | right: **Decay**
time-scrubber (0 → +1 yr; projects each node's R forward with the FSRS formula and re-encodes phases
live). Canvas: dot grid, a live pill top-left ("● claude-code · recall · now"), legend bottom-left,
zoom +/−/fit bottom-right. Click a node to select it (panel shows summary, meta, "Show full text"
for Full-phase memories, decay curve, the last 3 activity rows, and Reinforce / Local graph / ⋯).
Double-click focuses its 2-hop neighborhood.

### Explore (`/explore`) — `reference/Explore.dc.html`
Title + count. Toolbar: scope | Full · Summary · Ghost | Tags · Entities · Agent menus | right: Sort
(Last access ▾). Table columns: Memory · Phase · Strength · Next change · Namespace · Accessed.
Ghost rows show "Summary dropped" in italic muted text. Virtualized (TanStack Virtual) for 100k rows.
Clicking a row opens the panel (with "Show on map"); ↑/↓ move the selection. Sort and filter state
lives in the URL.

### Search (⌘K overlay) — `reference/Search.dc.html`
Uses **recalld's existing hybrid search** (vector + FTS + entity recall + spreading activation),
read-only, with `explain: true`. There is no UI-side search index. For "All namespaces", run one
search per namespace in parallel and merge: by score when namespaces share an embedding profile,
grouped by namespace when they don't. ↵ opens the memory in Explore; ⌘↵ shows it on the map; Tab
moves to the scope chip. Debounce 120ms; show the last results while the next query runs.

### Timeline (`/timeline`) — `reference/Timeline.dc.html`
Scope chip. Card: range segmented control (30 days · 90 days · 1 year · All time) + legend. Chart:
memories stored per bucket, stacked by **current** phase above the axis; recalls per bucket mirrored
below in moss. Buckets are weekly, monthly when the range exceeds 2 years. Below: an activity stream
grouped by day ("Today", "Nov 14"); rows are time (mono) · dot (filled = recall, ring = store,
grey ring = sweep) · "agent verb" muted + subject · right meta.

### Health (`/health`) — `reference/Health.dc.html`
Row 1: **Memories** card (display number, phase bar, Full/Summary/Ghost/Tombstone/Permastore counts)
and a **Coming up** card (phase changes expected in 7 / 30 / 90 days, straight from
`/health/report.decayForecast`; zeros greyed). Row 2: **Fading soon** (soonest transitions, with
Reinforce on each) and **Storage** (stacked bar + meta.db / vectors / edges.db / text log). Row 3:
**System** — tiles for Version, Uptime, Memory (RSS), Embedding latency;
per-subsystem status dots; connected agents; next decay sweep.

## 9. Words

- View names: **Memory map**, **Explore**, **Timeline**, **Health**, **Search**. Routes `/map`,
  `/explore`, `/timeline`, `/health`.
- Use recalld's terms exactly: Full, Summary, Ghost, Tombstone, Permastore; strength (R), stability
  (S); recall, store, reinforce, forget, decay sweep; namespace.
- Relative times: "just now", "14m ago", "3d ago", "2w ago", "4mo ago". Durations: "16 d", "3 mo", "4 yr".
- Sentence case everywhere. No exclamation marks. No emoji.

## 9a. Logo and favicon

- **Mark:** a simple lightbulb: a slightly hand-drawn outline in `--color-accent-strong` (1.8 stroke)
  over cream glass `#F3EEDC`, one `--color-accent` dot inside (a memory), and a single base line.
  Nothing else. Files: `ui/static/logo.svg` (light) and `ui/static/favicon.svg` (switches to
  light-on-dark in dark mode). In the dark theme the sidebar mark takes the favicon's dark variant
  through the same three tokens.
- **Sidebar:** the 26px mark + 8px gap + "recalld" wordmark (serif 25/500). Collapsed rail: the 24px
  mark is the expand button (`aria-label="Expand sidebar"`).
- **Browser tab:** `favicon.svg` with fallbacks `favicon.ico` (16+32), `favicon-32.png` and
  `apple-touch-icon.png` (180, on an off-white tile). The page `<title>` is "recalld · ⟨View⟩".
- Keep it as drawn: don't recolor it per theme beyond the favicon's dark variant, don't add text
  inside it, and keep at least 4px of space around it.

## 10. Accessibility

Real `<button>` / `<a>` / `<input>` + `<label>` elements; icon-only buttons have `aria-label`;
toggles use `aria-pressed`; the palette is `role="dialog"` with a `listbox`; the map canvas has an
`aria-label` and an off-screen list of the visible nodes, so keyboard users can Tab through results.
Focus rings: 2px `--color-accent` outline, 2px offset. Text contrast ≥ 4.5:1 in both themes
(`ui/tests/unit/themes.test.ts` checks every text color on every ground). Hit targets ≥ 34px in
toolbars and 44px for primary actions.

## 11. Reference files

`reference/*.dc.html` are the approved screens as design-canvas source. They use a design-tool
runtime (`support.js`, `{{holes}}`, `<sc-if>`/`<sc-for>`), so they **won't run as-is**. Read them for
layout, exact spacing, sizes and colors; don't copy their structure into the app. The data in them is
sample data.
