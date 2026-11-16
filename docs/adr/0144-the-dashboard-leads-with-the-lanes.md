# 0144. The dashboard leads with the lanes

Status: **Accepted** (2026-11-16). Amends the overview and the config page of
[0136](0136-the-dashboard-is-laid-out-by-task.md). Adds `FreePart::name` to core, speculation rules to the dashboard's
pages and `'inline-speculation-rules'` to its content security policy.

## Context

The owner found the dashboard of 0136 worse than the one before it. The budget lanes shrank to a 6rem strip beside
rows of text, the four health figures to a corner of the title row, and attention took the top of the page as four
cards of wrapped sentences, cut mid-sentence on a phone. On a phone the config editor's line numbers ran to the
file's last line beside a textarea a third as tall that scrolled inside the page, and the doctor's twenty passing
checks stood at full height. Every page change blanked the screen, and the doctor's checks pushed the page down when
they arrived.

## Decision

**The overview leads with the lanes, and the lanes are the slots.** Attention is one line beside the title: its count
in the worst item's tone, that item's title, core's count of the rest; it opens to every item. An item about a
repository is also shown on the repository's row. Then the four health figures at a size that reads across a desk.
Then the budget as one bar whose blocks are the slots, each as wide as its share of the slice and filled from the
bottom to what its job uses now, carrying as much as its width has room for: job, step, time, memory and CPU with
their sparklines, then less, down to its name on its side. The mirror, seeds, shells, what is free and what is closed
are blocks too, each a figure over its short name (`HeldPart::name`, `FreePart::name`, `ClosedKind::name`), core's
sentence in its title. A slot that holds none of the budget, as an idle one under measured admission, is no sliver in the bar: it is
a chip in one row under it, its profile's swatch, its name and core's state word, so an idle machine reads as one
wide free lane over its idle slots. A container narrower than 44rem, or than the bar's lanes need at their 1.75rem
floor, stacks the same lanes as rows, each over a strip of the slice with its block where it sits. Then the queue, the latest jobs, the repositories and the slice's charts.

**Settings puts problems first.** The doctor comes before the editor; a check that fails or warns stands with its
fix; the rest fold into one line that counts them by core's label and names them. The page waits 50 ms for the
doctor, which a recent run answers within (0145); otherwise the region asks once the page shows, and keeps the box
the checks took on the last visit at that width and layout. The editor has a row per line
and one to spare: the gutter and the text are one height, the page is the only thing that scrolls up and down, a
long line scrolls sideways in the text alone, and the buttons ride the bottom of the screen while the editor is in
view. The check's budget is drawn as lanes.

**Moving between pages keeps the page until the next one paints.** `@view-transition` crossfades in under 150 ms, the
shell stays still and the view's mark slides to the view opened. Speculation rules prefetch a linked page as a pointer
presses its link or a finger lands on it (`conservative`), never a login link or a live stream. Resting the pointer
(`moderate`) was tried first: Chromium then served that copy for a click up to 5 minutes later, so Jobs or Settings
opened on a page minutes old with nothing to say so, and passing over Settings started a doctor run. Across 15
navigations of the demo in headless Chromium, the median from click to first paint is 68 to 88 ms whichever way, or
with no rules, since the demo's server answers in under 20 ms; the gain is on a slower daemon.
Prerendering was not adopted: it never activated in the test browser, and a prerendered live page would open a
stream to the daemon for a page that may never be shown.

## Consequences

- `check-boundaries.sh` and the parity tests are unchanged; the attention parity test now reads the one-line form.
- A browser without view transitions or speculation rules navigates as before.
- The doctor's box: see [0145](0145-the-dashboard-paints-the-doctors-last-run.md), which puts a recent run in the
  first response.
