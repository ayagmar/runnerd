# 0176. A captured page goes only once its lines commit

Status: **Accepted** (2026-11-24). Amends [0018](0018-job-identity-and-logs-from-inside-the-runner.md) and
[0169](0169-the-history-commits-telemetry-together-and-leases-runner-names.md): the capture script reads acks on its
stdin and takes a third argument, the bytes of removed pages it may keep. Adds `Store::append_log`, `LogSegment`,
`Appended`, `store::LOG_GAP`, `Captured::Gap` and `DockerApi::exec_fed`. No wire, schema or config change.

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
  position is exactly `from` and `to` is past it, then moves the position to `to`, in the segment's savepoint.
  Otherwise nothing of it is stored: the position never goes back, even if the script read pages out of order. A segment is never split. `Store::log`, which appends wherever the log ends, stays for callers that check
  nothing.
- **Sent is not stored.** `append_log` still waits for its share of the backlog, then returns a receiver. The writer
  answers once the transaction is done: `Stored`; `Full` when the log reached `log_max` or is packed, which is a cut,
  not a loss; or `Refused` when the segment did not start where the log ends or end past it, its write failed, or its
  commit failed and may have kept it. Segments of every runner still commit together in the one-second window: the capture never
  waits for an answer before it sends the next segment.
- **One owner per capture.** `LogCursor`, held by the capture task, keeps the committed position and the segments
  sent and not answered, in order. The task ends only when every segment it sent is answered: `Done` if each was
  stored or the log is full, `Broken` on the first refusal, a Docker break, or an ack the script did not take within
  5 s. When the script's output ends, the task inspects the container: done once it stopped or went, broken while it
  runs, as then only the script ended. Nothing of it is in `Engine`, and
  `Watches` no longer keeps a resume position.
- **A capture starts from the stored position.** The engine reads the stored position of the runner's job just
  before it starts the capture's task, once every write of the capture before was answered. A failed read is a break, retried with the
  break backoff, never a start from page 1.
- **The script drops a page once it is committed.** The capture writes `<page>` on the script's stdin
  (`DockerApi::exec_fed`) once the committed position reaches that page: every page before it is stored. The script
  drops those links and links none of them again. Until then it keeps a removed page, ends it once, and prints it
  again to a capture started again from inside it. Its stdin's end means the daemon's end is gone, and the script
  exits rather than linking pages nobody will ack.
- **Pages are read by number.** The script lists the job's pages by number, so page 10 comes after page 9, and links
  no page earlier than one it already printed. A page is whole, and ends, once the runner removed it or a later page
  exists: the runner closes a page before it opens the next, and may upload it after. So a rollover is not a gap, and
  a last line with no line end is stored when its page ends.
- **What is kept is bounded.** Each pass first drops the committed links and counts every link it keeps, those of
  earlier runs of the script too, at its size. It then links new pages in order, each counted as it is linked, only
  while that count is under `log_max`, as when the history refuses every write. A page linked under the bound may grow
  past it by what the runner writes to it after.
- **A gap is named, and is not a cut.** The parser starts at the capture's start position. When the script's next
  bytes start past where the last ones ended, it reports a `Captured::Gap`: the segment holds the cut line, if any,
  and says bytes are `missing`. The writer then stores `store::LOG_GAP`, a line saying lines are missing there, which
  counts toward none of `log_max`, so a loss near the cap neither reads as a cut nor crowds out the lines after it.
  The capture never stitches the two sides together silently.

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
  long keeps about `log_max` of them per running job, however often its capture starts again.
- A script stuck on its stdin for 5 s holds the capture's reads that long before it starts again.
