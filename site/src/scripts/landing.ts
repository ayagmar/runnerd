// The landing page's behaviour: the copy buttons, the theme switch and the cue on a terminal wider than its box. All
// are enhancements; without the script the commands stay plain, selectable text and the page stays dark.

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
}
