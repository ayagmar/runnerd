// The landing page's dashboard overview, replayed: the web dashboard's demo daemon (crates/web/examples/demo) as a
// script of what it would show over six minutes. `at(t)` is the whole state at demo second t, so the page renders
// second 0 at build time and the script only moves t. Every number comes from the demo's own formulas: the fixture's
// samples up to second 0 (fake.rs, `job`), then the samples main.rs sends every 2 seconds. Nothing here is tuned.

const GIB = 1024 ** 3;
const MIB = 1024 ** 2;

/** Bytes the way runnerd's ByteSize prints them: the largest unit, two decimals, trailing zeros dropped. */
export function size(bytes: number): string {
  const units: [string, number][] = [
    ['T', 1024 ** 4],
    ['G', GIB],
    ['M', MIB],
    ['K', 1024],
  ];
  const unit = units.find(([, factor]) => bytes >= factor);
  if (!unit) return `${Math.round(bytes)}B`;
  return `${(bytes / unit[1]).toFixed(2).replace(/0+$/, '').replace(/\.$/, '')}${unit[0]}`;
}

/** Millicores as cores with two decimals, trailing zeros dropped, as the dashboard prints them. */
const cores = (millicores: number) => (millicores / 1000).toFixed(2).replace(/0+$/, '').replace(/\.$/, '');

/** The dashboard's run time: 9s, 8m 46s. */
function duration(s: number): string {
  s = Math.max(0, Math.floor(s));
  return s < 60 ? `${s}s` : `${Math.floor(s / 60)}m ${String(s % 60).padStart(2, '0')}s`;
}

const ago = (s: number) => (s < 10 ? 'just now' : s < 60 ? `${Math.floor(s)} s ago` : `${Math.floor(s / 60)} min ago`);

// The fixtures' two workflows, step names and seconds from fake.rs.
type Step = [name: string, seconds: number];
const LEDGER: Step[] = [
  ['Set up job', 3],
  ['Run actions/checkout@v6', 4],
  ['Set up JDK 21', 6],
  ['Restore Gradle cache', 9],
  ['Build', 148],
  ['Unit tests', 212],
  ['Integration tests (Testcontainers)', 281],
  ['Post Run actions/checkout@v6', 1],
  ['Complete job', 1],
];
const STOREFRONT: Step[] = [
  ['Set up job', 2],
  ['Run actions/checkout@v6', 3],
  ['Set up pnpm', 5],
  ['pnpm install --frozen-lockfile', 41],
  ['Build', 96],
  ['Playwright e2e', 388],
  ['Post Run actions/checkout@v6', 1],
  ['Complete job', 1],
];

interface Sample {
  at: number;
  memory: number;
  cpu: number;
}

interface Run {
  repo: string;
  name: string;
  steps: Step[];
  /** The fixture's `Spec::memory`: what its sampled memory is drawn around. */
  memory: number;
  /** Demo second the runner's container started; it registers and waits for its job until `started`. */
  container: number;
  started: number;
  /** The fixture's samples up to second 0, for the two jobs running when the demo starts. */
  history: Sample[];
}

interface Profile {
  name: string;
  memory: number;
  cpus: number;
  /** The CSS class of its colour: the first profile mint, the second pink, as on the dashboard. */
  tone: string;
  slots: Run[][];
}

const length = (run: Run) => run.steps.reduce((sum, step) => sum + step[1], 0);

/** fake.rs's `Noise`: a 64-bit LCG seeded with 7, so the fixture's samples come out the same here. */
class Noise {
  state = 7n;
  next(below: number): number {
    this.state = (this.state * 6_364_136_223_846_793_005n + 1_442_695_040_888_963_407n) & 0xffff_ffff_ffff_ffffn;
    return Number((this.state >> 33n) % BigInt(Math.max(1, below)));
  }
}

/** fake.rs's `job` samples of a job running at second 0: every 2 s from its container's start, in the order it adds
 * them (ledger, then storefront), so each draws the same noise. */
