// What only a browser sees, against the demo: the attention strip opens and closes by mouse, finger and keyboard
// with its script running; the line typed on in the settings editor stays above the buttons' bar and the tab bar;
// a first visit to Settings, the doctor's checks included, moves nothing once painted; the jobs list's chips sit in
// the Filters fold on a phone and beside the search on a wide screen; on a phone a running job's log is part of the
// page, last, followed only while the reader is at its newest line, and holds the reader's line as lines above go;
// "Jump to newest" moves nothing; reseeding a snapshot asks first; a profile's link on Insights opens its section on
// Capacity and its link back opens its queue on Insights; a chart card holds its labels at a laptop's
// widths; and every queued job is a row of the queue's page, reached from the overview whenever it shows fewer, by
// keyboard and finger, as jobs leave it; and the live badge says "Retrying" while a dropped read is tried again,
// keeping the page and its focus, "Offline" then "Live" when the stream's body breaks and the next request answers,
// "Signed out" with Reload and no further request when it answers 401, and "Finished" on a finished job's page; on a
// phone, each fits the screen.
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
const failures = [];
const fail = (what) => {
  failures.push(what);
  console.log(`FAIL ${what}`);
};
const pass = (what) => console.log(`ok   ${what}`);
const check = (holds, what) => (holds ? pass(what) : fail(what));

// Whether the page is no wider than `width`, the device's, with the live badge and its Reload link inside it. A phone's
// layout viewport grows to what overflows, so the window's own width would not show it.
const fits = (page, width) =>
  page.evaluate((width) => {
    const badge = document.querySelector('.connection')?.getBoundingClientRect();
    const reload = document.querySelector('.connection .feed-reload');
    const shown = reload?.checkVisibility() ? reload.getBoundingClientRect() : null;
    return {
      width,
      scroll: document.documentElement.scrollWidth,
      badge: Math.round(badge?.right ?? 0),
      reload: !!shown && shown.left >= 0 && shown.right <= width,
      fits: document.documentElement.scrollWidth <= width && (badge?.right ?? 0) <= width,
    };
  }, width);

// The demos started, so each is stopped however the run ends.
const demos = [];
let browser;

// A demo on `address` with `env` set, signed in once: its login link, and the cookies that sign in every context after.
async function startDemo(address, env = {}) {
  const dir = mkdtempSync(join(tmpdir(), 'runnerd-browser-'));
  const server = spawn(demo, [dir, address], { stdio: 'ignore', env: { ...process.env, ...env } });
  const started = { dir, server };
  demos.push(started);
  const loginFile = join(dir, 'runnerd.web');
  for (let tries = 0; !existsSync(loginFile) || readFileSync(loginFile, 'utf8').trim() === ''; tries++) {
    if (tries > 300) throw new Error('the demo wrote no login link');
    await new Promise((done) => setTimeout(done, 100));
  }
  const login = readFileSync(loginFile, 'utf8').trim().split('\n')[0];
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
  const cookies = await context.cookies();
  await context.close();
  return { ...started, login, cookies };
}

function stopDemo(started) {
  started.server.kill();
  rmSync(started.dir, { recursive: true, force: true });
}

