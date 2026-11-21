// DESIGN §6 visual encoding, as graphology attributes for sigma.
import type { MapTheme } from './theme';
import { alpha, mix, solid } from './theme';
import { displaySize } from './protocol';

export const EDGE = { parent: 1, associative: 2, causal: 3, contradicts: 4, entity: 5, temporal: 6, supersedes: 7 } as const;
export const EDGE_LABELS: Record<number, string> = {
  1: 'Parent', 2: 'Associative', 3: 'Causal', 4: 'Contradicts', 5: 'Entity', 6: 'Temporal', 7: 'Supersedes',
};

/** A memory's look by stage (DESIGN §6): deep forest green while Full and
 *  strong, lighter as it fades toward Summary, pale sage once a Summary, a
 *  sage outline as a Ghost. `strength` is retrievability now (0..1). */
export function nodeAttributes(theme: MapTheme, phase: number, degree: number, permastore: boolean, strength = 1) {
  const size = displaySize(degree, strength);
  const r = Math.min(1, Math.max(0, strength));
  switch (phase) {
    case 1: {
      // Full: R 1 → 0.7 runs deep forest → moss, solid → slightly see-through.
      const t = (1 - r) / 0.3;
      return { size, color: solid(theme.canvas, mix(theme.node, theme.nodeFading, t), 0.94 - 0.2 * Math.min(1, t)), ring: alpha(theme.node, 0), visibility: 'visible' as const, permastore };
    }
    case 2: {
      // Summary: R 0.7 → 0.3 runs moss → sage, lighter than Full but never
      // close to the canvas (strength is computed live, so most Summary
      // memories sit toward the light end).
      const t = Math.min(1, Math.max(0, (0.7 - r) / 0.4));
      return { size, color: solid(theme.canvas, mix(theme.nodeFading, theme.nodeSummary, 0.6 * t), 0.9 - 0.15 * t), ring: alpha(theme.node, 0), visibility: 'visible' as const, permastore };
    }
    case 3:
      // Ghost: the lightest stage, with a solid outline, still well clear of the canvas.
      return { size, color: solid(theme.canvas, theme.nodeGhost, 0.45), ring: theme.nodeGhost, visibility: 'visible' as const, permastore };
    default:
      // Tombstones and memories the decay projection deletes.
      return { size: 1.6, color: alpha(theme.nodeGhost, 0.35), ring: alpha(theme.node, 0), visibility: 'hidden' as const, permastore: false };
  }
}

export function edgeAttributes(theme: MapTheme, type: number, weight: number) {
  // Soft solid lines whose opacity carries the weight, so weak links recede
  // and dense clusters read as a haze rather than a scratchy mesh. Only
  // contradictions are dashed: fine dashes in a dense cluster look like noise.
  const a = 0.32 * (0.3 + 0.7 * weight);
  const base = type === EDGE.contradicts ? theme.contradicts : theme.edge;
  const stronger = type === EDGE.supersedes ? 1.25 : 1;
  const dashed = type === EDGE.contradicts ? [3, 3] : null;
  const color = alpha(base, Math.min(1, a * stronger));
  return {
    size: type === EDGE.supersedes ? 1.2 : 1,
    color: dashed ? alpha(base, 0) : color,
    dashColor: dashed ? color : alpha(base, 0),
    dashSize: dashed ? dashed[0] : 0,
    gapSize: dashed ? dashed[1] : 0,
    baseColor: color,
    weight,
  };
}
