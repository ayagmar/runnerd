---
title: Pages and keys
description: What each page of the dashboard shows, and every key of the dashboard and of runnerctl tui.
---

## Dashboard pages

The navigation has five views: Overview, Jobs (History and Records), Insights, Capacity (Sizing and Caches) and Settings (Config).

| Page | Shows |
|---|---|
| Overview | what needs attention, when anything does; slots running, jobs waiting, the p95 wait over 24 hours and failures in one row; the machine: the host's figures, then the memory budget as lanes, a lane per slot with its running job's step, memory and CPU and a row each for the mirror, what is free and what is hatched where the host or [measured admission](../../concepts/admission/#measured-admission) closes it, and the slice's memory and CPU; each waiting job with why it waits; the latest jobs; pause and resume per repository, which lasts across restarts |
| Jobs | every job, searched by job name, workflow or branch and filtered by repository, workflow, branch, outcome, time and record; a column header sorts, showing the first page of that order; the **Timeline** lens draws what each slot ran over a day or a week, the **Trends** lens charts the last 30 days. Every filter is in the link, so a view can be bookmarked |
| Records | the [records of failed jobs](../../guides/records/) with size and age, including records whose job has left the history; remove one or all |
| Sizing | per profile, memory, jobs, waits, warm starts and CPU in one table with the [advice](../../guides/sizing/), which a button opens in the config editor, unsaved; what [auto sizing](../../guides/sizing/#auto-sizing) changes next and has changed, with a link that opens the last change undone |
| Insights | a card per [insight](../../guides/insights/), warnings first; the last 24 hours; each repository's waits this week against the week before, warm and cold apart |
| Caches | each volume in one table with its size over 30 days; the build cache's 7-day hit rate and whether it serves [GitHub Actions' cache](../../guides/caches/#github-actions-cache); reseed a snapshot or empty a cache from a volume's menu |
| Config | `runnerd.toml` in an editor with line numbers that checks it as you type against `MemoryMax`, marks the line a parse error is on, and shows the changes before **Save and reload**; the doctor's checks; **Add a device** shows a QR code |

### Keys

`1` to `5` open the views, `j` and `k` move between rows and `Enter` opens one, `/` goes to the page's search, `?` lists the keys, `Esc` leaves a field, and `Ctrl K` opens a palette of pages, rows and job numbers. Keys typed into a field stay in it, and each has a button for a pointer or a finger.

## runnerctl tui

Its views, `1` to `5`, are Overview, Jobs, Insights, Capacity and Settings; pressing a view's digit again opens its next section, so `2` `2` goes from History to Records and `4` `4` from Sizing to Caches. The overview lists what needs attention, then a row per slot. `enter` opens a job: on a failed one the failure first, then its timeline, steps, memory and CPU against the caps, latest runs, and the log, folded by step and followed while the job runs.

| Key | |
|---|---|
| `1` to `5` | overview, jobs, insights, capacity, settings; again for the view's next section; `o` and `h` open the overview and history too |
| `tab`, `j` `k` or arrows, `PgDn` `PgUp` | the overview's pane; move in it, or in a log; by ten |
| `enter`, `esc` | open the selected job, or where an attention item points; back |
| `!` | what needs attention; the header's count opens it too |
| `/`, `n` `N` | jobs: search names, workflows and branches; job: search the log, the next and previous match |
| `f` `s` `t`, `l`, `x` | jobs: pick a filter, the order, the time range; the slots' timeline; clear the filters |
| `]` `[` | jobs: next and previous page |
| `enter`, `G` `g` | job: fold or open a step or group; follow the log, or go to its start |
| `c` | job: compare with an earlier run of the same job |
| `x`, `a` `A` | job: cancel its run, re-run it, re-run the run's failed jobs; each shows what GitHub would do and sends only on `y` |
| `p` `r`, `P` `U` | pause or resume the selected repository, or every one; a pause refused while a job runs asks before forcing it |
| `R` | reseed the selected slot's profile |
| `d` `D` | records: remove the selected record, or all, after `y`; caches: empty the selected volume, after `y` |
| `a` | sizing: write the selected profile's advice into the config, after `y` |
| `u` | settings: read the config and the doctor's checks again |
| click, wheel | select, click again to open; scroll. `--no-mouse` or `RUNNERCTL_NO_MOUSE=1` leaves the mouse to the terminal |
| `?`, `q` or `ctrl-c` | all keys; quit |
| `ctrl-z` | suspend; `fg` brings it back |

A daemon too old for a view's read says so in the view, `not supported by this runnerd`, rather than showing it empty.
