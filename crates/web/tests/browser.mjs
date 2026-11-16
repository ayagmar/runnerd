// What only a browser sees, against the demo: the attention strip opens and closes by mouse, finger and keyboard
// with its script running; the line typed on in the settings editor stays above the buttons' bar and the tab bar;
// and a first visit to Settings, the doctor's checks included, moves nothing once painted.
//
// node crates/web/tests/browser.mjs target/debug/examples/demo
// Uses the site's playwright-core and the Chromium at $CHROMIUM, else the one playwright-core installed.

import { spawn } from 'node:child_process';
import { mkdtempSync, readFileSync, rmSync, existsSync } from 'node:fs';
import { createRequire } from 'node:module';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

const require = createRequire(new URL('../../../site/package.json', import.meta.url));
const { chromium } = require('playwright-core');

const demo = process.argv[2];
if (!demo) {
  console.error('usage: node browser.mjs DEMO_BINARY');
  process.exit(2);
}
const dir = mkdtempSync(join(tmpdir(), 'runnerd-browser-'));
const address = `127.0.0.1:${7600 + (process.pid % 300)}`;
const server = spawn(demo, [dir, address], { stdio: 'ignore' });
const failures = [];
const fail = (what) => {
  failures.push(what);
  console.log(`FAIL ${what}`);
};
const pass = (what) => console.log(`ok   ${what}`);
const check = (holds, what) => (holds ? pass(what) : fail(what));

const loginFile = join(dir, 'runnerd.web');
for (let tries = 0; !existsSync(loginFile) || readFileSync(loginFile, 'utf8').trim() === ''; tries++) {
  if (tries > 300) throw new Error('the demo wrote no login link');
  await new Promise((done) => setTimeout(done, 100));
}
const login = readFileSync(loginFile, 'utf8').trim().split('\n')[0];
const browser = await chromium.launch({ executablePath: process.env.CHROMIUM || undefined });

// The login link signs one browser in once: its cookies sign in every context after.
const cookies = await (async () => {
  const context = await browser.newContext();
  const page = await context.newPage();
  for (let tries = 0; ; tries++) {
    try {
      await page.goto(login);
      break;
    } catch (error) {
      if (tries > 50) throw error;
      await page.waitForTimeout(100);
    }
  }
  const signed = await context.cookies();
  await context.close();
  return signed;
})();

async function signedIn(size) {
  const context = await browser.newContext({
    viewport: { width: size.width, height: size.height },
    isMobile: !!size.phone,
    hasTouch: !!size.phone,
  });
  await context.addCookies(cookies);
  return { context, page: await context.newPage() };
}

const sizes = [
  { name: '1440x900', width: 1440, height: 900 },
  { name: '390x844', width: 390, height: 844, phone: true },
  { name: '390x400', width: 390, height: 400, phone: true },
];

try {
  // The attention strip: open stays open, a second press closes it, whatever presses it.
  for (const size of [sizes[0], sizes[1]]) {
    const { context, page } = await signedIn(size);
    await page.goto(new URL('/', login).href);
    await page.waitForSelector('#attention summary');
    const open = () => page.evaluate(() => document.getElementById('attention').open);
    const summary = page.locator('#attention summary');
    const presses = size.phone
      ? [['finger', () => summary.tap()]]
      : [
          ['mouse', () => summary.click()],
          ['keyboard', () => summary.focus().then(() => page.keyboard.press('Enter'))],
        ];
    for (const [how, press] of presses) {
      await press();
      await page.waitForTimeout(400);
      const opened = await open();
      const listed = await page.locator('#attention .attention-list').isVisible();
      check(opened && listed, `${size.name}: the attention strip opens by ${how}`);
      await press();
      await page.waitForTimeout(400);
      check(!(await open()), `${size.name}: the attention strip closes by ${how}`);
    }
    await context.close();
  }

  // The editor: typing at the end of the file and arrowing back up, the caret's line stays clear of the bars.
  for (const size of sizes) {
    const { context, page } = await signedIn(size);
    await page.goto(new URL('/config', login).href);
    await page.waitForSelector('#toml');
    const covered = () =>
      page.evaluate(() => {
        const editor = document.getElementById('toml');
        const style = getComputedStyle(editor);
        const line = editor.value.slice(0, editor.selectionStart).split('\n').length - 1;
        const lineHeight = parseFloat(style.lineHeight);
        const bottom = editor.getBoundingClientRect().top + parseFloat(style.paddingTop) + (line + 1) * lineHeight;
        const bars = [document.querySelector('.editor > .editor-actions'), document.querySelector('.top nav')]
          .map((bar) => bar.getBoundingClientRect())
          .filter((box) => box.height > 0 && box.top < innerHeight && box.bottom > innerHeight - 200)
          .map((box) => box.top);
        const top = Math.min(innerHeight, ...bars);
        return bottom > top + 1 ? `line ${line + 1} ends at ${Math.round(bottom)}, a bar starts at ${Math.round(top)}` : '';
      });
    await page.locator('#toml').focus();
    await page.keyboard.press('Control+End');
    const seen = [];
    for (let step = 0; step < 8; step++) {
      await page.keyboard.press('Enter');
      await page.keyboard.type(`# line ${step}`);
      await page.waitForTimeout(60);
      const found = await covered();
      if (found) seen.push(found);
    }
    for (let step = 0; step < 25; step++) {
      await page.keyboard.press('ArrowUp');
      await page.waitForTimeout(20);
    }
    for (let step = 0; step < 25; step++) {
      await page.keyboard.press('ArrowDown');
      await page.waitForTimeout(30);
      const found = await covered();
      if (found) seen.push(found);
    }
    check(seen.length === 0, `${size.name}: the caret's line is never under a bar${seen.length ? ` (${seen[0]})` : ''}`);
    await context.close();
  }

  // A first visit to Settings: the doctor's checks are in the first paint, so nothing moves.
  for (const size of [sizes[0], sizes[1]]) {
    const { context, page } = await signedIn(size);
    await page.addInitScript(() => {
      window.shifted = 0;
      new PerformanceObserver((list) => {
        for (const entry of list.getEntries()) if (!entry.hadRecentInput) window.shifted += entry.value;
      }).observe({ type: 'layout-shift', buffered: true });
    });
    await page.goto(new URL('/config', login).href);
    await page.waitForTimeout(2500);
    const shifted = await page.evaluate(() => window.shifted);
    check(shifted < 0.01, `${size.name}: a first visit to Settings shifts ${shifted.toFixed(4)}`);
    await context.close();
  }
} finally {
  await browser.close();
  server.kill();
  rmSync(dir, { recursive: true, force: true });
}

if (failures.length) {
  console.log(`${failures.length} failed`);
  process.exit(1);
}
