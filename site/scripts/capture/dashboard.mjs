// Screenshots of the dashboard against the demo daemon, light and dark, into a directory capture.sh passes on. Reads
// RUNNERD_CAPTURE_LOGIN (the demo's one-time login link) and RUNNERD_CAPTURE_IDLE_LOGIN (the same for a second demo
// in its idle scene) from capture.sh.
import { readFileSync, mkdirSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { chromium } from 'playwright-core';

const link = (file) => readFileSync(file, 'utf8').trim().split('\n')[0];
const login = link(process.env.RUNNERD_CAPTURE_LOGIN);
const idleLogin = link(process.env.RUNNERD_CAPTURE_IDLE_LOGIN);
const out = join(dirname(process.env.RUNNERD_CAPTURE_LOGIN), 'shots');
mkdirSync(out, { recursive: true });
const executablePath = process.env.CHROMIUM || undefined;
const browser = await chromium.launch({ executablePath });

// Pages for the docs: name, path, the shot's height in CSS pixels by size, a button to press first, and `idle` for the
// idle demo. Desktop is 1280 wide at 1.5x, phone 390 at 2x; a page taller than its shot is cut, since a whole history
// is a wall of rows.
const pages = [
  { name: 'overview', path: '/', heights: { desktop: 1440, phone: 1200 } },
  { name: 'overview-idle', path: '/', heights: { desktop: 720, phone: 1250 }, idle: true },
  // One phone screen of the overview scrolled to the budget, tab bar included, for the landing page beside the TUI's
  // frame: its 390 by 731 is the phone box's inside where the TUI's 96 by 26 frame is 526 px tall.
  { name: 'screen', path: '/', heights: { phone: 731 }, scrollTo: '#budget' },
  { name: 'timeline', path: '/history?lens=timeline', heights: { desktop: 520 } },
  { name: 'insights', path: '/insights', heights: { desktop: 760 } },
  { name: 'job-failed', path: '/jobs/9401', heights: { desktop: 1840 } },
  { name: 'cancel', path: '/jobs/120055991700', heights: { desktop: 440 }, press: '[data-asks="cancel"]' },
  { name: 'settings', path: '/config', heights: { desktop: 900, phone: 1400 } },
  // Jobs opens on its 30 days and its search, then the rows; a few rows say the rest.
  { name: 'jobs', path: '/history', heights: { desktop: 900, phone: 1300 } },
  { name: 'sizing', path: '/sizing', heights: { desktop: 680, phone: 1300 } },
  { name: 'caches', path: '/caches', heights: { desktop: 1180, phone: 1300 } },
];
// Parts of pages for the landing page, each cut to the box around its elements: at 1000 wide and 2x, so they stay
// legible at about half the page's width, and at a phone's width for phones.
const parts = [
  { name: 'part-lanes', path: '/', elements: ['#health', '.machine'] },
  { name: 'part-queue', path: '/', elements: ['#queue'] },
  { name: 'part-disk', path: '/insights', elements: ['section.disk'] },
  { name: 'part-failure', path: '/jobs/9401', elements: ['#failure'] },
  { name: 'part-timeline', path: '/history?lens=timeline', elements: ['.timeline'] },
  { name: 'part-cancel', path: '/jobs/120055991700', elements: ['#job-action'], press: '[data-asks="cancel"]' },
];
const sizes = {
  desktop: { width: 1280, height: 900, scale: 1.5, suffix: '' },
  phone: { width: 390, height: 844, scale: 2, suffix: '-phone' },
  part: { width: 1000, height: 900, scale: 2, suffix: '' },
};

// The login link works once: sign in once, and give every context the session cookie it sets.
async function signIn(link) {
  const context = await browser.newContext();
  await (await context.newPage()).goto(link, { waitUntil: 'networkidle' }).catch(() => {});
  const cookies = await context.cookies();
  await context.close();
  return cookies;
}
const cookies = await signIn(login);
const idleCookies = await signIn(idleLogin);

// The viewport is as tall as the shot, so what the page fixes to the viewport's bottom, such as the phone's tab bar,
// is at the shot's bottom.
async function open(size, theme, path, press, height = sizes[size].height, idle = false) {
  const { width, scale } = sizes[size];
  const context = await browser.newContext({
    viewport: { width, height },
    deviceScaleFactor: scale,
    colorScheme: theme,
    reducedMotion: 'reduce',
  });
  await context.addCookies(idle ? idleCookies : cookies);
  const page = await context.newPage();
  await page.goto(new URL(path, idle ? idleLogin : login).href, { waitUntil: 'load' });
  await page.waitForTimeout(2500);
  if (press) {
    await page.click(press);
    await page.waitForTimeout(1200);
  }
  return { context, page };
}

for (const theme of ['light', 'dark']) {
  for (const { name, path, heights, press, idle, scrollTo } of pages) {
    for (const [size, height] of Object.entries(heights)) {
      const { context, page } = await open(size, theme, path, press, height, idle);
      if (scrollTo) {
        // One screen as a reader sees it there: the element 16 px under the viewport's top, or under a header the page
        // keeps at the top.
        await page.locator(scrollTo).first().evaluate((el) => {
          const header = [...document.querySelectorAll('body *')].find((h) => {
            const { position } = getComputedStyle(h);
            return (position === 'sticky' || position === 'fixed') && h.getBoundingClientRect().top <= 0 && h.offsetHeight < 200;
          });
          window.scrollTo(0, el.getBoundingClientRect().top + window.scrollY - 16 - (header?.offsetHeight ?? 0));
        });
        await page.waitForTimeout(300);
        await page.screenshot({ path: join(out, `${name}${sizes[size].suffix}-${theme}.png`) });
        await context.close();
        continue;
      }
      await page.screenshot({
        path: join(out, `${name}${sizes[size].suffix}-${theme}.png`),
        clip: { x: 0, y: 0, width: sizes[size].width, height: Math.min(height, await page.evaluate(() => document.documentElement.scrollHeight)) },
        fullPage: true,
      });
      await context.close();
    }
  }
  for (const { name, path, elements, press } of parts) {
    for (const size of ['part', 'phone']) {
      const { context, page } = await open(size, theme, path, press);
      // A cut-out has no viewport, so what the page fixes to one, such as the phone's tab bar, would land mid-shot.
      await page.evaluate(() => {
        for (const el of document.querySelectorAll('body *')) {
          if (getComputedStyle(el).position === 'fixed') el.style.visibility = 'hidden';
        }
      });
      const boxes = [];
      for (const element of elements) {
        const box = await page.locator(element).first().boundingBox();
        if (!box) throw new Error(`${name}: no ${element} on ${path}`);
        boxes.push(box);
      }
      // Page coordinates: the boxes are the viewport's, and the page has not scrolled.
      const x = Math.min(...boxes.map((b) => b.x));
      const y = Math.min(...boxes.map((b) => b.y));
      const clip = {
        x,
        y,
        width: Math.max(...boxes.map((b) => b.x + b.width)) - x,
        height: Math.max(...boxes.map((b) => b.y + b.height)) - y,
      };
      await page.screenshot({ path: join(out, `${name}${sizes[size].suffix}-${theme}.png`), clip, fullPage: true });
      await context.close();
    }
  }
}
await browser.close();