function history(run: Run, noise: Noise): Sample[] {
  const samples: Sample[] = [];
  for (let at = run.container; at <= 0; at += 2) {
    const progress = Math.floor(((at - run.container) * 100) / Math.max(1, -run.container));
    const memory = Math.floor((run.memory * (35 + Math.min(progress, 60))) / 100) + noise.next(Math.floor(run.memory / 10));
    const cpu = 400 + noise.next(progress > 20 ? 7200 : 2000);
    noise.next(3 * MIB);
    samples.push({ at, memory, cpu });
  }
  return samples;
}

/** main.rs's live sample of a running job at demo second n (even, from 2): around 1.5 times its first sample. A job
 * the replay starts has no fixture samples; its first is `job`'s formula at progress 0, without the noise. */
function live(run: Run, n: number): Sample {
  const first = run.history[0]?.memory ?? Math.floor((run.memory * 35) / 100);
  const base = Math.floor((first * 3) / 2);
  return { at: n, memory: base + ((n * 7919) % 97) * Math.floor(base / 600), cpu: 1500 + ((n * 6151) % 5200) };
}

/** A running job's last sample at demo second t. */
function sampleAt(run: Run, t: number): Sample {
  if (t >= 2 && t - (t % 2) > run.started) return live(run, t - (t % 2));
  return run.history.length ? run.history[run.history.length - 1] : live(run, Math.max(2, run.started + 2 - (run.started % 2)));
}

/** fake.rs's last host sample, at second 0: 600M, 4G for each of the two running jobs, and its noise once all 24
 * history jobs have drawn theirs. Checked in, so the page doesn't replay those jobs to reach it. */
const FIXTURE_SLICE = 9_742_430_564;

/** The slice's sample at demo second t: the fixture's until main.rs sends its first, at second 2, then main.rs's: 9G
 * and a little more, whatever runs. */
function slice(t: number): number {
  if (t < 2) return FIXTURE_SLICE;
  const n = t - (t % 2);
  const base = 9 * GIB;
  return base + ((n * 104_729) % 89) * Math.floor(base / 300);
}

// Second 0 is the fixture: ledger's build 422 s in (step 7 of 9), storefront's playwright 305 s in (step 6 of 8),
// a storefront runner in node slot 2 for the one queued job, jvm slot 2 free. Then that runner takes its job,
// both first jobs finish and free their slots, and a new ledger job gets a runner in jvm slot 1, the first free.
const ledger = 'halyard/ledger';
const storefront = 'halyard/storefront';
const LEDGER_MEMORY = Math.floor((9 * GIB * 7) / 10);
const STOREFRONT_MEMORY = Math.floor((7 * GIB * 6) / 10);
const run = (repo: string, name: string, steps: Step[], memory: number, container: number, started: number): Run => ({
  repo,
  name,
  steps,
  memory,
  container,
  started,
  history: [],
});
const firstLedger = run(ledger, 'build', LEDGER, LEDGER_MEMORY, -427, -422);
const firstStorefront = run(storefront, 'playwright', STOREFRONT, STOREFRONT_MEMORY, -309, -305);
const noise = new Noise();
firstLedger.history = history(firstLedger, noise);
firstStorefront.history = history(firstStorefront, noise);
const PROFILES: Profile[] = [
  {
    name: 'jvm',
    memory: 9 * GIB,
    cpus: 8,
    tone: 'p0',
    slots: [[firstLedger, run(ledger, 'build', LEDGER, LEDGER_MEMORY, 258, 268)], []],
  },
  {
    name: 'node',
    memory: 7 * GIB,
    cpus: 8,
    tone: 'p1',
    slots: [[firstStorefront], [run(storefront, 'playwright', STOREFRONT, STOREFRONT_MEMORY, -12, 6)]],
  },
];
const MAX = 33 * GIB;
const MIRROR = 512 * MIB;
export const LOOP = 360;
/** When a slot changes, for the scrubber's marks: a runner starts, takes its job, or its job ends. */
export const MOMENTS = [
  ...new Set(
    PROFILES.flatMap((p) => p.slots.flat().flatMap((r) => [r.container, r.started, r.started + length(r)])).filter(
      (t) => t > 0 && t < LOOP,
    ),
  ),
].sort((a, b) => a - b);
/** Demo seconds per second of playback. */
export const SPEED = 8;
/** How far back a slot's sparkline reaches, as on the dashboard. */
const SPARK = 300;