try {
  browser = await chromium.launch({ executablePath: process.env.CHROMIUM || undefined });
  const main = await startDemo(`127.0.0.1:${7600 + (process.pid % 300)}`);
  const { login, cookies } = main;

  async function signedIn(size, signedCookies = cookies) {
    const context = await browser.newContext({
      viewport: { width: size.width, height: size.height },
      isMobile: !!size.phone,
      hasTouch: !!size.phone,
    });
    await context.addCookies(signedCookies);
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
    // A scroll up in the same moment a line comes: the line arrives before the browser reports the scroll, and must
    // not take the reader back down. Once a CI flake when the demo's next line landed on the 40px nudge above.
    await page.locator('[data-jump]').tap();
    await page.waitForTimeout(1500);
    const raced = await page.evaluate(async () => {
      const tail = document.getElementById('log-lines');
      scrollBy({ top: -40, behavior: 'instant' });
      const line = [...tail.children].find((li) => li.getClientRects().length > 0 && li.getBoundingClientRect().top >= 0);
      const before = line.getBoundingClientRect().top;
      const li = document.createElement('li');
      li.value = tail.lastElementChild.value + 1;
      li.innerHTML = `<span class="ln">${li.value}</span><span class="text">appended</span>`;
      tail.append(li);
      await new Promise((done) => setTimeout(done, 300));
      return {
        jump: !document.querySelector('[data-jump]').hidden,
        moved: Math.round(line.getBoundingClientRect().top - before),
      };
    });
    check(
      raced.jump && Math.abs(raced.moved) <= 1,
      `${sizes[1].name}: a line that comes with a scroll up leaves the reader's line where it is (${raced.moved}px)`,
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

  // A job another runner ran says where in its badge, cut to its column: the overview's latest jobs keep the badge
  // clear of the job's name, the history and a job's page scroll nothing sideways, and the badge says it whole on
  // hover.
  for (const size of [sizes[0], sizes[1]]) {
    const { context, page } = await signedIn(size);
    const clear = () =>
      page.evaluate(() => {
        const found = [];
        const box = (element) => element.getBoundingClientRect();
        for (const badge of document.querySelectorAll('.badge')) {
          if (!/^ran on /.test(badge.textContent.trim())) continue;
          const row = badge.closest('a, tr, h1');
          const name = row?.querySelector('.job-name');
          if (name && box(name).width > 0 && box(badge).right > box(name).left + 1
              && box(badge).left < box(name).right && box(badge).bottom > box(name).top
              && box(badge).top < box(name).bottom) {
            found.push(`"${badge.textContent.trim()}" covers its job's name`);
          }
          const label = badge.firstElementChild ?? badge;
          if (label.scrollWidth > label.clientWidth + 1 && getComputedStyle(label).textOverflow !== 'ellipsis') {
            found.push(`"${badge.textContent.trim()}" spills out of its badge`);
          }
          if (box(badge).right > innerWidth + 1) found.push(`"${badge.textContent.trim()}" ends past the page`);
          if (badge.title !== badge.textContent.trim()) found.push(`"${badge.textContent.trim()}" has no whole title`);
        }
        if (document.documentElement.scrollWidth > innerWidth + 1) {
          found.push(`the page scrolls sideways: ${document.documentElement.scrollWidth} > ${innerWidth}`);
        }
        return { found, seen: [...document.querySelectorAll('.badge')].filter((badge) => /^ran on /.test(badge.textContent.trim())).length };
      });
    await page.goto(new URL('/', login).href);
    await page.waitForSelector('#recent .job-list');
    const overview = await clear();
    check(overview.seen >= 2 && overview.found.length === 0, `${size.name}: the latest jobs fit "ran on" badges (${overview.seen}) ${overview.found.join('; ')}`);
    const link = await page.evaluate(() =>
      [...document.querySelectorAll('#recent .job-list a')].find((a) => /arc-prod/i.test(a.textContent))?.getAttribute('href'));
    await page.goto(new URL('/history', login).href);
    await page.waitForSelector('.history-table tbody tr');
    const history = await clear();
    check(history.seen >= 2 && history.found.length === 0, `${size.name}: the history fits "ran on" badges (${history.seen}) ${history.found.join('; ')}`);
    await page.goto(new URL(link ?? '/jobs/0', login).href);
    await page.waitForSelector('h1 .badge');
    const job = await clear();
    check(job.seen >= 1 && job.found.length === 0, `${size.name}: a job's page fits its "ran on" badge ${job.found.join('; ')}`);
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

  // Insights and Capacity link one profile's sections to each other by its name: a click from why jvm's jobs queued
  // opens jvm's section on Capacity, in view, and its link back opens jvm's queue on Insights.
  for (const size of [sizes[0], sizes[1]]) {
    const { context, page } = await signedIn(size);
    await page.goto(new URL('/insights', login).href);
    const target = () =>
      page.evaluate(() => {
        const shown = document.querySelector(':target');
        const box = shown?.getBoundingClientRect();
        return {
          path: location.pathname,
          text: shown?.textContent ?? '',
          inView: !!box && box.top >= 0 && box.top < innerHeight,
        };
      });
    await page.locator('.queue-profile a[href^="/sizing#"]', { hasText: /^jvm / }).first().click();
    await page.waitForURL(/\/sizing#profile-/);
    const capacity = await target();
    check(
      capacity.path === '/sizing' && /\bjvm\b/.test(capacity.text) && capacity.inView,
      `${size.name}: jvm's link on Insights opens jvm's section on Capacity, in view ("${capacity.text.trim().slice(0, 30)}")`,
    );
    await page.locator(':target').locator('xpath=ancestor::section[1]').locator('a[href^="/insights#"]').click();
    await page.waitForURL(/\/insights#profile-/);
    const insights = await target();
    check(
      insights.path === '/insights' && /\bjvm\b/.test(insights.text) && insights.inView,
      `${size.name}: jvm's link back opens jvm's queue on Insights, in view ("${insights.text.trim().slice(0, 30)}")`,
    );
    await context.close();
  }

  // The queue: the overview shows the oldest six, three on a phone, and links to every one whenever it shows fewer
  // than wait; the queue's page lists every one with why it waits, fits the screen and moves nothing once painted.
  for (const count of [0, 3, 4, 6, 7, 60]) {
    const queued = await startDemo('127.0.0.1:0', { RUNNERD_DEMO_QUEUED: String(count) });
    for (const size of [sizes[0], sizes[1]]) {
      const { context, page } = await signedIn(size, queued.cookies);
      await page.addInitScript(() => {
        window.shifted = 0;
        new PerformanceObserver((list) => {
          for (const entry of list.getEntries()) if (!entry.hadRecentInput) window.shifted += entry.value;
        }).observe({ type: 'layout-shift', buffered: true });
      });
      await page.goto(new URL('/', queued.login).href);
      await page.waitForSelector('#queue');
      const shown = Math.min(count, size.phone ? 3 : 6);
      const rows = await page.locator('#queue .queue-list > li').filter({ visible: true }).count();
      check(rows === shown, `${size.name}, ${count} queued: the overview shows ${rows} of ${shown}`);
      const link = page.locator('#queue a[href="/queue"]').filter({ visible: true });
      const linked = (await link.count()) === 1;
      check(linked === count > shown, `${size.name}, ${count} queued: the link to every one ${linked ? 'shows' : 'is absent'}`);
      if (linked) {
        await (size.phone ? link.tap() : link.click());
        await page.waitForURL('**/queue');
      } else {
        await page.goto(new URL('/queue', queued.login).href);
      }
      await page.waitForSelector('#waiting');
      await page.waitForTimeout(1500);
      const listed = await page.evaluate(() => {
        const rows = [...document.querySelectorAll('#waiting .queue-list > li')];
        return {
          rows: rows.filter((row) => row.getClientRects().length > 0).length,
          reasons: rows.filter((row) => row.querySelector('.reason')?.textContent.trim()).length,
          out: rows.filter((row) => row.getBoundingClientRect().right > innerWidth + 0.5).length,
          sideways: document.documentElement.scrollWidth > innerWidth + 1,
          empty: document.querySelector('#waiting > p.muted')?.textContent ?? '',
          shifted: window.shifted,
        };
      });
      check(
        listed.rows === count && listed.reasons === count,
        `${size.name}, ${count} queued: the queue's page shows ${listed.rows} rows, ${listed.reasons} with why`,
      );
      check(!listed.sideways && listed.out === 0, `${size.name}, ${count} queued: the queue's page fits the screen`);
      check(listed.shifted < 0.01, `${size.name}, ${count} queued: the queue's page shifts ${listed.shifted.toFixed(4)}`);
      if (count === 0) check(/^Nothing waiting/.test(listed.empty), `${size.name}: an empty queue says nothing waits`);
      if (size.phone && count >= 7) {
        // Every name and branch is read whole on a phone: jobs of one name differ only at their branch's end.
        const cut = await page.evaluate(() =>
          [...document.querySelectorAll('#waiting .queue-main :is(.job-name, .repo)')]
            .filter((el) => el.scrollWidth > el.clientWidth + 1)
            .map((el) => el.textContent.trim()),
        );
        const places = await page.evaluate(() => {
          const named = [...document.querySelectorAll('#waiting .queue-list > li')].filter(
            (row) => row.querySelector('.job-name')?.textContent.trim() === 'CI / test (1)',
          );
          return named.map((row) => row.querySelector('.repo').innerText.trim());
        });
        check(
          cut.length === 0 && places.length >= 2 && new Set(places).size === places.length,
          `${size.name}, ${count} queued: names and branches wrap whole, ${places.length} jobs of one name tell apart${cut.length ? ` (cut: ${cut[0]})` : ''}`,
        );
      }
      await context.close();
    }
    stopDemo(queued);
  }

  // The queue's page by keyboard and by finger, while jobs leave it: j and k move between rows, Enter opens a row's
  // page and Tab reaches its GitHub link; a focused row stays focused as an older one leaves, and once its own job
  // leaves, j picks up the rows still there.
  {
    const leaving = await startDemo('127.0.0.1:0', { RUNNERD_DEMO_QUEUED: '7', RUNNERD_DEMO_START_EVERY: '3' });
    const { context, page } = await signedIn(sizes[0], leaving.cookies);
    await page.goto(new URL('/queue', leaving.login).href);
    await page.waitForSelector('#waiting .queue-list > li');
    const focused = () => page.evaluate(() => document.activeElement?.closest('[data-row]')?.id ?? '');
    await page.keyboard.press('j');
    await page.keyboard.press('j');
    const second = await focused();
    const firstId = await page.evaluate(() => document.querySelector('#waiting .queue-list > li').id);
    check(second !== '' && second !== firstId, `${sizes[0].name}: j moves to the second row (${second})`);
    await page.keyboard.press('Tab');
    const tabbed = await page.evaluate(() => ({
      row: document.activeElement?.closest('[data-row]')?.id ?? '',
      github: document.activeElement?.classList.contains('queue-github') ?? false,
    }));
    check(tabbed.row === second && tabbed.github, `${sizes[0].name}: Tab reaches the row's GitHub link`);
    await page.keyboard.press('Shift+Tab');
    const left = (id) =>
      page
        .waitForFunction((gone) => !document.getElementById(gone), id, { timeout: 8000 })
        .then(() => true, () => false);
    const gone = await left(firstId);
    const kept = await focused();
    check(gone && kept === second, `${sizes[0].name}: an older job leaves (${gone}) and the focused row stays focused (${kept})`);
    const ownGone = await left(second);
    check(ownGone, `${sizes[0].name}: the focused row's own job leaves the page`);
    await page.keyboard.press('j');
    await page.keyboard.press('j');
    const down = await focused();
    await page.keyboard.press('k');
    const up = await focused();
    const order = await page.evaluate(() => [...document.querySelectorAll('#waiting .queue-list > li')].map((row) => row.id));
    check(
      ownGone && down !== '' && down !== second && order.indexOf(up) === order.indexOf(down) - 1,
      `${sizes[0].name}: once the focused row's job leaves, j picks up the rows still there (${down}) and k goes back one (${up})`,
    );
    const href = await page.evaluate(() => document.activeElement.getAttribute('href'));
    await page.keyboard.press('Enter');
    await page.waitForURL((url) => url.pathname === href, { timeout: 5000 }).catch(() => {});
    check(new URL(page.url()).pathname === href && href.startsWith('/jobs/'), `${sizes[0].name}: Enter opens the row's job (${href})`);
    await context.close();

    const phone = await signedIn(sizes[1], leaving.cookies);
    await phone.page.goto(new URL('/', leaving.login).href);
    const all = phone.page.locator('#queue a[href="/queue"]').filter({ visible: true });
    await all.tap();
    await phone.page.waitForURL('**/queue');
    const row = phone.page.locator('#waiting a.queue-main[href^="/jobs/"]').first();
    const target = await row.getAttribute('href');
    const box = await row.boundingBox();
    await row.tap();
    await phone.page.waitForURL((url) => url.pathname === target, { timeout: 5000 }).catch(() => {});
    check(
      new URL(phone.page.url()).pathname === target && box.height >= 44,
      `${sizes[1].name}: a tap on a ${Math.round(box.height)}px row opens its job (${target})`,
    );
    await phone.context.close();
    stopDemo(leaving);
  }
  // The live badge says what the stream says of the page's data: "Retrying" while a read is tried again, the page
  // and its focus kept, and "Live" once it answers; a finished job's page says so, with no claim to connect again.
  {
    const dropping = await startDemo('127.0.0.1:0', { RUNNERD_DEMO_DROP_EVERY: '5' });
    const { context, page } = await signedIn(sizes[0], dropping.cookies);
    await page.goto(new URL('/', dropping.login).href);
    const feed = (state, timeout = 9000) =>
      page
        .waitForSelector(`.connection[data-feed="${state}"]`, { timeout })
        .then(() => true, () => false);
    check(await feed('current'), `${sizes[0].name}: the overview's badge is current once the stream delivers`);
    // Outside the regions, which re-render with the data anyway: a recovery that reloaded the page would lose both.
    const focused = page.locator('header nav a[href]').first();
    await focused.focus();
    await page.evaluate(() => {
      window.notReloaded = true;
    });
    const retrying = await feed('retrying');
    const during = await page.evaluate(() => ({
      label: document.querySelector('.connection .feed-label').textContent,
      since: document.querySelector('.connection .feed-since').checkVisibility()
        ? document.querySelector('.connection .feed-since').textContent
        : '',
      regions: document.querySelectorAll('#main section[id]').length,
    }));
    check(
      retrying && during.label === 'Retrying' && during.since.startsWith('as of ') && during.regions > 0,
      `${sizes[0].name}: a dropped read shows "${during.label}" with the data's time (${during.since}), the page kept`,
    );
    const back = await feed('current');
    const kept = await focused.evaluate((element) => element === document.activeElement && window.notReloaded === true);
    const label = await page.locator('.connection .feed-label').textContent();
    check(back && label === 'Live' && kept, `${sizes[0].name}: the read answers, the badge is ${label}, the page and its focus kept`);
    await context.close();
    // On a phone the retrying badge, its time and its reason fit the screen.
    const narrow = await signedIn(sizes[1], dropping.cookies);
    await narrow.page.goto(new URL('/', dropping.login).href);
    const narrowRetrying = await narrow.page
      .waitForSelector('.connection[data-feed="retrying"]', { timeout: 9000 })
      .then(() => true, () => false);
    const narrowFit = await fits(narrow.page, sizes[1].width);
    check(
      narrowRetrying && narrowFit.fits,
      `${sizes[1].name}: the retrying badge fits the screen (${narrowFit.scroll}px of ${narrowFit.width}px, badge to ${narrowFit.badge}px)`,
    );
    await narrow.context.close();
    stopDemo(dropping);

    const finishing = await startDemo('127.0.0.1:0', { RUNNERD_DEMO_FINISH_AFTER: '8' });
    const job = await signedIn(sizes[0], finishing.cookies);
    await job.page.goto(new URL('/', finishing.login).href);
    const running = await job.page.locator('#slots a[href^="/jobs/"]').first().getAttribute('href');
    await job.page.goto(new URL(running, finishing.login).href);
    const live = await job.page
      .waitForSelector('.connection[data-feed="current"]', { timeout: 9000 })
      .then(() => true, () => false);
    const done = await job.page
      .waitForSelector('.connection[data-feed="finished"]', { timeout: 15000 })
      .then(() => true, () => false);
    const badge = await job.page.evaluate(() => ({
      label: document.querySelector('.connection .feed-label').textContent,
      title: document.querySelector('.connection').title,
      reload: document.querySelector('.connection .feed-reload').checkVisibility(),
    }));
    check(
      live && done && badge.label === 'Finished' && !/again/.test(badge.title) && !badge.reload,
      `${sizes[0].name}: a job that finishes while its page is open says ${badge.label}, not that it retries ("${badge.title}")`,
    );
    await job.context.close();
    stopDemo(finishing);

    // A stream whose body breaks: the badge says Offline while Datastar waits, then Live on the next request. A 401 to
    // that request, as after a sign-out between connections, is the end: Signed out with Reload, no further request.
    const cutting = await startDemo('127.0.0.1:0', { RUNNERD_DEMO_CUT_EVERY: '6' });
    for (const size of [sizes[0], sizes[1]]) {
      const live = await signedIn(size, cutting.cookies);
      let requests = 0;
      let refuse = false;
      await live.page.route('**/live/overview**', (route) => {
        requests += 1;
        return refuse ? route.fulfill({ status: 401, body: 'sign in' }) : route.continue();
      });
      await live.page.goto(new URL('/', cutting.login).href);
      const state = (feed, timeout = 9000) =>
        live.page
          .waitForSelector(`.connection[data-feed="${feed}"]`, { timeout })
          .then(() => true, () => false);
      const first = await state('current');
      const offline = await state('offline');
      const offlineFit = await fits(live.page, size.width);
      const offlineLabel = await live.page.locator('.connection .feed-label').textContent();
      const back = await state('current');
      check(
        first && offline && offlineLabel === 'Offline' && back && requests === 2,
        `${size.name}: a broken stream shows ${offlineLabel} while it waits and Live on the next request (${requests} requests)`,
      );
      check(offlineFit.fits, `${size.name}: the offline badge fits the screen (${offlineFit.scroll}px of ${offlineFit.width}px)`);
      refuse = true;
      const signedOut = await state('signed-out', 12000);
      const badge = await live.page.evaluate(() => ({
        label: document.querySelector('.connection .feed-label').textContent,
        title: document.querySelector('.connection').title,
        reload: document.querySelector('.connection .feed-reload').checkVisibility(),
      }));
      const refusedFit = await fits(live.page, size.width);
      const asked = requests;
      await live.page.waitForTimeout(3000);
      check(
        signedOut && badge.label === 'Signed out' && badge.reload && !/connecting again/.test(badge.title) && requests === asked,
        `${size.name}: a 401 shows ${badge.label} with Reload and no further request (${asked} then ${requests}; "${badge.title}")`,
      );
      check(
        refusedFit.fits && refusedFit.reload,
        `${size.name}: the signed-out badge and Reload fit the screen (${refusedFit.scroll}px of ${refusedFit.width}px)`,
      );
      await live.context.close();
    }
    stopDemo(cutting);
  }
} finally {
  // From the spawn on, so a browser that fails to start, or a demo that never signs in, leaves no demo running.
  await browser?.close();
  for (const started of demos) stopDemo(started);
}

if (failures.length) {
  console.log(`${failures.length} failed`);
  process.exit(1);
}
