// Screenshots of the dashboard against the demo daemon, light and dark, desktop and phone, into $SHOTS or a directory
// capture.sh passes on. Reads RUNNERD_CAPTURE_LOGIN (the demo's one-time login link) from capture.sh.
import { readFileSync, mkdirSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { chromium } from 'playwright-core';

const login = readFileSync(process.env.RUNNERD_CAPTURE_LOGIN, 'utf8').trim().split('\n')[0];
const out = join(dirname(process.env.RUNNERD_CAPTURE_LOGIN), 'shots');
mkdirSync(out, { recursive: true });
const executablePath = process.env.CHROMIUM || undefined;
const browser = await chromium.launch({ executablePath });
// name, path, height of the shot in CSS pixels, by size. Desktop is 1280 wide at 1.5x, phone 390 at 2x; a page taller
// than its shot is cut, since a screenshot of the whole history would be a wall of rows.
const pages = [
  ['overview', '/', { desktop: 1353, phone: 800 }],
  ['history', '/history', { desktop: 733 }],
  ['insights', '/insights', { desktop: 627 }],
  // The job page's exact times, where "Seen by runnerd" is listed, start closed.
  ['job', '/jobs/120055991700', { desktop: 1290 }, 'details.marks summary'],
];
const sizes = { desktop: { width: 1280, scale: 1.5, suffix: '' }, phone: { width: 390, scale: 2, suffix: '-phone' } };
// The login link works once: sign in once, and give every context the session cookie it sets.
const signIn = await browser.newContext();
await (await signIn.newPage()).goto(login, { waitUntil: 'networkidle' }).catch(() => {});
const cookies = await signIn.cookies();
await signIn.close();
for (const theme of ['light', 'dark']) {
  for (const [size, { width, scale, suffix }] of Object.entries(sizes)) {
    for (const [name, path, heights, open] of pages) {
      const height = heights[size];
      if (!height) continue;
      const context = await browser.newContext({
        viewport: { width, height },
        deviceScaleFactor: scale,
        colorScheme: theme,
        reducedMotion: 'reduce',
      });
      await context.addCookies(cookies);
      const page = await context.newPage();
      await page.goto(new URL(path, login).href, { waitUntil: 'load' });
      await page.waitForTimeout(2500);
      if (open) await page.click(open);
      await page.waitForTimeout(300);
      await page.screenshot({ path: join(out, `${name}${suffix}-${theme}.png`) });
      await context.close();
    }
  }
}
await browser.close();
