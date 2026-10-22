// The light/dark switch: the choice is kept in localStorage; without one, the system's preference applies.
const root = document.documentElement;
const stored = localStorage.getItem('runnerd-theme');
if (stored === 'light' || stored === 'dark') root.dataset.theme = stored;

document.addEventListener('click', (event) => {
  if (!(event.target instanceof Element) || !event.target.closest('[data-theme-toggle]')) return;
  const dark = root.dataset.theme
    ? root.dataset.theme === 'dark'
    : matchMedia('(prefers-color-scheme: dark)').matches;
  root.dataset.theme = dark ? 'light' : 'dark';
  localStorage.setItem('runnerd-theme', root.dataset.theme);
});
