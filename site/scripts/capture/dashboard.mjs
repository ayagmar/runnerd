// Screenshots of the dashboard against the demo daemon, light and dark, into a directory capture.sh passes on. Reads
// RUNNERD_CAPTURE_LOGIN (the demo's one-time login link) and RUNNERD_CAPTURE_IDLE_LOGIN (the same for a second demo
// in its idle scene) from capture.sh. Any page that does not answer 200 or lacks what it should show fails the run,
// so a re-shoot never writes an error page over an asset.
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

// Pages for the docs. `ready` must be visible before the shot. `end` names, by size, the element whose bottom ends the
// shot, or null for the whole page, so a shot ends between blocks, never through a line, whatever the demo's content.
// `press` is a button to press first, `scrollTo` shoots one viewport scrolled to an element, `idle` uses the idle demo.
// Desktop is 1280 wide at 1.5x, phone 390 at 2x.
const pages = [
  { name: 'overview', path: '/', ready: '#budget', end: { desktop: null, phone: '.machine' } },
  { name: 'overview-idle', path: '/', ready: '#budget', end: { desktop: '.machine', phone: '.machine' }, idle: true },
  // One phone screen of the overview scrolled to the budget, tab bar included, for the landing page beside the TUI's
  // frame: its 390 by 731 is the phone box's inside where the TUI's 96 by 26 frame is 526 px tall.
  { name: 'screen', path: '/', ready: '#budget', end: { phone: null }, scrollTo: '#budget', viewport: 731 },
  { name: 'timeline', path: '/history?lens=timeline', ready: '.timeline', end: { desktop: '.timeline', phone: '.timeline' } },
  { name: 'insights', path: '/insights', ready: '.insight-list', end: { desktop: '.insight-list', phone: '.insight-list' } },
  { name: 'job-failed', path: '/jobs/9401', ready: '#failure', end: { desktop: '#log', phone: '#steps' } },
  {
    name: 'cancel',
    path: '/jobs/120055991700',
    ready: '[data-asks="cancel"]',
    end: { desktop: '#job-action', phone: '#job-action' },
    press: '[data-asks="cancel"]',
  },
  { name: 'settings', path: '/config', ready: '#doctor', end: { desktop: null, phone: '#doctor' } },
  // Jobs opens on its 30 days and its search, then the rows; four rows say the rest.
  {
    name: 'jobs',
    path: '/history',
    ready: '.history-table',
    end: { desktop: '.history-table tbody tr:nth-child(4)', phone: '.history-table tbody tr:nth-child(4)' },
  },
  { name: 'sizing', path: '/sizing', ready: '.sizing-lanes', end: { desktop: '.auto-plan > summary', phone: '.auto-plan > summary' } },
  { name: 'caches', path: '/caches', ready: '#volumes', end: { desktop: null, phone: '.volumes > li:nth-child(4)' } },
];
// Parts of pages for the landing page, each cut to the box around its elements: at 1000 wide and 2x, so they stay
// legible at about half the page's width, and at a phone's width for phones. Each must show its first element, or
// its `ready` when that one appears only after `press`.
const parts = [
  { name: 'part-lanes', path: '/', elements: ['#health', '.machine'] },
  { name: 'part-queue', path: '/', elements: ['#queue'] },
  { name: 'part-disk', path: '/insights', elements: ['section.disk'] },
  { name: 'part-failure', path: '/jobs/9401', elements: ['#failure'] },
  { name: 'part-timeline', path: '/history?lens=timeline', elements: ['.timeline'] },
  {
    name: 'part-cancel',
    path: '/jobs/120055991700',
    elements: ['#job-action'],
    press: '[data-asks="cancel"]',
    ready: '[data-asks="cancel"]',
  },
];
const sizes = {
  desktop: { width: 1280, height: 900, scale: 1.5, suffix: '' },
  phone: { width: 390, height: 844, scale: 2, suffix: '-phone' },
  part: { width: 1000, height: 900, scale: 2, suffix: '' },
};
// Light and dark of one shot are taken one after the other, so their figures are seconds apart.
const themes = ['light', 'dark'];
// Space under the element that ends a shot, at most.
const GAP = 24;

// The login link works once: sign in once, and give every context the session cookie it sets.
async function signIn(link) {
  const context = await browser.newContext();
  await (await context.newPage()).goto(link, { waitUntil: 'load' });
  const cookies = await context.cookies();
  await context.close();
  if (cookies.length === 0) throw new Error(`signing in at ${new URL(link).origin} set no cookie`);
  return cookies;
}
const cookies = await signIn(login);
const idleCookies = await signIn(idleLogin);

