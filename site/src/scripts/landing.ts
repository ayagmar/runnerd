// The landing page's behaviour: the copy buttons, the theme switch and the entrance of each section's parts. All are
// enhancements; without the script the commands stay plain, selectable text, the page stays dark and every part is
// already in place.

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
    button.textContent = next === 'light' ? 'Light' : 'Dark';
  };
  button.addEventListener('click', () => {
    const next = document.documentElement.dataset.theme === 'light' ? 'dark' : 'light';
    document.documentElement.dataset.theme = next;
    localStorage.setItem('starlight-theme', next);
    label();
  });
  label();
}

// Each section's parts below the first screen rise in once as they near the viewport, under its heading. The class
// that hides them is added here, so a failed script never leaves one hidden.
function reveal() {
  if (matchMedia('(prefers-reduced-motion: reduce)').matches) return;
  const parts = document.querySelectorAll<HTMLElement>('main .part-text > :not(h2), main .part > :not(.part-text, .steps, h2), .steps > li');
  const observer = new IntersectionObserver(
    (entries) => {
      for (const entry of entries) {
        if (!entry.isIntersecting) continue;
        entry.target.classList.add('in');
        observer.unobserve(entry.target);
      }
    },
    { rootMargin: '0px 0px -8% 0px' },
  );
  // Read every position before writing any style, so the page lays out once.
  const tops = Array.from(parts, (tile) => tile.getBoundingClientRect().top);
  for (const [i, tile] of parts.entries()) {
    // What the first screen already shows stays put: an entrance would only delay the largest paint.
    if (tops[i]! < innerHeight) {
      tile.classList.add('in');
      continue;
    }
    tile.style.setProperty('--d', `${(i % 3) * 80}ms`);
    tile.classList.add('reveal');
    requestAnimationFrame(() => observer.observe(tile));
  }
}

// A table that is wider than its box scrolls; a line above it says so, and goes when the tile is wide enough.
function scrollCues() {
  const observer = new ResizeObserver((entries) => {
    for (const { target } of entries) {
      const cue = target.previousElementSibling;
      if (cue instanceof HTMLElement && cue.classList.contains('cue')) cue.hidden = target.scrollWidth <= target.clientWidth;
    }
  });
  for (const pre of document.querySelectorAll('pre.terminal.wide')) {
    const cue = document.createElement('p');
    cue.className = 'cue';
    cue.textContent = 'Scroll sideways to read the rest →';
    cue.hidden = true;
    pre.before(cue);
    observer.observe(pre);
  }
}

export function setup() {
  copyButton();
  scrollCues();
  themeToggle();
  reveal();
}
