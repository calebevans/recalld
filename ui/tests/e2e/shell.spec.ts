import { expect, test } from '@playwright/test';
import { open } from '../support/ui';
import { centrePixel } from '../support/pixel';

test('signs in with the launch token and lists namespaces from the daemon', async ({ page }) => {
  await open(page, '/explore');
  await expect(page).toHaveTitle('recalld · Explore');
  const scope = page.getByRole('group', { name: 'Namespace scope' });
  await expect(scope.getByRole('button', { name: /All namespaces\s*135/ })).toBeVisible();
  await expect(scope.getByRole('button', { name: /project-recalld\s*98/ })).toBeVisible();
  await expect(scope.getByRole('button', { name: /topic-rust\s*12/ })).toBeVisible();
  await expect(page.getByText('read-only · daemon connected')).toBeVisible();
});

test('the API refuses requests without the session cookie', async ({ browser }) => {
  const ctx = await browser.newContext();
  const res = await ctx.request.get(new URL('/api/ui/namespaces', process.env.RECALLD_UI_URL!).toString());
  expect(res.status()).toBe(403);
  await ctx.close();
});

test('a fresh browser starts on the default namespace', async ({ page }) => {
  const url = new URL(process.env.RECALLD_UI_URL!);
  await page.goto(url.toString());
  await page.goto(new URL('/explore', url.origin).toString());
  await expect(page.getByRole('toolbar', { name: 'Filters' }).getByRole('button', { name: /default/ })).toBeVisible();
  await expect(page.getByText('25 memories')).toBeVisible();
});

test('namespace scope is global and persists', async ({ page }) => {
  await open(page, '/explore');
  await page.getByRole('group', { name: 'Namespace scope' }).getByRole('button', { name: /topic-rust/ }).click();
  await expect(page.getByRole('toolbar', { name: 'Filters' }).getByRole('button', { name: /topic-rust/ })).toBeVisible();
  await expect(page.getByText('12 memories')).toBeVisible();
  await page.reload();
  await expect(page.getByText('12 memories')).toBeVisible();
  await page.getByRole('group', { name: 'Namespace scope' }).getByRole('button', { name: /All namespaces/ }).click();
});

test('the sidebar collapses to a rail and remembers it', async ({ page }) => {
  await open(page, '/map');
  await page.getByRole('button', { name: 'Collapse sidebar' }).click();
  await expect(page.getByRole('button', { name: 'Expand sidebar' })).toBeVisible();
  await page.reload();
  await expect(page.getByRole('button', { name: 'Expand sidebar' })).toBeVisible();
  await page.getByRole('button', { name: 'Expand sidebar' }).click();
});

test('the theme follows the system, and the sidebar button switches and remembers it', async ({ page }) => {
  await page.emulateMedia({ colorScheme: 'dark' });
  await open(page, '/map');
  const html = page.locator('html');
  const button = page.getByRole('button', { name: 'Dark theme' });
  await expect(html).toHaveAttribute('data-theme', 'dark');
  await expect(button).toHaveAttribute('aria-pressed', 'true');

  // The map draws with the theme's colors: a memory is lighter than the dark
  // canvas and darker than the light one. Sample both as a user sees them.
  await page.waitForFunction(() => (window as unknown as { __recalldSettled?: boolean }).__recalldSettled === true, null, { timeout: 60_000 });
  await page.mouse.move(0, 0);
  const brightness = async (x: number, y: number) =>
    centrePixel(await page.screenshot({ clip: { x: x - 1, y: y - 1, width: 3, height: 3 } })).reduce((a, c) => a + c, 0);
  const memoryMinusCanvas = async () => {
    await page.waitForTimeout(500);
    const at = await page.evaluate(() => {
      const node = (window as unknown as { __recalldNodeAt: () => { x: number; y: number } | null }).__recalldNodeAt()!;
      const box = document.querySelector('canvas.sigma-stage')!.getBoundingClientRect();
      return { node: { x: Math.round(node.x), y: Math.round(node.y) }, corner: { x: Math.round(box.right - 6), y: Math.round(box.top + 6) } };
    });
    return (await brightness(at.node.x, at.node.y)) - (await brightness(at.corner.x, at.corner.y));
  };
  expect(await memoryMinusCanvas()).toBeGreaterThan(60);

  // Choosing light on a dark system recolors the map at once and is remembered.
  await button.click();
  await expect(html).toHaveAttribute('data-theme', 'light');
  await expect(button).toHaveAttribute('aria-pressed', 'false');
  expect(await memoryMinusCanvas()).toBeLessThan(-60);
  await page.reload();
  await expect(html).toHaveAttribute('data-theme', 'light');
  await expect(button).toHaveAttribute('aria-pressed', 'false');

  // Going back to the system's own theme forgets the choice.
  await button.click();
  await expect(html).toHaveAttribute('data-theme', 'dark');
  expect(await page.evaluate(() => localStorage.getItem('recalld.theme'))).toBeNull();
});
