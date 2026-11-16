# 0146. The dashboard paints the doctor's last run while it is no older than the refresh

Status: **Accepted** (2026-11-16). Amends [0132](0132-one-attention-list-core-words.md): how old a run may be to
answer `GET /v1/doctor`, and [0145](0145-the-dashboard-leads-with-the-lanes.md): the doctor's box on Settings. Adds
`DoctorQuery` to the API. No config change.

## Context

Under 0132 a run of the doctor answers a request while it is at most `Timing::fresh` (a minute) old; past that the
request asks for a new run and waits for it. The worker runs the checks every `Timing::refresh` (10 minutes), so most
visits to Settings found a run too old, waited for a new one, and painted the checks 0.7 to 2 s after the page. Since
0145 puts the doctor above the editor, their arrival moved the editor by about 510px on a phone (layout shift 0.37).
The box reserved from the last visit missed every first visit, a rotated phone and a resized window, and kept the
height of an error when the last visit got one.

## Decision

- `GET /v1/doctor?recent=true` takes the last run as it stands when it is of the config running and at most
  `Timing::refresh` old. Without `recent` nothing changes: a run over a minute old asks for a new one, which is what
  `runnerctl doctor`, the TUI and the dashboard's "Run again" want. A daemon before this ignores the parameter.
- Settings asks with `recent` and waits 50 ms. A run that stands is in the first response, its age beside the title
  as core words it (`DoctorState::checked`: "Checked 4m 00s ago") with a "Run again" button.
- An older run, none, or a failed one keeps the pending region, which asks without `recent`. Its box is the height
  the checks took on the last visit at that width and layout (one column or two), recorded only from checks that
  stand.
- How the checks are laid out is core's: `Doctor::summary` splits those that need the reader from the rest and counts
  the rest by label, and `Doctor::CHECKING` is what a client says while it waits.

## Consequences

- A visit to Settings shows checks up to 10 minutes old without saying it less plainly than their age; a run that
  fails is never shown as standing, and one older than `Timing::stale` is marked stale as before.
- A first visit no longer moves the page in the common case. A daemon whose worker could not run in the last 10
  minutes still shows the pending box once.
- `crates/web/tests/browser.mjs` checks the first visit's layout shift in CI, with the attention strip and the
  editor's caret, which no Rust test can see.
