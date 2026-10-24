// Screenshots and TUI frames open full size in a <dialog>. A screenshot is a link to its full-size file, which
// works without JS; with JS it opens here instead, in the page's theme, the full file loading only then. A TUI frame
// becomes a button that opens its text at a readable size. The overlay closes on Esc, its close button, a click
// outside the picture, the back button (it adds a history entry) and a swipe down; focus returns to what opened it.

const dialog = document.createElement('dialog');
dialog.className = 'zoom';
dialog.setAttribute('aria-label', 'Full size');
dialog.innerHTML = '<button class="zoom-close" type="button">Close</button><div class="zoom-body"></div>';
const body = dialog.querySelector<HTMLElement>('.zoom-body')!;
const close = dialog.querySelector<HTMLButtonElement>('.zoom-close')!;
let opener: HTMLElement | null = null;
// Set from a request to close until the dialog has closed, so a double click goes back only once.
let closing = false;
let closedAt = -Infinity;

// A reload while it was open brings back its history entry without it: drop the entry's mark.
if (history.state?.zoom) history.replaceState(null, '');

function show(trigger: HTMLElement, content: HTMLElement) {
  if (!dialog.isConnected) document.body.append(dialog);
  opener = trigger;
  body.replaceChildren(content);
  body.scrollTo(0, 0);
  document.documentElement.classList.add('zoom-open');
  dialog.showModal();
  close.focus();
  history.pushState({ zoom: true }, '');
}

// Every way to close goes back past the entry show() added, and popstate closes the dialog; it stays open until
// then, so a second tap cannot reach the page under it.
function dismiss() {
  if (closing) return;
  closing = true;
  if (history.state?.zoom) history.back();
  else dialog.close();
}

// However it closed, even natively without a cancel that could be stopped, the page is let go here.
dialog.addEventListener('close', () => {
  document.documentElement.classList.remove('zoom-open');
  body.replaceChildren();
  opener?.focus({ preventScroll: true });
  opener = null;
  if (!closing && history.state?.zoom) history.back();
  closing = false;
  closedAt = performance.now();
});

// The second tap of a double tap that closed it lands on the page once it has closed: it opens nothing.
document.addEventListener(
  'click',
  (event) => {
    if (performance.now() - closedAt > 400) return;
    event.preventDefault();
    event.stopPropagation();
  },
  true,
);

function openShot(link: HTMLAnchorElement) {
  const shown = link.querySelector('img')!;
  const light = document.documentElement.dataset.theme === 'light';
  const full = (light && link.dataset.zoomLight) || link.href;
  const img = document.createElement('img');
  img.alt = shown.alt;
  let width = Number(link.dataset.zoomWidth);
  let height = Number(link.dataset.zoomHeight);
  // What the page shows is already loaded: it stands in until the full-size file arrives. On a phone, a phone crop
  // is shown as it is: its text reads better than the desktop capture fitted to the screen.
  const same = Math.abs(shown.naturalWidth / shown.naturalHeight - width / height) < 0.01;
  const phoneCrop = !same && shown.complete && matchMedia('(max-width: 600px)').matches;
  if (phoneCrop) {
    width = shown.naturalWidth;
    height = shown.naturalHeight;
  }
  img.width = width;
  img.height = height;
  if (phoneCrop) img.src = shown.currentSrc;
  else if (same && shown.complete) {
    img.src = shown.currentSrc;
    const loader = new Image();
    loader.onload = () => (img.src = full);
    loader.src = full;
  } else img.src = full;
  // Fitted to the screen first; a click shows it at its own size when that is larger, and back.
  img.addEventListener('click', () => {
    const fitted = img.getBoundingClientRect().width < width - 1;
    if (fitted || img.classList.contains('actual')) img.classList.toggle('actual');
  });
  show(link, img);
}

function openTui(pre: HTMLElement) {
  const copy = pre.cloneNode(true) as HTMLElement;
  copy.removeAttribute('tabindex');
  copy.removeAttribute('aria-haspopup');
  copy.setAttribute('role', 'img');
  copy.classList.add('tui-zoomed');
  show(pre, copy);
}

for (const link of document.querySelectorAll<HTMLAnchorElement>('a[data-zoom]')) {
  link.setAttribute('aria-haspopup', 'dialog');
  link.addEventListener('click', (event) => {
    if (event.button !== 0 || event.metaKey || event.ctrlKey || event.shiftKey || event.altKey) return;
    event.preventDefault();
    openShot(link);
  });
}

for (const pre of document.querySelectorAll<HTMLElement>('pre.tui')) {
  pre.setAttribute('role', 'button');
  pre.setAttribute('aria-haspopup', 'dialog');
  pre.addEventListener('click', () => {
    // Selecting its text is not a request to enlarge it.
    if (getSelection()?.isCollapsed !== false) openTui(pre);
  });
  pre.addEventListener('keydown', (event) => {
    if (event.key !== 'Enter' && event.key !== ' ') return;
    event.preventDefault();
    openTui(pre);
  });
}

dialog.addEventListener('cancel', (event) => {
  event.preventDefault();
  dismiss();
});
close.addEventListener('click', dismiss);
// Anything but the picture and the close button is backdrop.
dialog.addEventListener('click', (event) => {
  if (event.target === dialog || event.target === body) dismiss();
});
addEventListener('popstate', () => {
  if (dialog.open) dialog.close();
});

// A swipe down closes it, unless the picture is pinch-zoomed or the swipe scrolled it.
let start: { x: number; y: number; top: number } | null = null;
dialog.addEventListener(
  'touchstart',
  (event) => {
    const touch = event.touches[0];
    start = event.touches.length === 1 && touch ? { x: touch.clientX, y: touch.clientY, top: body.scrollTop } : null;
  },
  { passive: true },
);
dialog.addEventListener('touchend', (event) => {
  const touch = event.changedTouches[0];
  const zoomed = (visualViewport?.scale ?? 1) > 1.01;
  if (!start || !touch || zoomed || start.top > 0 || body.scrollTop > 0) return;
  const dy = touch.clientY - start.y;
  if (dy > 80 && Math.abs(touch.clientX - start.x) < dy / 2) dismiss();
  start = null;
});
