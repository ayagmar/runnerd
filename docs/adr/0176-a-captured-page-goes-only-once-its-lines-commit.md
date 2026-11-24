# 0176. A captured page goes only once its lines commit

Status: **Accepted** (2026-11-24). Amends [0018](0018-job-identity-and-logs-from-inside-the-runner.md) and
[0169](0169-the-history-commits-telemetry-together-and-leases-runner-names.md): the capture script reads acks on its
stdin and takes a third argument, the bytes of removed pages it may keep. Adds `Store::append_log`, `LogSegment`,
`Appended`, `DockerApi::exec_fed` and `capture::GAP`. No wire, schema or config change.

## Context

The capture script hard-links each page the runner writes under `_diag/pages`, prints what it has not printed, and
dropped its link as soon as the runner removed the page. The capture task took `Store::log().await` as progress: that
await only reserves a share of the backlog and queues the lines for the writer, which gathers telemetry for up to a
second before it commits ([0169](0169-the-history-commits-telemetry-together-and-leases-runner-names.md)). So:

- A write that failed after the page went lost its lines: a capture started again had no page to read them from.
- The task kept an in-memory resume position that ran ahead of the commit. A capture started again from the later of
  that and the stored position skipped the failed lines.
- A later segment's write could succeed after an earlier one failed and move the stored position past the hole, so
  even a restart resumed past it.
- At the script's end or at `log_max` the capture ended `Done` with its last lines still in the window. A failed
  commit then left the log short, with nothing to read it again.

## Decision

- **A segment is stored only where the log ends.** The capture sends each run of lines as a `LogSegment` with where
  it starts (`from`) and where the capture resumes after it (`to`). The writer stores it only if the job's stored
  position is exactly `from`, then moves the position to `to`, in the segment's savepoint. Otherwise nothing of it is
  stored. A segment is never split. `Store::log`, which appends wherever the log ends, stays for callers that check
  nothing.
- **Sent is not stored.** `append_log` still waits for its share of the backlog, then returns a receiver. The writer
  answers once the transaction is done: `Stored`; `Full` when the log reached `log_max` or is packed, which is a cut,
  not a loss; or `Refused` when the segment did not start where the log ends, its write failed, or its commit failed
  and may have kept it. Segments of every runner still commit together in the one-second window: the capture never
  waits for an answer before it sends the next segment.
- **One owner per capture.** `LogCursor`, held by the capture task, keeps the committed position and the segments
  sent and not answered, in order. The task ends only when every segment it sent is answered: `Done` if each was
  stored or the log is full, `Broken` on the first refusal or a Docker break. Nothing of it is in `Engine`, and
  `Watches` no longer keeps a resume position.
- **A capture starts from the stored position.** The task reads the stored position of its runner's job before it
  runs the script, after every write of the capture before it was answered. A failed read is a break, retried with the
  break backoff, never a start from page 1.
- **The script drops a page once it is committed.** The capture writes `<page>` on the script's stdin
  (`DockerApi::exec_fed`) once the committed position reaches that page: every page before it is stored. The script
  drops those links and links none of them again. Until then it keeps a removed page, ends it once, and prints it
  again to a capture started again from inside it. Its stdin's end means the daemon's end is gone, and the script
  exits rather than linking pages nobody will ack.
- **What is kept is bounded.** The script links no new page while the removed pages it keeps hold `log_max` bytes or
  more, as when the history refuses every write. Pages still in `_diag/pages` cost nothing more than the runner's
  own.
- **A gap is named.** The parser starts at the capture's start position. When the script's next bytes start past
  where the last ones ended, the pending part of the cut line is stored as a line, followed by `capture::GAP`, a line
  saying lines are missing there. The capture never stitches the two sides together silently.

## Consequences

- The durability boundary: a line the script printed is in the history once its segment commits, or is read again
  from the page the container keeps, through daemon restarts, failed writes and Docker breaks. Outside it: a container
  that stopped or was removed before the lines committed, as no script runs there; pages the runner removed before any
  script linked them, as while no daemon ran; pages past the bound; and a power loss of telemetry committed since the
  last sync. The first two leave the log short; the others are named as a gap when a later page is read.
- A capture's `Done` comes up to about a second later than before, after the commit; a release's 10 s drain covers
  it.
- A refused segment ends the capture, and every segment queued after it is refused too. The next capture starts
  from the stored position after the break backoff: at once after the first break.
- Removed pages stay in the container until their acks, normally within a second or two. A history that refuses for
  long keeps up to `log_max` of them per running job.
