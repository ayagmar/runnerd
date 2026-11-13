// The dashboard's keys, as runnerctl tui has them where they make sense: 1-5 open the views, j and k move between
// rows, Enter opens one, / goes to the page's search, ? shows the keys, Esc closes what is open, and Ctrl+K (⌘K)
// opens the command palette. A key typed into a field is the field's; every key has a pointer and touch path too.

const typing = (target) =>
  target instanceof Element &&
  target.closest(
    'input:not([type="checkbox"], [type="radio"], [type="button"], [type="submit"]), textarea, select, [contenteditable]:not([contenteditable="false"])',
  ) !== null;

const shown = (element) => element.getClientRects().length > 0;

// The rows the page lists, in order: jobs, queued jobs, slots, records, attention items.
function rows() {
  return [...document.querySelectorAll('[data-row]')].filter(shown);
}

function focusRow(row) {
  const target = row.matches('a, button, [tabindex]') ? row : row.querySelector('a, button, [tabindex]') ?? row;
  if (!target.hasAttribute('tabindex') && !target.matches('a, button')) target.setAttribute('tabindex', '-1');
  target.focus();
  target.scrollIntoView({ block: 'nearest' });
}

function moveRow(step) {
  const list = rows();
  if (list.length === 0) return;
  const at = list.findIndex((row) => row.contains(document.activeElement));
  const next = at < 0 ? (step > 0 ? 0 : list.length - 1) : Math.min(list.length - 1, Math.max(0, at + step));
  focusRow(list[next]);
}

function open(dialog) {
  if (!(dialog instanceof HTMLDialogElement) || dialog.open) return;
  for (const other of document.querySelectorAll('dialog[open]')) other.close();
  dialog.showModal();
}

// The palette: the views and sections, the theme, the page's rows by their text, and a job by its number.
function commands() {
  const items = [];
  for (const link of document.querySelectorAll('nav[aria-label="Dashboard"] a, nav.sections a')) {
    items.push({ label: link.textContent.trim(), hint: 'Go to', run: () => link.click() });
  }
  const theme = document.querySelector('[data-theme-toggle]');
  if (theme) items.push({ label: theme.getAttribute('aria-label'), hint: 'Theme', run: () => theme.click() });
  for (const row of rows()) {
    const link = row.matches('a') ? row : row.querySelector('a[href]');
    if (link) items.push({ label: link.textContent.replace(/\s+/g, ' ').trim(), hint: 'Open', run: () => link.click() });
  }
  return items;
}

function palette() {
  const dialog = document.getElementById('palette');
  if (!(dialog instanceof HTMLDialogElement)) return;
  const input = dialog.querySelector('input');
  const list = dialog.querySelector('ul');
  const all = commands();
  let matches = all;
  let at = 0;
  const draw = () => {
    const query = input.value.trim().toLowerCase();
    matches = all.filter((item) => item.label.toLowerCase().includes(query));
    // A job's number fits a u64: at most 19 digits are always one.
    if (/^\d{1,19}$/.test(query)) {
      const id = query;
      matches.unshift({ label: id, hint: 'Open job', run: () => location.assign(`/jobs/${id}`) });
    }
    at = Math.min(at, Math.max(0, matches.length - 1));
    list.replaceChildren(
      ...matches.slice(0, 50).map((item, index) => {
        const li = document.createElement('li');
        li.setAttribute('role', 'option');
        li.id = `palette-${index}`;
        li.setAttribute('aria-selected', String(index === at));
        const hint = document.createElement('span');
        hint.className = 'hint';
        hint.textContent = item.hint;
        li.append(hint, ` ${item.label}`);
        li.addEventListener('click', () => run(index));
        return li;
      }),
    );
    input.setAttribute('aria-activedescendant', matches.length ? `palette-${at}` : '');
  };
  const run = (index) => {
    const item = matches[index];
    if (!item) return;
    dialog.close();
    item.run();
  };
  input.value = '';
  input.oninput = () => {
    at = 0;
    draw();
  };
  input.onkeydown = (event) => {
    if (event.key === 'ArrowDown' || event.key === 'ArrowUp') {
      at = Math.min(matches.length - 1, Math.max(0, at + (event.key === 'ArrowDown' ? 1 : -1)));
      draw();
      document.getElementById(`palette-${at}`)?.scrollIntoView({ block: 'nearest' });
      event.preventDefault();
    } else if (event.key === 'Enter') {
      run(at);
      event.preventDefault();
    }
  };
  draw();
  open(dialog);
  input.focus();
}

