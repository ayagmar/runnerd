// The social card, public/og.png: the landing page's first 1200 by 630 at 1x in the dark theme, with the lanes under
// measured peaks. Shoots a running `pnpm -C site preview`:
//
//   pnpm -C site build && pnpm -C site preview --port 4329 &
//   pnpm -C site exec node scripts/capture/card.mjs http://localhost:4329/runnerd/
import { chromium } from 'playwright-core';

const url = process.argv[2];
if (!url) throw new Error('usage: card.mjs <the landing page’s URL>');
const browser = await chromium.launch({ executablePath: process.env.CHROMIUM || undefined });
const context = await browser.newContext({ viewport: { width: 1200, height: 630 }, colorScheme: 'dark', reducedMotion: 'reduce' });
const page = await context.newPage();
await page.goto(url, { waitUntil: 'networkidle' });
await page.screenshot({ path: new URL('../../public/og.png', import.meta.url).pathname });
await browser.close();