interface Slot {
  key: string;
  profile: Profile;
  number: number;
  state: 'idle' | 'starting' | 'busy';
  /** The runner in the slot now, if any. */
  run?: Run;
  /** The slot's latest job that has started, running or done: an open card keeps listing its steps. */
  last?: Run;
  /** How many steps its jobs have: an open card keeps room for them all. */
  rows: number;
  /** Index of the step running, which is also how many are done; for a busy slot. */
  step?: number;
  sample?: Sample;
  t: number;
}

export function at(t: number) {
  const slots: Slot[] = PROFILES.flatMap((profile) =>
    profile.slots.map((runs, i) => {
      const last = runs.filter((r) => r.started <= t).pop();
      const rows = Math.max(0, ...runs.map((r) => r.steps.length));
      const slot: Slot = { key: `${profile.name}-${i + 1}`, profile, number: i + 1, state: 'idle', rows, last, t };
      const r = runs.find((r) => t >= r.container && t < r.started + length(r));
      if (!r) return slot;
      if (t < r.started) return { ...slot, run: r, state: 'starting' };
      let end = r.started;
      const step = r.steps.findIndex(([, seconds]) => t < (end += seconds));
      return { ...slot, run: r, state: 'busy', step, sample: sampleAt(r, t) };
    }),
  );
  const reserved = slots.filter((s) => s.state !== 'idle').reduce((sum, s) => sum + s.profile.memory, 0) + MIRROR;
  return { t, slots, reserved, inUse: slice(t) };
}

type State = ReturnType<typeof at>;

const escape = (text: string) => text.replace(/&/g, '&amp;').replace(/</g, '&lt;');

/** The readout: the slice's total, which stays while the lens beside it says what one block holds. */
export function readout(state: State): string {
  return `<span class="big">${size(state.reserved)}</span> of ${size(MAX)} in <code>ci.slice</code><span class="muted"> · ${size(state.inUse)} in use</span>`;
}

const FREE = MAX - MIRROR - PROFILES.reduce((sum, p) => sum + p.memory * p.slots.length, 0);

/** The budget's blocks after the slots, the mirror and what is free, in GiB. */
export const rest = () => ({ mirror: MIRROR / GIB, free: FREE / GIB });

/** The lens: a line per slot saying what it holds, all in one place and hidden but for the selected slot's, so the
 * readout is as tall as its longest line and selecting a slot moves nothing. */
export function lenses(state: State, selected: string | null): string {
  return state.slots.map((slot) => `<span${slot.key === selected ? ' class="on"' : ''}>${lens(slot)}</span>`).join('');
}

/** What one slot holds, its figure in bold. */
function lens(slot: Slot): string {
  const name = `${slot.profile.name} ${slot.number}`;
  const cap = size(slot.profile.memory);
  if (slot.state === 'busy')
    return `<strong>${size(slot.sample!.memory)}</strong> of ${cap} in ${name}, ${slot.run!.name} on ${slot.run!.repo}`;
  if (slot.state === 'starting') return `<strong>${cap}</strong> ${name}, a runner for ${slot.run!.repo} waiting for its job`;
  return `<strong>${cap}</strong> ${name}, idle`;
}

/** A slot's block in the budget: its size and name; `--fill` its job's memory against the cap. */
export function block(slot: Slot) {
  const fill = slot.sample ? Math.min(1, slot.sample.memory / slot.profile.memory) : 0;
  return {
    className: `block ${slot.profile.tone} ${slot.state}`,
    fill: fill.toFixed(3),
    html: `<span class="fill"></span><span class="size">${size(slot.profile.memory)}</span><span class="name">${slot.profile.name} ${slot.number}</span>`,
  };
}

/** The bar's accessible name: what each slot holds, which changes only when a slot does. */
export function barLabel(state: State): string {
  const slots = state.slots.map((slot) => {
    const what = slot.state === 'busy' ? `${slot.run!.name} on ${slot.run!.repo}` : slot.state === 'starting' ? `a runner for ${slot.run!.repo}` : 'idle';
    return `${slot.profile.name} ${slot.number} ${size(slot.profile.memory)}, ${what}`;
  });
  return `Memory budget of ci.slice: ${size(state.reserved)} of ${size(MAX)} reserved; ${slots.join('; ')}; 512M mirror`;
}

