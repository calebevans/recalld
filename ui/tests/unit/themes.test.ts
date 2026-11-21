// The light and dark themes in docs/ui/tokens.css keep the same rules: the
// dark block redefines every color and nothing else, text stays readable on
// every ground (DESIGN §4, §10), and the colors the map reads are plain hex.
import { readFileSync } from 'node:fs';
import { join } from 'node:path';
import { describe, expect, it } from 'vitest';

const css = readFileSync(join(import.meta.dirname, '../../../docs/ui/tokens.css'), 'utf8').replace(/\/\*[\s\S]*?\*\//g, '');

function block(selector: string): Record<string, string> {
  const start = css.indexOf(`${selector} {`);
  expect(start, selector).toBeGreaterThanOrEqual(0);
  const body = css.slice(css.indexOf('{', start) + 1, css.indexOf('}', start));
  return Object.fromEntries([...body.matchAll(/(--[\w-]+):\s*([^;]+);/g)].map((m) => [m[1], m[2].trim()]));
}

const themes = { light: block(':root'), dark: block(":root[data-theme='dark']") };
const isColor = (name: string) => /^--(color|band|shadow)-/.test(name);

function luminance(hex: string): number {
  const [r, g, b] = [1, 3, 5].map((i) => parseInt(hex.slice(i, i + 2), 16) / 255).map((c) => (c <= 0.04045 ? c / 12.92 : ((c + 0.055) / 1.055) ** 2.4));
  return 0.2126 * r + 0.7152 * g + 0.0722 * b;
}
function contrast(a: string, b: string): number {
  const [hi, lo] = [luminance(a), luminance(b)].sort((x, y) => y - x);
  return (hi + 0.05) / (lo + 0.05);
}

describe('themes', () => {
  it('the dark theme redefines every color and nothing else', () => {
    expect(Object.keys(themes.dark).sort()).toEqual(Object.keys(themes.light).filter(isColor).sort());
  });

  describe.each(Object.entries(themes))('%s', (_, t) => {
    const grounds = ['bg', 'surface', 'canvas', 'sunken'];
    const texts = ['text', 'text-2', 'text-muted', 'text-axis', 'accent-text', 'accent-strong', 'suppressed-text'];
    it.each(texts.flatMap((text) => grounds.map((ground) => [text, ground])))('%s is readable on %s', (text, ground) => {
      expect(contrast(t[`--color-${text}`], t[`--color-${ground}`])).toBeGreaterThanOrEqual(4.5);
    });
    it('text on the accent and on a selected background is readable', () => {
      expect(contrast(t['--color-on-accent'], t['--color-accent'])).toBeGreaterThanOrEqual(4.5);
      expect(contrast(t['--color-accent-strong'], t['--color-accent-soft'])).toBeGreaterThanOrEqual(4.5);
      expect(contrast(t['--color-text-2'], t['--color-accent-soft'])).toBeGreaterThanOrEqual(4.5);
    });
    it('the colors the map reads are #RRGGBB', () => {
      // ui/src/lib/map/theme.ts blends them, which needs six hex digits.
      const read = ['node', 'node-fading', 'node-summary', 'node-ghost', 'node-lit', 'edge', 'contradicts', 'permastore', 'text', 'accent',
        'suppressed', 'label-halo', 'canvas-dot', 'canvas', 'agent-1', 'agent-2', 'agent-3', 'agent-4'];
      for (const name of read) expect(t[`--color-${name}`], name).toMatch(/^#[0-9A-Fa-f]{6}$/);
    });
  });
});
