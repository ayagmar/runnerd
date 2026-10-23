// The landing page's dashboard overview, replayed: the web dashboard's demo daemon (crates/web/examples/demo) as a
// script of what it would show over six minutes. `at(t)` is the whole state at demo second t, so the page renders
// second 0 at build time and the script only moves t. It must not invent states or numbers the dashboard does not have.

const GIB = 1024 ** 3;

/** Bytes the way runnerd's ByteSize prints them: the largest unit, two decimals, trailing zeros dropped. */
export function size(bytes: number): string {
  const units: [string, number][] = [
    ['T', 1024 ** 4],
    ['G', GIB],
    ['M', 1024 ** 2],
    ['K', 1024],
  ];
  const unit = units.find(([, factor]) => bytes >= factor);
  if (!unit) return `${Math.round(bytes)}B`;
  return `${(bytes / unit[1]).toFixed(2).replace(/0+$/, '').replace(/\.$/, '')}${unit[0]}`;
}

/** Cores with two decimals, trailing zeros dropped, as the dashboard prints them. */
const cores = (n: number) => n.toFixed(2).replace(/0+$/, '').replace(/\.$/, '');

/** The dashboard's run time: 9s, 8m 46s. */
function duration(s: number): string {
  s = Math.max(0, Math.floor(s));
  return s < 60 ? `${s}s` : `${Math.floor(s / 60)}m ${String(s % 60).padStart(2, '0')}s`;
}

const ago = (s: number) => (s < 10 ? 'just now' : s < 60 ? `${Math.floor(s)} s ago` : `${Math.floor(s / 60)} min ago`);

// The fixtures' two workflows: step names and seconds from fake.rs; memory (GiB) and CPU (cores) each step settles
// at, chosen so the replay passes through the published screenshot's 4.14G and 2.63G.
type Step = [name: string, seconds: number, memory: number, cpu: number];
const LEDGER: Step[] = [
  ['Set up job', 3, 0.3, 0.5],
  ['Run actions/checkout@v6', 4, 0.4, 0.8],
  ['Set up JDK 21', 6, 0.7, 1.2],
  ['Restore Gradle cache', 9, 0.9, 1.5],
  ['Build', 148, 2.6, 6.2],
  ['Unit tests', 212, 3.3, 4.5],
  ['Integration tests (Testcontainers)', 281, 4.1, 1.6],
  ['Post Run actions/checkout@v6', 1, 4.1, 0.6],
  ['Complete job', 1, 4.1, 0.4],
];
const STOREFRONT: Step[] = [
  ['Set up job', 2, 0.3, 0.5],
  ['Run actions/checkout@v6', 3, 0.4, 0.8],
  ['Set up pnpm', 5, 0.5, 1],
  ['pnpm install --frozen-lockfile', 41, 1.1, 3.5],
  ['Build', 96, 1.8, 5.5],
  ['Playwright e2e', 388, 2.6, 1.6],
  ['Post Run actions/checkout@v6', 1, 2.6, 0.6],
  ['Complete job', 1, 2.6, 0.4],
];

