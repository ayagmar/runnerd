---
title: Pages and keys
description: What each page of the dashboard shows, and every key of the dashboard and of runnerctl tui.
---

## Dashboard pages

The navigation has five views: Overview, Jobs (History and Records), Insights, Capacity (Sizing and Caches) and Settings (Config).

| Page | Shows |
|---|---|
| Overview | what needs attention as one line beside the title, which opens to the list; slots running, jobs waiting, the p95 wait over 24 hours and failures as four large figures; the memory budget as lanes, a lane per slot as wide as its share of the slice, filled to what its job uses now and carrying the job's step, time, memory and CPU, then a lane each for the mirror, what is free and what is hatched where the host or [measured admission](../../concepts/admission/#measured-admission) closes it; slots that hold no memory, such as idle ones, as a row of chips under the lanes; on a phone, or with more lanes than fit, the lanes are rows over a strip of the slice; each waiting job with why it waits; the latest jobs; pause and resume per repository, which lasts across restarts; the slice's memory and CPU |
| Jobs | the last 30 days as five figures (jobs, succeeded, average run, average wait, sampled memory peak) and three small charts, then one search field for job name, workflow or branch; outcome and repository are chips, and workflow, branch, time range, record and sort sit in a **Filters** fold whose label counts how many are set; a column header sorts, showing the first page of that order, and a whole row opens its job; on a phone the chips move into the fold and a job is a row of name, outcome and repository over when, run time and peak. The **Timeline** lens draws what each slot ran over a day or a week, the **Trends** lens charts the 30 days. Every filter is in the link, so a view can be bookmarked |
| Records | the [records of failed jobs](../../guides/records/) with size and age, including records whose job has left the history; remove one or all |
| Sizing | a lane per profile on one shared scale: its cap as a pale block, the spread of its jobs' peaks solid inside it and what they need as a dashed mark; beside it the [advice](../../guides/sizing/) as a figure with its action, the next change when [auto sizing](../../guides/sizing/#auto-sizing) is on, otherwise **Review** that opens it in the config editor, unsaved; under it the percentiles, the advice in words and the profile's jobs, busy slots, waits, warm starts and CPU p99; a profile no job ran on as a chip under the lanes; auto sizing's state as one line that opens to its rules, its plan and what it has changed, with a link that opens the last change undone; then each profile's jobs in a table |
| Insights | a card per [insight](../../guides/insights/), warnings first, each leading with its kind, the number that changed and what it is measured against, then its sentence and links; the last 24 hours; disk writes as figures, a bar of who wrote to the drive and ranked bars for the workflows and jobs that wrote most, with the sentences in a fold; each repository's waits this week against the week before, warm and cold apart |
| Caches | each volume as a row with its size, its fill against its bound where it has one, its size over 30 days with the bound drawn, and one labelled button, **Reseed from slot 1** for a profile's snapshot and **Empty** for the rest, each asking first; the build cache's week as figures and whether it serves [GitHub Actions' cache](../../guides/caches/#github-actions-cache) |
| Config | the doctor's checks, problems first and the passing ones folded into one line, with how old they are and **Run again**; `runnerd.toml` in an editor with line numbers, as tall as the file, that checks it as you type against `MemoryMax`, marks the line a parse error is on, and shows the changes before **Save and reload**; **Add a device** shows a QR code |

A live page's badge, in the header, says whether what it shows is current: **Live** while it updates; **Retrying**, with the time its data is from, while runnerd does not answer and the page keeps what it had; **Offline** while the browser connects again. A page that will not update again says why: **Finished** when its job is done, **Signed out**, **Not updating** when runnerd or the dashboard refused it, **Stopped** with the dashboard, or **Offline** once the browser stops trying, all but the first with **Reload**.

### Keys

`1` to `5` open the views, `j` and `k` move between rows and `Enter` opens one, `/` goes to the page's search, `?` lists the keys, `Esc` leaves a field, and `Ctrl K` opens a palette of pages, rows and job numbers. Keys typed into a field stay in it, and each has a button for a pointer or a finger.

## runnerctl tui

Its views, `1` to `5`, are Overview, Jobs, Insights, Capacity and Settings; pressing a view's digit again opens its next section, so `2` `2` goes from History to Records and `4` `4` from Sizing to Caches. The overview opens on the budget as one strip, a segment per slot labelled under it, then a row per slot in the same order, the queue and the latest jobs; what needs attention is one line until `!` opens it. On a terminal 140 columns wide or more, Insights, Capacity and Settings show the selected row whole beside the list, and Jobs does where the list keeps every column beside it. Insights leads each row with the number that changed, and shows disk writes as figures, the drive's shares as one bar and the workflows and jobs that wrote most as ranked bars. `enter` opens a job: on a failed one the failure first, then its timeline, steps, memory and CPU against the caps, latest runs, and the log, folded by step and followed while the job runs.

| Key | |
|---|---|
| `1` to `5` | overview, jobs, insights, capacity, settings; again for the view's next section; `o` and `h` open the overview and history too |
| `tab`, `j` `k` or arrows, `PgDn` `PgUp` | the overview's pane; move in it, or in a log; by ten |
| `enter`, `esc` | open the selected job, an insight's newest run, or where an attention item points; settings: open or fold the passing checks; back |
| `!` | open or close what needs attention, back to the pane it was opened from; the header's count opens it too |
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
| `?`, `q` or `ctrl-c` | all keys, a page at a time on a short terminal, `j` `k` to turn; quit |
| `ctrl-z` | suspend; `fg` brings it back |

