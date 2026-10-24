// The landing page's behaviour: the copy buttons and the theme switch. Both are enhancements; without the script the
// commands stay plain, selectable text and the page keeps the system's theme.

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

export function setup() {
  copyButton();
  themeToggle();
}