interface Run {
  repo: string;
  name: string;
  steps: Step[];
  /** Demo second the runner's container started; it registers and waits for its job until `started`. */
  container: number;
  started: number;
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

// Second 0 is the fixture: ledger's build 422 s in (step 7 of 9), storefront's playwright 305 s in (step 6 of 8),
// a storefront runner in node slot 2 for the one queued job, jvm slot 2 free. Then that runner takes its job,
// both first jobs finish and free their slots, and a new ledger job gets a runner in jvm slot 1, the first free.
const ledger = 'halyard/ledger';
const storefront = 'halyard/storefront';
const PROFILES: Profile[] = [
  {
    name: 'jvm',
    memory: 9 * GIB,
    cpus: 8,
    tone: 'p0',
    slots: [
      [
        { repo: ledger, name: 'build', steps: LEDGER, container: -426, started: -422 },
        { repo: ledger, name: 'build', steps: LEDGER, container: 258, started: 268 },
      ],
      [],
    ],
  },
  {
    name: 'node',
    memory: 7 * GIB,
    cpus: 8,
    tone: 'p1',
    slots: [
      [{ repo: storefront, name: 'playwright', steps: STOREFRONT, container: -309, started: -305 }],
      [{ repo: storefront, name: 'playwright', steps: STOREFRONT, container: -12, started: 6 }],
    ],
  },
];
const MAX = 33 * GIB;
const MIRROR = 512 * 1024 ** 2;
export const LOOP = 360;
/** When a slot changes, for the scrubber's marks: a runner starts, takes its job, or its job ends. */
export const MOMENTS = [
  ...new Set(
    PROFILES.flatMap((p) => p.slots.flat().flatMap((run) => [run.container, run.started, run.started + length(run)])).filter(
      (t) => t > 0 && t < LOOP,
    ),
  ),
].sort((a, b) => a - b);
/** Demo seconds per second of playback. */
export const SPEED = 8;
/** How far back a slot's sparkline reaches, as on the dashboard. */
const SPARK = 300;

/** Deterministic noise in [-1, 1], so every replay and the built page agree. */
function noise(seed: number): number {
  const x = Math.sin(seed * 12.9898 + 78.233) * 43758.5453;
  return (x - Math.floor(x)) * 2 - 1;
}

/** A running job's sample at demo second t: each step ramps from the last step's level to its own over 30 s. */
function sample(run: Run, t: number) {
  const elapsed = t - run.started;
  let at = 0;
  let from = [0.25, 0.4];
  for (const [index, [, seconds, memory, cpu]] of run.steps.entries()) {
    if (elapsed < at + seconds || index === run.steps.length - 1) {
      const k = Math.min(1, Math.max(0, (elapsed - at) / 30));
      const seed = Math.floor(t / 2) * 2 + run.started;
      return {
        memory: (from[0] + (memory - from[0]) * k + noise(seed) * memory * 0.03) * GIB,
        cpu: Math.max(0.2, from[1] + (cpu - from[1]) * k + noise(seed + 0.5) * cpu * 0.45),
      };
    }
    at += seconds;
    from = [memory, cpu];
  }
  return { memory: 0, cpu: 0 };
}

interface Slot {
  key: string;
  profile: Profile;
  number: number;
  state: 'idle' | 'starting' | 'busy';
  run?: Run;
  /** Index of the step running, which is also how many are done; for a busy slot. */
  step?: number;
  memory?: number;
  cpu?: number;
  t: number;
}

export function at(t: number) {
  const slots: Slot[] = PROFILES.flatMap((profile) =>
    profile.slots.map((runs, i) => {
      const slot: Slot = { key: `${profile.name}-${i + 1}`, profile, number: i + 1, state: 'idle', t };
      const run = runs.find((r) => t >= r.container && t < r.started + length(r));
      if (!run) return slot;
      slot.run = run;
      if (t < run.started) return { ...slot, state: 'starting' };
      let end = run.started;
      const step = run.steps.findIndex(([, seconds]) => t < (end += seconds));
      return { ...slot, state: 'busy', step, ...sample(run, t) };
    }),
  );
  const running = slots.filter((s) => s.state !== 'idle');
  const reserved = running.reduce((sum, s) => sum + s.profile.memory, 0) + MIRROR;
  const jobs = slots.filter((s) => s.state === 'busy');
  // The slice holds more than the jobs' sampled memory: Docker inside each runner, page cache. The fixture's host
  // samples sit at 600M with nothing running.
  const inUse = 0.6 * GIB + jobs.reduce((sum, s) => sum + s.memory! * 1.55, 0) + noise(Math.floor(t / 2)) * 0.15 * GIB;
  return { t, slots, reserved, inUse };
}

type State = ReturnType<typeof at>;

const escape = (text: string) => text.replace(/&/g, '&amp;').replace(/</g, '&lt;');

/** What the readout above the bar says about one block: a figure in the big type, and the words after it. */
interface Lens {
  figure: string;
  text: string;
}

/** The readout: the slice's total, or what the block under the pointer or focus holds. */
export function readout(state: State, lens?: Lens): string {
  if (lens) return `<span class="big">${lens.figure}</span> ${escape(lens.text)}`;
  return `<span class="big">${size(state.reserved)}</span> of ${size(MAX)} in <code>ci.slice</code><span class="muted"> · ${size(state.inUse)} in use</span>`;
}

const FREE = MAX - MIRROR - PROFILES.reduce((sum, p) => sum + p.memory * p.slots.length, 0);

/** The budget's blocks after the slots, the mirror and what is free, in GiB. */
export const rest = () => ({ mirror: MIRROR / GIB, free: FREE / GIB });

function lens(slot: Slot): Lens {
  const name = `${slot.profile.name} ${slot.number}`;
  const cap = size(slot.profile.memory);
  if (slot.state === 'busy') return { figure: size(slot.memory!), text: `of ${cap} in ${name}, ${slot.run!.name} on ${slot.run!.repo}` };
  if (slot.state === 'starting') return { figure: cap, text: `${name}, a runner for ${slot.run!.repo} waiting for its job` };
  return { figure: cap, text: `${name}, idle` };
}

/** A slot's block in the budget: its size and name; `--fill` its job's memory against the cap. */
export function block(slot: Slot) {
  const fill = slot.memory ? Math.min(1, slot.memory / slot.profile.memory) : 0;
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

/** A sparkline from the job's start or five minutes back, whichever is later, in the dashboard's 1000 by 100 box. */
function spark(slot: Slot, key: 'memory' | 'cpu', top: number): string {
  const run = slot.run!;
  const from = Math.max(run.started, slot.t - SPARK);
  const points: string[] = [];
  for (let at = from - (from % 2); at <= slot.t; at += 2) {
    const x = ((Math.max(at, from) - from) / Math.max(1, slot.t - from)) * 1000;
    const y = 100 - Math.min(1, sample(run, at)[key] / top) * 100;
    points.push(`${x.toFixed(1)},${y.toFixed(1)}`);
  }
  if (points.length < 2) return '';
  const first = points[0].split(',')[0];
  const last = points[points.length - 1].split(',')[0];
  return `<svg class="sparkline" viewBox="0 0 1000 100" preserveAspectRatio="none" aria-hidden="true"><path class="area ${key}" d="M${first},100L${points.join('L')}L${last},100Z"/><path class="line ${key}" d="M${points.join('L')}" vector-effect="non-scaling-stroke"/></svg>`;
}

/** A slot's card under its heading, as the dashboard draws it. */
export function card(slot: Slot): string {
  const { run } = slot;
  if (slot.state === 'idle') return '';
  if (slot.state === 'starting')
    return `<span class="repo">${run!.repo}</span><p class="slot-idle">Waiting for its job <span class="muted">${ago(slot.t - run!.container)}</span></p>`;
  const steps = run!.steps;
  return `<div class="slot-title"><p class="slot-job"><span class="repo">${run!.repo}</span><span class="job-name">${run!.name}</span></p><p class="elapsed"><span class="visually-hidden">Running for </span>${duration(slot.t - run!.started)}</p></div><p class="step"><span class="step-count">${slot.step}/${steps.length}</span> ${escape(steps[slot.step!][0])}</p><dl class="slot-figures"><div><dt class="visually-hidden">Memory</dt><dd>${size(slot.memory!)}</dd>${spark(slot, 'memory', slot.profile.memory)}</div><div><dt class="visually-hidden">CPU</dt><dd>${cores(slot.cpu!)} cores</dd>${spark(slot, 'cpu', slot.profile.cpus)}</div></dl>`;
}

/** A busy slot's steps, as its job page lists them: done with their time, the one running, those to come. */
export function steps(slot: Slot): string {
  if (slot.state !== 'busy') return '';
  let start = slot.run!.started;
  return `<ol>${slot
    .run!.steps.map(([name, seconds], i) => {
      const state = i < slot.step! ? 'done' : i === slot.step ? 'now' : 'next';
      const time = state === 'done' ? duration(seconds) : state === 'now' ? duration(slot.t - start) : '';
      start += seconds;
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
  const play = root.querySelector<HTMLButtonElement>('[data-play]')!;
  const scrub = root.querySelector<HTMLInputElement>('[data-scrub]')!;
  const clock = root.querySelector<HTMLOutputElement>('[data-clock]')!;
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
    const hot = state.slots.find((slot) => slot.key === hovered);
    set(readoutEl, readout(state, hot && lens(hot)));
    bar.setAttribute('aria-label', barLabel(state));
    for (const slot of state.slots) {
      const b = block(slot);
      const blockEl = blocks.get(slot.key)!;
      blockEl.className = b.className;
      blockEl.style.setProperty('--fill', b.fill);
      set(blockEl, b.html);
      const cardEl = cards.get(slot.key)!;
      cardEl.className = `card ${slot.profile.tone} ${slot.state}`;
      set(cardEl.querySelector<HTMLElement>('[data-body]')!, card(slot));
      const open = picked === slot.key && slot.state === 'busy';
      const pick = cardEl.querySelector<HTMLElement>('[data-pick]')!;
      if (slot.state === 'busy') pick.setAttribute('aria-expanded', String(open));
      else pick.removeAttribute('aria-expanded');
      set(cardEl.querySelector<HTMLElement>('[data-steps]')!, open ? steps(slot) : '');
    }
    scrub.value = String(Math.floor(t));
    const time = `${Math.floor(t / 60)}:${String(Math.floor(t % 60)).padStart(2, '0')}`;
    clock.value = time;
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

  // A slot's block and card are one thing: pointing at either, or focusing the card, lights both and puts the slot
  // in the readout. A click or tap on either, or Enter on the card, opens or closes its steps; Escape closes them.
  const hover = (key: string | null) => {
    hovered = key;
    if (key) root.dataset.hot = key;
    else delete root.dataset.hot;
    render();
  };
  const pick = (key: string | null) => {
    picked = picked === key ? null : key;
    render();
  };
  for (const [key, el] of [...blocks, ...cards]) {
    el.addEventListener('pointerenter', (e) => e.pointerType === 'mouse' && hover(key));
    el.addEventListener('pointerleave', () => hover(null));
  }
  for (const [key, el] of blocks) el.addEventListener('click', () => pick(key));
  for (const [key, el] of cards) {
    const button = el.querySelector<HTMLElement>('[data-pick]')!;
    button.addEventListener('focus', () => hover(key));
    button.addEventListener('blur', () => hover(null));
    button.addEventListener('click', () => pick(key));
  }
  root.addEventListener('keydown', (e) => {
    if (e.key === 'Escape' && picked) pick(picked);
  });

  new IntersectionObserver(([entry]) => {
    visible = entry.isIntersecting;
    run();
  }).observe(root);
  document.addEventListener('visibilitychange', run);
  render();
  run();
}
