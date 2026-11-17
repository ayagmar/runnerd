// What only a browser sees, against the demo: the attention strip opens and closes by mouse, finger and keyboard
// with its script running; the line typed on in the settings editor stays above the buttons' bar and the tab bar;
// a first visit to Settings, the doctor's checks included, moves nothing once painted; the jobs list's chips sit in
// the Filters fold on a phone and beside the search on a wide screen; on a phone a running job's log is part of the
// page, last, followed only while the reader is at its newest line, and holds the reader's line as lines above go;
// "Jump to newest" moves nothing; reseeding a snapshot asks first; and a chart card holds its labels at a laptop's
// widths.
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
const failures = [];
const fail = (what) => {
  failures.push(what);
  console.log(`FAIL ${what}`);
};
const pass = (what) => console.log(`ok   ${what}`);
const check = (holds, what) => (holds ? pass(what) : fail(what));

const server = spawn(demo, [dir, address], { stdio: 'ignore' });
let browser;
try {
  const loginFile = join(dir, 'runnerd.web');
  for (let tries = 0; !existsSync(loginFile) || readFileSync(loginFile, 'utf8').trim() === ''; tries++) {
    if (tries > 300) throw new Error('the demo wrote no login link');
    await new Promise((done) => setTimeout(done, 100));
  }
  const login = readFileSync(loginFile, 'utf8').trim().split('\n')[0];
  browser = await chromium.launch({ executablePath: process.env.CHROMIUM || undefined });

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
  // The jobs list: on a phone the chips are in the fold, which opens to them; on a wide screen they stand beside it.
  for (const size of [sizes[0], sizes[1]]) {
    const { context, page } = await signedIn(size);
    await page.goto(new URL('/history', login).href);
    await page.waitForSelector('.more-filters summary');
    const failed = page.locator('.filters a', { hasText: /^Failed$/ }).filter({ visible: true });
    if (size.phone) {
      check((await failed.count()) === 0, `${size.name}: the outcome chips wait in the Filters fold`);
      await page.locator('.more-filters summary').tap();
      await page.waitForTimeout(200);
      check((await failed.count()) === 1, `${size.name}: the Filters fold opens on the outcome chips`);
    } else {
      check((await failed.count()) === 1, `${size.name}: the outcome chips stand beside the search`);
    }
    await context.close();
  }

  // Beside the sidebar, the jobs list fits its page: no sideways scroll inside it at a laptop's widths.
  for (const width of [1024, 1200]) {
    const { context, page } = await signedIn({ width, height: 800 });
    await page.goto(new URL('/history', login).href);
    const fit = await page.evaluate(() => {
      const wrap = document.querySelector('.history-table').closest('.table-wrap');
      return { scroll: wrap.scrollWidth, client: wrap.clientWidth };
    });
    check(fit.scroll <= fit.client, `${width}px: the jobs list is ${fit.scroll}px in its ${fit.client}px page`);
    await context.close();
  }

  // A chart card holds its heading, chart and labels at a laptop's widths, where the overview's two host charts share
  // half the page: a heading wider than its card once pushed the reading and the time axis out of it.
  for (const width of [1024, 1280, 1440]) {
    const { context, page } = await signedIn({ width, height: 900 });
    for (const path of ['/', '/history?lens=trends']) {
      await page.goto(new URL(path, login).href);
      await page.locator('.chart-card .chart-x').first().waitFor();
      const out = await page.evaluate(() =>
        [...document.querySelectorAll('.chart-card')].flatMap((card) => {
          const edge = card.getBoundingClientRect();
          return [...card.querySelectorAll('h3, h3 *, .chart-x, .chart-x *')]
            .filter((el) => el.getBoundingClientRect().right > edge.right + 0.5)
            .map((el) => `${el.textContent.trim().slice(0, 24)} by ${Math.round(el.getBoundingClientRect().right - edge.right)}px`);
        }),
      );
      check(out.length === 0, `${width}px: ${path}'s chart cards hold their labels${out.length ? ` (${out[0]})` : ''}`);
    }
    await context.close();
  }

  // A running job's log on a phone: no box scrolling inside the page; the page keeps the reader's place while they
  // read above the newest line, which "Jump to newest" brings back, and then follows the lines as they come.
  {
    const { context, page } = await signedIn(sizes[1]);
    await page.goto(new URL('/history?outcome=running', login).href);
    const href = await page.locator('.history-table tbody th a').first().getAttribute('href');
    await page.goto(new URL(href, login).href);
    await page.waitForSelector('#log-lines > li');
    const state = () =>
      page.evaluate(() => {
        const tail = document.getElementById('log-lines');
        const bar = document.querySelector('.top nav').getBoundingClientRect().top;
        return {
          own: getComputedStyle(tail).overflowY !== 'visible',
          y: scrollY,
          past: Math.round(tail.getBoundingClientRect().bottom - Math.min(innerHeight, bar)),
          jump: !document.querySelector('[data-jump]').hidden,
        };
      });
    check(!(await state()).own, `${sizes[1].name}: a running job's log scrolls with the page`);
    await page.waitForTimeout(1500);
    check((await state()).y === 0, `${sizes[1].name}: new lines leave a reader at the top where they are`);
    // A reader at the top is not in the log: a step growing above it, and the tail crossing 200 lines (its note
    // appears), move nothing on their screen when the next line comes.
    await page.evaluate(() => {
      const grown = document.createElement('div');
      grown.style.height = '80px';
      document.getElementById('steps').append(grown);
      const tail = document.getElementById('log-lines');
      let value = tail.lastElementChild.value;
      while (tail.children.length <= 200) {
        const li = document.createElement('li');
        li.value = ++value;
        li.innerHTML = `<span class="ln">${value}</span><span class="text">appended</span>`;
        tail.append(li);
      }
    });
    await page.waitForTimeout(2500);
    const topped = await state();
    const noted = await page.evaluate(() => getComputedStyle(document.querySelector('.older-lines')).display !== 'none');
    check(noted && topped.y === 0, `${sizes[1].name}: growth above the log leaves a reader at the top at ${topped.y}`);
    await page.evaluate(() => document.getElementById('log-lines').scrollIntoView({ block: 'start' }));
    await page.waitForTimeout(300);
    const above = await state();
    await page.waitForTimeout(1500);
    const later = await state();
    check(above.jump && later.y === above.y, `${sizes[1].name}: above the newest line, the page holds and offers a jump`);
    await page.locator('[data-jump]').tap();
    await page.waitForTimeout(300);
    await page.waitForTimeout(1500);
    const followed = await state();
    check(
      !followed.jump && followed.past < 48 && followed.past > -160,
      `${sizes[1].name}: after the jump, the newest line stays at the foot (${followed.past}px)`,
    );
    // A small scroll up is the reader's: the page stops following, however fast the lines come.
    await page.evaluate(() => scrollBy({ top: -40, behavior: 'instant' }));
    await page.waitForTimeout(300);
    // The reader's line, the first on screen: lines hidden above it as the tail passes 200 move the page, not it.
    const reading = () =>
      page.evaluate(() => {
        const line = [...document.getElementById('log-lines').children].find(
          (li) => li.getClientRects().length > 0 && li.getBoundingClientRect().top >= 0,
        );
        return { value: line.value, top: Math.round(line.getBoundingClientRect().top) };
      });
    const nudged = { ...(await state()), line: await reading() };
    await page.waitForTimeout(2500);
    const held = await page.evaluate(
      (value) => Math.round([...document.getElementById('log-lines').children].find((li) => li.value === value).getBoundingClientRect().top),
      nudged.line.value,
    );
    check(
      nudged.jump && Math.abs(held - nudged.line.top) <= 1,
      `${sizes[1].name}: 40px up from the newest line stops following (line ${nudged.line.value} at ${nudged.line.top} then ${held})`,
    );
    // Lines hidden or dropped above the reader leave their line where it is. Chromium's own anchoring is off for the
    // tail, as Safari has none, so this is the script's doing; Safari itself is not run.
    const kept = await page.evaluate(async () => {
      const tail = document.getElementById('log-lines');
      const foot = document.querySelector('.top nav').getBoundingClientRect().top;
      const line = [...tail.children].findLast((li) => li.getBoundingClientRect().top < foot - 60);
      const before = line.getBoundingClientRect().top;
      let value = tail.lastElementChild.value;
      for (let n = 0; n < 150; n++) {
        const li = document.createElement('li');
        li.value = ++value;
        li.innerHTML = `<span class="ln">${value}</span><span class="text">appended</span>`;
        tail.append(li);
      }
      await new Promise((done) => setTimeout(done, 300));
      return { hidden: [...tail.children].filter((li) => li.getClientRects().length === 0).length, moved: Math.round(line.getBoundingClientRect().top - before) };
    });
    check(
      kept.hidden > 0 && Math.abs(kept.moved) <= 1,
      `${sizes[1].name}: ${kept.hidden} lines hidden above the reader move their line ${kept.moved}px`,
    );
    // The log comes last on a phone, after the charts and the latest runs, so they stay near the top.
    const order = await page.evaluate(() =>
      ['metrics', 'runs', 'log'].map((id) => document.getElementById(id)?.getBoundingClientRect().top ?? -1),
    );
    check(order[0] < order[2] && order[1] < order[2], `${sizes[1].name}: a running job's log comes after its charts and runs`);
    await context.close();
  }

  // On a wide screen "Jump to newest" shows over the tail and moves nothing under the log.
  {
    const { context, page } = await signedIn(sizes[0]);
    await page.goto(new URL('/history?outcome=running', login).href);
    const href = await page.locator('.history-table tbody th a').first().getAttribute('href');
    await page.goto(new URL(href, login).href);
    await page.waitForSelector('#log-lines > li');
    await page.waitForTimeout(500);
    const below = () => page.evaluate(() => Math.round(document.getElementById('metrics').getBoundingClientRect().top + scrollY));
    await page.evaluate(() => document.getElementById('log-lines').scrollIntoView({ block: 'center' }));
    await page.waitForTimeout(300);
    const before = await below();
    await page.evaluate(() => {
      document.getElementById('log-lines').scrollTop -= 200;
    });
    await page.waitForTimeout(400);
    const shown = await page.evaluate(() => !document.querySelector('[data-jump]').hidden);
    const after = await below();
    check(shown && before === after, `${sizes[0].name}: Jump to newest shows and the charts stay at ${before} (${after})`);
    // Scrolling the page while reading up the log's box leaves the box where the reader left it as lines come.
    const read = await page.evaluate(() => document.getElementById('log-lines').scrollTop);
    await page.mouse.move(20, 450);
    await page.mouse.wheel(0, 300);
    await page.waitForTimeout(2500);
    const kept = await page.evaluate(() => ({ top: document.getElementById('log-lines').scrollTop, y: scrollY }));
    check(
      kept.y > 0 && kept.top === read,
      `${sizes[0].name}: a page scroll leaves the log's box at ${read} as lines come (${kept.top})`,
    );
    await context.close();
  }

  // Reseeding a snapshot asks first, as emptying a volume does; dismissed, nothing is sent.
  for (const size of [sizes[0], sizes[1]]) {
    const { context, page } = await signedIn(size);
    const asked = [];
    const sent = [];
    page.on('dialog', (dialog) => {
      asked.push(dialog.message());
      dialog.dismiss();
    });
    page.on('request', (request) => {
      if (request.method() === 'POST' && request.url().includes('/reseed')) sent.push(request.url());
    });
    await page.goto(new URL('/caches', login).href);
    const button = page.locator('.volume-action button', { hasText: 'Reseed from slot 1' }).first();
    await (size.phone ? button.tap() : button.click());
    await page.waitForTimeout(400);
    check(
      asked.length === 1 && asked[0].startsWith('Reseed') && sent.length === 0,
      `${size.name}: Reseed asks "${asked[0]?.slice(0, 40)}…" and sends nothing when dismissed`,
    );
    await context.close();
  }
} finally {
  // From the spawn on, so a browser that fails to start, or a demo that never signs in, leaves no demo running.
  await browser?.close();
  server.kill();
  rmSync(dir, { recursive: true, force: true });
}

if (failures.length) {
  console.log(`${failures.length} failed`);
  process.exit(1);
}
