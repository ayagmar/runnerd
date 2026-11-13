---
title: Jobs API
description: The routes for jobs, their logs, the slots' timeline, comparisons, and cancel and re-run.
---

The same socket and units as the [HTTP API](../api/).

### `GET /v1/jobs?repository=&state=&conclusion=&workflow=&name=&branch=&q=&record=&since=&until=&before=&limit=`

Jobs, newest first; `next` is the next page's `before`. `q` finds 1 to 100 bytes as written in a name, workflow or branch; `record` is `kept`, `removed`, `skipped` or `any`; each job's `record_mark` says what became of its record. At most 500 jobs and 1 MiB a page.

### `GET /v1/jobs?sort=&order=&limit=` and the filters above

Sorted by `queued`, `wait`, `run` or `memory`, `desc` or `asc`, jobs without the figure yet last. Newest first pages by `before`; any other order is one page, its first `limit` jobs, with `total`, how many the filters admit. Long names are cut, and `shortened` lists the jobs cut.

### `POST /v1/jobs/{id}/cancel?force=&confirm=`, `.../rerun?force=&confirm=&failed=`

Without `force=true` and a `confirm` this daemon issued, `409` with a preview of what GitHub would do and a `confirm` valid once for 5 minutes, and nothing sent; with them, GitHub's answer: taken, already finished, refused, or unknown. `failed=true` re-runs the run's failed jobs. `403` while [`[github] job_actions`](../configuration-more/#github) is off ([Cancel and re-run](../../guides/cancel-rerun/)).

### `GET /v1/jobs/{id}/compare?with=`

The job beside the latest earlier success of the same job on its branch, or the job `with` names: whole-job figures and steps paired by name; `steps_cut` when a run has more steps than a comparison reads (500).

### `GET /v1/timeline?since=&until=`

What each slot ran in a window of at most 7 days (the last 24 hours by default), when runnerd was watching, and the jobs no runner here took.

### `GET /v1/jobs/{id}`, `.../samples`, `.../logs?from=&follow=`

A job with timeline, steps and metrics; its samples; its log, open until the job is final with `follow=true`.

### `GET /v1/jobs/{id}/log/outline`, `.../log/lines?from=&limit=&follow=`, `.../log/search?q=&from=&limit=&context=`

Its log's groups, marks, each step's lines and a failed job's summary; its lines styled, one JSON object per line; a page of a literal search of the lines' messages (not their timestamps or `##[…]` commands), `next` where the next one starts.