async function open(size, theme, { path, press, idle, ready }, height = sizes[size].height) {
  const { width, scale } = sizes[size];
  const context = await browser.newContext({
    viewport: { width, height },
    deviceScaleFactor: scale,
    colorScheme: theme,
    reducedMotion: 'reduce',
  });
  await context.addCookies(idle ? idleCookies : cookies);
  const page = await context.newPage();
  const response = await page.goto(new URL(path, idle ? idleLogin : login).href, { waitUntil: 'load' });
  if (!response?.ok()) throw new Error(`${path} answered ${response?.status()}`);
  if (ready) await page.locator(ready).first().waitFor({ state: 'visible', timeout: 10_000 });
  await page.waitForTimeout(2500);
  if (press) {
    await page.click(press);
    await page.waitForTimeout(1200);
  }
  return { context, page };
}

// Where a shot ends, in page pixels: the bottom of `end` plus a gap and whatever the page fixes to the viewport's
// bottom (the phone's tab bar), so that bar never covers the last row; with no `end`, the whole page.
async function bottom(page, end) {
  if (!end) return page.evaluate(() => document.documentElement.scrollHeight);
  const box = await page.locator(end).first().boundingBox();
  if (!box) throw new Error(`no ${end} on ${page.url()}`);
  // The gap stops where the next block starts, so a row that follows closely does not show its top edge.
  const gap = await page.locator(end).first().evaluate((el, most) => {
    const below = el.getBoundingClientRect().bottom;
    for (let at = el; at && at !== document.body; at = at.parentElement) {
      for (let next = at.nextElementSibling; next; next = next.nextElementSibling) {
        const r = next.getBoundingClientRect();
        if (r.height > 0 && r.top >= below - 1) return Math.max(0, Math.min(most, r.top - below));
      }
    }
    return most;
  }, GAP);
  const bar = await page.evaluate(() =>
    Math.max(
      0,
      ...[...document.querySelectorAll('body *')]
        .filter((el) => getComputedStyle(el).position === 'fixed')
        .map((el) => el.getBoundingClientRect())
        .filter((r) => r.height > 0 && Math.abs(r.bottom - innerHeight) < 2)
        .map((r) => r.height),
    ),
  );
  return Math.floor(box.y + box.height + gap + bar);
}

for (const shot of pages) {
  for (const [size, end] of Object.entries(shot.end)) {
    for (const theme of themes) {
      const file = join(out, `${shot.name}${sizes[size].suffix}-${theme}.png`);
      const { context, page } = await open(size, theme, shot, shot.viewport);
      if (shot.scrollTo) {
        // One screen as a reader sees it there: the element 16 px under the viewport's top, or under a header the page
        // keeps at the top.
        await page.locator(shot.scrollTo).first().evaluate((el) => {
          const header = [...document.querySelectorAll('body *')].find((h) => {
            const { position } = getComputedStyle(h);
            return (position === 'sticky' || position === 'fixed') && h.getBoundingClientRect().top <= 0 && h.offsetHeight < 200;
          });
          window.scrollTo(0, el.getBoundingClientRect().top + window.scrollY - 16 - (header?.offsetHeight ?? 0));
        });
        await page.waitForTimeout(300);
        await page.screenshot({ path: file });
        await context.close();
        continue;
      }
      // The viewport is as tall as the shot, so what the page fixes to the viewport's bottom is at the shot's bottom.
      const { width } = sizes[size];
      await page.setViewportSize({ width, height: await bottom(page, end) });
      await page.waitForTimeout(300);
      const height = Math.min(await bottom(page, end), await page.evaluate(() => document.documentElement.scrollHeight));
      await page.screenshot({ path: file, clip: { x: 0, y: 0, width, height }, fullPage: true });
      await context.close();
    }
  }
}
for (const part of parts) {
  for (const size of ['part', 'phone']) {
    for (const theme of themes) {
      const { context, page } = await open(size, theme, { ...part, ready: part.ready ?? part.elements[0] });
      // A cut-out has no viewport, so what the page fixes to one, such as the phone's tab bar, would land mid-shot.
      await page.evaluate(() => {
        for (const el of document.querySelectorAll('body *')) {
          if (getComputedStyle(el).position === 'fixed') el.style.visibility = 'hidden';
        }
      });
      const boxes = [];
      for (const element of part.elements) {
        const box = await page.locator(element).first().boundingBox();
        if (!box) throw new Error(`${part.name}: no ${element} on ${part.path}`);
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
      await page.screenshot({ path: join(out, `${part.name}${sizes[size].suffix}-${theme}.png`), clip, fullPage: true });
      await context.close();
    }
  }
}
await browser.close();