// A preview that asked for a choice, such as a cancel's or the config's diff: Esc or its quiet button puts back the
// empty place it filled, and focus goes back to the button that asked for it.
function dismiss(panel) {
  const back = document.querySelector(panel.dataset.dismissTo ?? '');
  const empty = document.createElement('div');
  empty.id = panel.id;
  panel.replaceWith(empty);
  if (back instanceof HTMLElement) back.focus();
}

document.addEventListener('keydown', (event) => {
  const panel = event.target instanceof Element ? event.target.closest('[data-dismiss-to]') : null;
  if (event.key === 'Escape' && panel && !typing(event.target)) {
    event.preventDefault();
    dismiss(panel);
    return;
  }
  if ((event.ctrlKey || event.metaKey) && !event.altKey && event.key.toLowerCase() === 'k') {
    event.preventDefault();
    palette();
    return;
  }
  // Esc leaves a field, unless the field is in a dialog, which Esc closes.
  if (event.key === 'Escape' && typing(event.target) && !event.target.closest('dialog')) {
    event.target.blur();
    return;
  }
  if (event.ctrlKey || event.metaKey || event.altKey || event.defaultPrevented || typing(event.target)) return;
  if (document.querySelector('dialog[open]')) return;
  const view = document.querySelector(`nav[aria-label="Dashboard"] a[data-key="${CSS.escape(event.key)}"]`);
  if (view) {
    event.preventDefault();
    view.click();
  } else if (event.key === 'j' || event.key === 'k') {
    event.preventDefault();
    moveRow(event.key === 'j' ? 1 : -1);
  } else if (event.key === '/') {
    const search = [...document.querySelectorAll('[data-search]')].find(shown);
    if (search) {
      event.preventDefault();
      search.focus();
      search.select?.();
    }
  } else if (event.key === '?') {
    event.preventDefault();
    open(document.getElementById('keys'));
  } else if (event.key === 'Escape' && document.activeElement instanceof HTMLElement) {
    document.activeElement.blur();
  }
});

// The buttons that open the help and the palette, for a pointer or a finger.
document.addEventListener('click', (event) => {
  if (!(event.target instanceof Element)) return;
  if (event.target.closest('[data-open-keys]')) open(document.getElementById('keys'));
  if (event.target.closest('[data-open-palette]')) palette();
  const quiet = event.target.closest('[data-dismiss]');
  if (quiet) {
    const panel = quiet.closest('[data-dismiss-to]');
    if (panel) dismiss(panel);
  }
  const close = event.target.closest('[data-close]');
  if (close) close.closest('dialog')?.close();
  // A click on the backdrop, outside the dialog's box, closes it.
  if (event.target instanceof HTMLDialogElement && event.target.open) {
    const box = event.target.getBoundingClientRect();
    const inside =
      event.clientX >= box.left && event.clientX <= box.right && event.clientY >= box.top && event.clientY <= box.bottom;
    if (!inside) event.target.close();
  }
});

// The log: a line linked to opens the folds around it; otherwise the failed step's fold scrolls into the log's box.
// Scrolling is inside the box only, so the page stays on the failure summary above it.
function scrollInto(box, target) {
  box.scrollTop += target.getBoundingClientRect().top - box.getBoundingClientRect().top;
}

function openLine() {
  const match = /^#L(\d+)$/.exec(location.hash);
  const line = match && document.getElementById(`L${match[1]}`);
  if (line) {
    for (let fold = line.closest('details'); fold; fold = fold.parentElement?.closest('details')) fold.open = true;
    line.scrollIntoView({ block: 'center' });
    return;
  }
  const box = document.getElementById('log-view');
  const here = box?.querySelector('[data-scroll-here]');
  if (box && here) scrollInto(box, here);
}

