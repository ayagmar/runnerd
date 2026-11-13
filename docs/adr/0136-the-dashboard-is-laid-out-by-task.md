# 0136. The dashboard is laid out by task

Status: **Accepted** (2026-11-12). Restructures `crates/web`'s pages; adds `POST /jobs/{id}/cancel`,
`POST /jobs/{id}/rerun` and `POST /config/diff` to the dashboard, and `assets/keys.js`. Adds to core
`Budget::host_figures`, `JobAction::for_job`, `StyledLine::omitted_words`, `Records::history_gone`, `Feature`,
`Config::parse_error` and `SlotTimeline::cut_words`.

## Context

ADRs 0132 to 0135 gave core an attention list, a log outline, a searched and sorted history, a timeline, a comparison
and job actions. The dashboard drew none of them, and its pages had grown by accretion: the overview said the
budget three times (a sentence, a bar and a readout) and listed slots as cards beside the bar that already showed
them; the host was a sentence; the history's filters were rows of links; a failed job showed its log before what
failed; the config editor saved without showing what would change.

## Decision

**Five views, each page titled.** Overview, Jobs, Insights, Capacity and Settings, as core's `View` names them, each
page with a visible `h1` and its sections as tabs. The look stays: flat blocks, no radius, the profile colours,
Libre Franklin.

**The overview says each fact once.** Attention first, and nothing when it is empty. Health is one row of figures.
The machine is one region: the budget's bar, the host as figures (`Budget::host_figures`), one lane per slot whose
block is its share of the budget, a running lane with its job, step, memory and CPU, a warm or idle lane one row,
and the slice's two charts. Then waiting jobs, the latest finished jobs and the repositories.

**Jobs is one page with lenses.** The list, the timeline and the trends are `?lens=` of one page whose filters are a
plain GET form, so a filtered view is a link. Column headers sort; a sort is one page that says how many the filters
admit (`JobList::shown_words`); only newest first pages. The records section lists kept records whose job left the
history, without a link to a page that would 404.

**A job page opens on what failed.** Core's failure headline, exit code, last lines and the shell command come first.
The log is folded by step and group with the failed step open and scrolled to inside its box, lines coloured from
core's runs, each line linkable, searched by message only. A running job's tail follows its end until scrolled.

**Writes are previewed.** Cancel and re-run are offered only while `Status::job_actions` offers them. The first post
returns the daemon's preview; its button carries that preview's confirmation token, and only a post with it sends.
A config save is preceded by a diff of the file against the edit. Every write goes through `auth::require_session`,
so a foreign `Origin` is 403 and a missing session 401.

**Keys are an addition.** 1 to 5 open the views, j and k move between rows, Enter opens one, `/` focuses the page's
search, `?` lists the keys, Esc leaves a field or closes a dialog, Ctrl+K opens a palette. A key typed into a field
is the field's. Each has a button or link, so pointer and touch reach the same.

**Older daemons.** A route an older runnerd lacks (`ClientError::is_unsupported`) reads as `Feature::unsupported`'s
sentence in its place, never as an empty list.

## Consequences

- The dashboard's pages are tested against `crates/fixtures` for each new region (attention, host figures, log,
  lenses, actions); the demo's fake answers as an older daemon when `Fixtures::older` is set.
- A diff of the config is computed in the dashboard over the two texts the browser already has; it is layout, not
  a fact of runnerd's.
- The site's dashboard screenshots predate this and need recapturing.