/** A sparkline from the job's start or five minutes back, whichever is later, in the dashboard's 1000 by 100 box:
 * the fixture's samples, then the live ones. */
function spark(slot: Slot, key: 'memory' | 'cpu', top: number): string {
  const r = slot.run!;
  const from = Math.max(r.started, slot.t - SPARK);
  const samples = [...r.history, ...Array.from({ length: Math.floor(slot.t / 2) }, (_, i) => 2 * (i + 1))
    .filter((n) => n > r.started)
    .map((n) => live(r, n))].filter((s) => s.at >= from && s.at <= slot.t);
  if (samples.length < 2) return '';
  const points = samples.map((s) => `${(((s.at - from) / Math.max(1, slot.t - from)) * 1000).toFixed(1)},${(100 - Math.min(1, s[key] / top) * 100).toFixed(1)}`);
  const x0 = points[0].split(',')[0];
  const x1 = points[points.length - 1].split(',')[0];
  return `<svg class="sparkline" viewBox="0 0 1000 100" preserveAspectRatio="none" aria-hidden="true"><path class="area ${key}" d="M${x0},100L${points.join('L')}L${x1},100Z"/><path class="line ${key}" d="M${points.join('L')}" vector-effect="non-scaling-stroke"/></svg>`;
}

/** A slot's card under its heading, as the dashboard draws it. */
export function card(slot: Slot): string {
  const r = slot.run;
  if (slot.state === 'idle') return '';
  if (slot.state === 'starting')
    return `<span class="repo">${r!.repo}</span><p class="slot-idle">Waiting for its job <span class="muted">${ago(slot.t - r!.container)}</span></p>`;
  const steps = r!.steps;
  const s = slot.sample!;
  return `<div class="slot-title"><p class="slot-job"><span class="repo">${r!.repo}</span><span class="job-name">${r!.name}</span></p><p class="elapsed"><span class="visually-hidden">Running for </span>${duration(slot.t - r!.started)}</p></div><p class="step"><span class="step-count">${slot.step}/${steps.length}</span> ${escape(steps[slot.step!][0])}</p><dl class="slot-figures"><div><dt class="visually-hidden">Memory</dt><dd>${size(s.memory)}</dd>${spark(slot, 'memory', slot.profile.memory)}</div><div><dt class="visually-hidden">CPU</dt><dd>${cores(s.cpu)} cores</dd>${spark(slot, 'cpu', slot.profile.cpus * 1000)}</div></dl>`;
}

/** The steps of a slot's latest job, as its job page lists them: done with their time, the one running, those to
 * come. A finished job's stay listed until the next one starts, so the card keeps its height. */
export function steps(slot: Slot): string {
  const r = slot.last;
  if (!r) return '';
  let start = r.started;
  return `<ol>${r.steps
    .map(([name, seconds]) => {
      const end = start + seconds;
      const state = slot.t >= end ? 'done' : slot.t >= start ? 'now' : 'next';
      const time = state === 'done' ? duration(seconds) : state === 'now' ? duration(slot.t - start) : '';
      start = end;
      return `<li class="${state}"${state === 'now' ? ' aria-current="step"' : ''}><span>${escape(name)}</span><span>${time}</span></li>`;
    })
    .join('')}</ol>`;
}

