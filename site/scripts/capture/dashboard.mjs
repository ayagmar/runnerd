// Screenshots of the dashboard against the demo daemon, light and dark, into a directory capture.sh passes on. Reads
// RUNNERD_CAPTURE_LOGIN (the demo's one-time login link) from capture.sh.
import { readFileSync, mkdirSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { chromium } from 'playwright-core';

const login = readFileSync(process.env.RUNNERD_CAPTURE_LOGIN, 'utf8').trim().split('\n')[0];
const out = join(dirname(process.env.RUNNERD_CAPTURE_LOGIN), 'shots');
mkdirSync(out, { recursive: true });
const executablePath = process.env.CHROMIUM || undefined;
const browser = await chromium.launch({ executablePath });

// Pages for the docs: name, path, the shot's height in CSS pixels by size, and a button to press first. Desktop is
// 1280 wide at 1.5x, phone 390 at 2x; a page taller than its shot is cut, since a whole history is a wall of rows.
const pages = [
  { name: 'overview', path: '/', heights: { desktop: 1720, phone: 1200 } },
  // One phone screen of the overview, tab bar included, for the landing page beside the TUI's frame: its 390 by 731
  // is the phone box's inside where the TUI's 96 by 26 frame is 526 px tall.
  { name: 'screen', path: '/', heights: { phone: 731 } },
  { name: 'timeline', path: '/history?lens=timeline', heights: { desktop: 520 } },
  { name: 'insights', path: '/insights', heights: { desktop: 760 } },
  { name: 'job-failed', path: '/jobs/9401', heights: { desktop: 1840 } },
  { name: 'cancel', path: '/jobs/120055991700', heights: { desktop: 440 }, press: '[data-asks="cancel"]' },
];
// Parts of pages for the landing page, each cut to one element: at 1000 wide and 2x, so they stay legible at about
// half the page's width, and at a phone's width for phones.
const parts = [
  { name: 'part-attention', path: '/', element: '#attention' },
  { name: 'part-failure', path: '/jobs/9401', element: '#failure' },
  { name: 'part-timeline', path: '/history?lens=timeline', element: '.timeline' },
  { name: 'part-cancel', path: '/jobs/120055991700', element: '#job-action', press: '[data-asks="cancel"]' },
];
const sizes = {
  desktop: { width: 1280, height: 900, scale: 1.5, suffix: '' },
  phone: { width: 390, height: 844, scale: 2, suffix: '-phone' },
  part: { width: 1000, height: 900, scale: 2, suffix: '' },
};

// The login link works once: sign in once, and give every context the session cookie it sets.
const signIn = await browser.newContext();
await (await signIn.newPage()).goto(login, { waitUntil: 'networkidle' }).catch(() => {});
const cookies = await signIn.cookies();
await signIn.close();

// The viewport is as tall as the shot, so what the page fixes to the viewport's bottom, such as the phone's tab bar,
// is at the shot's bottom.
async function open(size, theme, path, press, height = sizes[size].height) {
  const { width, scale } = sizes[size];
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
  if (press) {
    await page.click(press);
    await page.waitForTimeout(1200);
  }
  return { context, page };
}

for (const theme of ['light', 'dark']) {
  for (const { name, path, heights, press } of pages) {
    for (const [size, height] of Object.entries(heights)) {
      const { context, page } = await open(size, theme, path, press, height);
      await page.screenshot({
        path: join(out, `${name}${sizes[size].suffix}-${theme}.png`),
        clip: { x: 0, y: 0, width: sizes[size].width, height: Math.min(height, await page.evaluate(() => document.documentElement.scrollHeight)) },
        fullPage: true,
      });
      await context.close();
    }
  }
  for (const { name, path, element, press } of parts) {
    for (const size of ['part', 'phone']) {
      const { context, page } = await open(size, theme, path, press);
      await page.locator(element).first().screenshot({ path: join(out, `${name}${sizes[size].suffix}-${theme}.png`) });
      await context.close();
    }
  }
}
await browser.close();