addEventListener('hashchange', openLine);
if (document.readyState === 'loading') document.addEventListener('DOMContentLoaded', openLine);
else openLine();

// Follow while running: the live tail stays at its end as lines arrive while it is at its end. Scrolled up to read
// an earlier line, it stays there until scrolled back to the end; unticked, it never moves.
for (const toggle of document.querySelectorAll('[data-follow]')) {
  const id = toggle.dataset.follow;
  const atEnd = (box) => box.scrollHeight - box.clientHeight - box.scrollTop < 8;
  let following = true;
  document.addEventListener(
    'scroll',
    (event) => {
      if (event.target instanceof Element && event.target.id === id) following = atEnd(event.target);
    },
    true,
  );
  const follow = () => {
    const box = document.getElementById(id);
    if (toggle.checked && following && box) box.scrollTop = box.scrollHeight;
  };
  new MutationObserver(follow).observe(document.body, { childList: true, subtree: true });
  toggle.addEventListener('change', () => {
    following = true;
    follow();
  });
  follow();
}

// The config editor's line numbers: as many as its lines, scrolled with it, the line a parse error is on marked.
for (const editor of document.querySelectorAll('textarea[data-gutter]')) {
  const gutter = document.getElementById(editor.dataset.gutter);
  if (!gutter) continue;
  const draw = () => {
    const count = editor.value.split('\n').length;
    const marked = Number(document.querySelector('[data-error-line]')?.dataset.errorLine ?? 0);
    gutter.replaceChildren(
      ...Array.from({ length: count }, (_, index) => {
        const line = document.createElement('span');
        line.textContent = `${index + 1}\n`;
        if (index + 1 === marked) line.className = 'error';
        return line;
      }),
    );
    gutter.scrollTop = editor.scrollTop;
  };
  editor.addEventListener('input', draw);
  editor.addEventListener('scroll', () => {
    gutter.scrollTop = editor.scrollTop;
  });
  const result = document.getElementById('config-result')?.parentElement;
  if (result) new MutationObserver(draw).observe(result, { childList: true, subtree: true });
  draw();
}

// A filter form sends only the filters set, so its link reads as what it shows: `/history?outcome=failure`.
document.addEventListener('submit', (event) => {
  if (!(event.target instanceof HTMLFormElement) || event.target.method !== 'get') return;
  for (const field of event.target.elements) {
    if ((field instanceof HTMLInputElement || field instanceof HTMLSelectElement) && field.name && field.value === '') {
      field.disabled = true;
    }
  }
});
// Back to a page whose fields were disabled for the send: they are fields again.
addEventListener('pageshow', () => {
  for (const field of document.querySelectorAll('form[method="get"] :disabled')) field.disabled = false;
});

// A choice sent from a preview, such as "Cancel run", empties the preview when the answer lands, which leaves focus
// on nothing: once, it goes back to the button that asked.
let returnTo = null;
document.addEventListener('click', (event) => {
  const panel = event.target instanceof Element ? event.target.closest('[data-dismiss-to]') : null;
  if (panel && event.target.closest('button:not([data-dismiss])')) returnTo = panel.dataset.dismissTo;
});
new MutationObserver(() => {
  if (!returnTo || document.querySelector(`[data-dismiss-to="${CSS.escape(returnTo)}"]`)) return;
  const back = document.querySelector(returnTo);
  returnTo = null;
  if (back instanceof HTMLElement && (document.activeElement === document.body || !document.activeElement)) back.focus();
}).observe(document.body, { childList: true, subtree: true });

// A log line cut to four rows opens in full on a click, and closes on the next; selecting text in it does neither.
document.addEventListener('click', (event) => {
  const text = event.target instanceof Element ? event.target.closest('.log-lines .text') : null;
  if (!text || !getSelection()?.isCollapsed) return;
  const line = text.closest('li');
  if (line && (line.classList.contains('open') || text.scrollHeight > text.clientHeight + 1)) line.classList.toggle('open');
});