/** Plays the replay in the overview at `root`: it moves t while on screen, and the visitor can pause, scrub and pick a slot. */
export function mount(root: HTMLElement) {
  const blocks = new Map([...root.querySelectorAll<HTMLElement>('[data-block]')].map((el) => [el.dataset.block!, el]));
  const cards = new Map([...root.querySelectorAll<HTMLElement>('[data-card]')].map((el) => [el.dataset.card!, el]));
  const bar = root.querySelector<HTMLElement>('[data-bar]')!;
  const readoutEl = root.querySelector<HTMLElement>('[data-readout]')!;
  const lensEl = root.querySelector<HTMLElement>('[data-lens]')!;
  const play = root.querySelector<HTMLButtonElement>('[data-play]')!;
  const scrub = root.querySelector<HTMLInputElement>('[data-scrub]')!;
  const clock = root.querySelector<HTMLElement>('[data-clock]')!;
  root.querySelector<HTMLElement>('[data-controls]')!.hidden = false;

  let t = 0;
  // The build opens jvm 1's steps, so the page shows that a card opens; the visitor picks another or closes it.
  let picked: string | null = root.querySelector<HTMLElement>('[aria-expanded="true"]')?.closest<HTMLElement>('[data-card]')?.dataset.card ?? null;
  let hovered: string | null = null;
  let playing = !matchMedia('(prefers-reduced-motion: reduce)').matches;
  let visible = false;
  let timer = 0;

  const set = (el: HTMLElement, html: string) => {
    if (el.dataset.html !== html) {
      el.innerHTML = html;
      el.dataset.html = html;
    }
  };

  const render = () => {
    const state = at(t);
    set(readoutEl, readout(state));
    set(lensEl, lenses(state, hovered));
    bar.setAttribute('aria-label', barLabel(state));
    for (const slot of state.slots) {
      const b = block(slot);
      const linked = slot.key === hovered;
      const blockEl = blocks.get(slot.key)!;
      blockEl.className = `${b.className}${linked ? ' linked' : ''}`;
      blockEl.style.setProperty('--fill', b.fill);
      set(blockEl, b.html);
      const cardEl = cards.get(slot.key)!;
      cardEl.className = `card ${slot.profile.tone} ${slot.state}${linked ? ' linked' : ''}`;
      set(cardEl.querySelector<HTMLElement>('[data-body]')!, card(slot));
      const pick = cardEl.querySelector<HTMLElement>('[data-pick]')!;
      if (slot.rows) pick.setAttribute('aria-expanded', String(picked === slot.key));
      else pick.removeAttribute('aria-expanded');
      const stepsEl = cardEl.querySelector<HTMLElement>('[data-steps]')!;
      stepsEl.toggleAttribute('data-open', picked === slot.key && slot.rows > 0);
      set(stepsEl, picked === slot.key ? steps(slot) : '');
    }
    scrub.value = String(Math.floor(t));
    const time = `${Math.floor(t / 60)}:${String(Math.floor(t % 60)).padStart(2, '0')}`;
    clock.textContent = time;
    scrub.setAttribute('aria-valuetext', time);
  };

  const run = () => {
    clearInterval(timer);
    root.dataset.playing = String(playing);
    play.setAttribute('aria-label', playing ? 'Pause the replay' : 'Play the replay');
    if (!playing || !visible || document.hidden) return;
    timer = window.setInterval(() => {
      t = (t + SPEED / 4) % LOOP;
      render();
    }, 250);
  };

  play.addEventListener('click', () => {
    playing = !playing;
    run();
  });
  scrub.addEventListener('input', () => {
    t = Number(scrub.value);
    render();
  });

  // A slot's block and card are one thing: pointing at, focusing or tapping either outlines both and puts the slot in
  // the lens, as the dashboard does. A click or tap, or Enter on the card, also opens or closes its steps; Escape
  // closes them.
  const hover = (key: string | null) => {
    hovered = key;
    render();
  };
  // A slot that never runs a job in the replay has no steps to open.
  const pick = (key: string) => {
    hovered = key;
    if (!at(t).slots.find((slot) => slot.key === key)?.rows) return render();
    picked = picked === key ? null : key;
    render();
  };
  for (const [key, el] of [...blocks, ...cards]) {
    el.addEventListener('pointerenter', (e) => e.pointerType === 'mouse' && hover(key));
    el.addEventListener('pointerleave', (e) => e.pointerType === 'mouse' && hovered === key && hover(null));
  }
  for (const [key, el] of blocks) el.addEventListener('click', () => pick(key));
  for (const [key, el] of cards) {
    const button = el.querySelector<HTMLElement>('[data-pick]')!;
    button.addEventListener('focus', () => hover(key));
    button.addEventListener('blur', () => hovered === key && hover(null));
    button.addEventListener('click', () => pick(key));
  }
  root.addEventListener('keydown', (e) => {
    if (e.key !== 'Escape' || !picked) return;
    picked = null;
    render();
  });

  new IntersectionObserver(([entry]) => {
    visible = entry.isIntersecting;
    run();
  }).observe(root);
  document.addEventListener('visibilitychange', run);
  render();
  run();
}
