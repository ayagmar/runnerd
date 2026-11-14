# 0140. A job's workflow is its run's workflow, not the run's name

Status: **Accepted** (2026-11-14).

## Context

GitHub's job payload has a `workflow_name`, and runnerd stored it as the job's workflow. GitHub fills it with the run's
name, which is the workflow's `name:` only when the workflow sets no `run-name:`. A workflow with
`run-name: deploy ${{ github.ref_name }}` gets a new "workflow" on every run, and so does every Dependabot update
("npm_and_yarn in / for undici - Update #1649646180"). The 0.1.1 end-to-end run found it: compare gave a failed job
no baseline because its earlier success had another run name.

Everything that tells jobs apart keys on the workflow: compare's baseline, the history's workflow filter, admission's
and sizing's per-job predictions, warm runners' bursts per workflow, insights ("slower", "fails then passes"), the
slot a job prefers, savings' Dependabot and Pages exclusion, and the job's title in every client.

## Decision

A job's workflow is the workflow its run is of, named as GitHub lists the repository's workflows
(`GET /repos/{owner}/{repo}/actions/workflows`). `github::Github` resolves it, so the rest of runnerd keeps reading
`QueuedJob::workflow` and `JobDetails::workflow_name` and gets the real name.

- A run's `workflow_id` comes from the runs listing a poll already reads; a run first seen through a job read costs one
  `GET .../actions/runs/{id}`, once. `run_trigger`'s read of the run notes it too. A run GitHub no longer has gets no
  workflow.
- The workflows' names are read when a run names a workflow the last read did not list, and again once that read is
  an hour old. The list is kept for a conditional request, so an unchanged list costs a 304. A workflow the last read
  did not list keeps GitHub's name and is looked for again only once that read is an hour old.
- One read of a run or of the list is in flight at a time, so job reads made together ask once. After a read fails,
  nothing more is read for that repository for a minute.
- What is learnt stays in memory (`github::workflows`): the last 2,048 runs noted per repository, in the order they
  were noted, since a re-run keeps its old run's ID. A restart learns it again.
- A job read never fails for its workflow's name. A job whose workflow cannot be read keeps GitHub's name, and its run
  is owed the workflow's. Each tick the engine asks the client for the runs owed a name that can now be read
  (`GithubApi::named_runs`), and the history renames that run's jobs. A restart forgets what is owed: those jobs keep
  GitHub's name. A poll of queued jobs that cannot read the workflow lists the job under GitHub's name; its details
  correct it.
- The key stays the workflow's name, not its ID or path: names are what every client shows, and the history's rows
  for workflows without `run-name` already hold them, so the history stays continuous with no migration.

## Consequences

Runs of one workflow are one workflow whatever their `run-name`. Renaming a workflow's `name:` starts a new one within
the hour, as the next read of the list finds the new name; jobs recorded before keep the old one. Two workflow files
with the same `name:` count as one workflow.

Jobs stored before this change from a workflow with `run-name` keep the run's name: the history has no workflow ID to
correct them with. They match no later job, and age out with the history.

The run's own name is no longer stored or shown. No client showed it apart from the workflow.

The wire format and the history's schema are unchanged.
