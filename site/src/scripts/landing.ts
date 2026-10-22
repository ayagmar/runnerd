// The landing page's behaviour: the budget calculator, the doctor checks, the copy button, the theme switch and
// the section entrances. Everything here is an enhancement; without it the page shows the example config.

const GIB = 1024 ** 3;
const UNITS: [string, number][] = [
  ['T', 1024 ** 4],
  ['G', GIB],
  ['M', 1024 ** 2],
  ['K', 1024],
];

/** Formats bytes the way runnerd's ByteSize does: the largest unit, two decimals, trailing zeros dropped. */
function size(bytes: number): string {
  const unit = UNITS.find(([, factor]) => bytes >= factor);
  if (!unit) return `${bytes}B`;
  const number = (bytes / unit[1]).toFixed(2).replace(/0+$/, '').replace(/\.$/, '');
  return `${number}${unit[0]}`;
}

function calculator() {
  const form = document.querySelector<HTMLFormElement>('[data-calc]');
  const bar = document.querySelector<HTMLElement>('[data-calc-bar]');
  const maxOut = document.querySelector<HTMLElement>('[data-out="max"]');
  const result = document.querySelector<HTMLElement>('[data-out="result"]');
  if (!form || !bar || !maxOut || !result) return;
  form.hidden = false;

  const number = (name: string) => Number((form.elements.namedItem(name) as HTMLInputElement).value);

  const render = () => {
    const max = number('max') * GIB;
    const profiles = ['trellis', 'glowly'].map((kind) => ({
      kind,
      memory: number(`${kind}-memory`) * GIB,
      slots: number(`${kind}-slots`),
    }));
    const mirror = (form.elements.namedItem('mirror') as HTMLInputElement).checked ? 512 * 1024 ** 2 : 0;
    const total = profiles.reduce((sum, p) => sum + p.memory * p.slots, 0) + mirror;

    for (const output of form.querySelectorAll<HTMLOutputElement>('output[data-for]')) {
      const name = output.dataset.for!;
      output.value = name.endsWith('slots') ? String(number(name)) : size(number(name) * GIB);
    }

    const scale = Math.max(max, total);
    const blocks = profiles.flatMap((p) => Array.from({ length: p.slots }, () => ({ kind: p.kind, bytes: p.memory })));
    if (mirror) blocks.push({ kind: 'mirror', bytes: mirror });
    const row = bar.querySelector<HTMLElement>('.blocks')!;
    row.replaceChildren(
      ...blocks.map(({ kind, bytes }) => {
        const block = document.createElement('div');
        block.className = `block ${kind}`;
        block.style.setProperty('--size', String(bytes / GIB));
        return block;
      }),
    );
    const free = document.createElement('div');
    free.className = 'free';
    free.style.setProperty('--size', String(Math.max(0, max - total) / GIB));
    row.append(free);
    bar.querySelector<HTMLElement>('.slice')!.style.setProperty('--share', String(max / scale));

    maxOut.textContent = size(max);
    const over = total > max;
    bar.classList.toggle('over', over);
    result.classList.toggle('error', over);
    result.textContent = over
      ? `error: slots × memory over all profiles plus the mirror is ${size(total)}, the slice ci.slice's MemoryMax is ${size(max)}`
      : `runnerd.toml: valid, memory caps sum to ${size(total)}`;
  };

  form.addEventListener('input', render);
  render();
}

function doctorTabs() {
  const list = document.querySelector<HTMLElement>('[data-tabs]');
  if (!list) return;
  list.hidden = false;
  const tabs = [...list.querySelectorAll<HTMLButtonElement>('[role="tab"]')];

  const select = (tab: HTMLButtonElement, focus = false) => {
    for (const other of tabs) {
      const selected = other === tab;
      other.setAttribute('aria-selected', String(selected));
      other.tabIndex = selected ? 0 : -1;
      document.getElementById(other.getAttribute('aria-controls')!)!.toggleAttribute('data-active', selected);
    }
    if (focus) tab.focus();
  };

  const finePointer = matchMedia('(hover: hover) and (pointer: fine)');
  tabs.forEach((tab, i) => {
    tab.addEventListener('click', () => select(tab));
    tab.addEventListener('pointerenter', () => finePointer.matches && select(tab));
    tab.addEventListener('keydown', (event) => {
      const step = { ArrowDown: 1, ArrowRight: 1, ArrowUp: -1, ArrowLeft: -1 }[event.key];
      if (step) {
        event.preventDefault();
        select(tabs[(i + step + tabs.length) % tabs.length], true);
      } else if (event.key === 'Home' || event.key === 'End') {
        event.preventDefault();
        select(tabs[event.key === 'Home' ? 0 : tabs.length - 1], true);
      }
    });
  });
}

function copyButton() {
  for (const button of document.querySelectorAll<HTMLButtonElement>('[data-copy]')) {
    if (!navigator.clipboard) continue;
    button.hidden = false;
    button.addEventListener('click', async () => {
      await navigator.clipboard.writeText(button.dataset.copy!);
      button.textContent = 'Copied';
      setTimeout(() => (button.textContent = 'Copy'), 1600);
    });
  }
}

function themeToggle() {
  const button = document.querySelector<HTMLButtonElement>('[data-theme-toggle]');
  if (!button) return;
  const label = () => {
    const next = document.documentElement.dataset.theme === 'light' ? 'dark' : 'light';
    button.setAttribute('aria-label', `Switch to ${next} theme`);
    button.firstElementChild!.textContent = next === 'light' ? 'Light' : 'Dark';
  };
  button.addEventListener('click', () => {
    const next = document.documentElement.dataset.theme === 'light' ? 'dark' : 'light';
    document.documentElement.dataset.theme = next;
    localStorage.setItem('starlight-theme', next);
    label();
  });
  label();
}

function entrances() {
  if (matchMedia('(prefers-reduced-motion: reduce)').matches) return;
  const groups = document.querySelectorAll<HTMLElement>(
    '.calc-text, .calc-result, .slots-head, .slot-steps, .isolation, .doctor-text, .roadmap, .install-tile, .footer',
  );
  const observer = new IntersectionObserver(
    (entries) => {
      for (const entry of entries) {
        if (!entry.isIntersecting) continue;
        entry.target.classList.add('in');
        observer.unobserve(entry.target);
      }
    },
    { rootMargin: '0px 0px -10% 0px' },
  );
  for (const group of groups) {
    [...group.children].forEach((child, i) => {
      const element = child as HTMLElement;
      element.dataset.reveal = '';
      element.style.setProperty('--d', String(Math.min(i, 6)));
      observer.observe(element);
    });
  }
}

export function setup() {
  calculator();
  doctorTabs();
  copyButton();
  themeToggle();
  entrances();
}
