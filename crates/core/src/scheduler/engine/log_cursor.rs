//! Where one runner's log capture stands (ADR 0176): how far it read, which segments it sent the history and is waiting
//! on, and how far the history committed. It owns that value, which only its methods change, and tells the capture
//! when to let the script drop pages; it must not read the script's output, call Docker or write to the store.

use std::collections::VecDeque;

use tokio::sync::oneshot;

use crate::store::{Appended, LogPosition};

/// A capture's position in its job's log. Lives as long as the capture task: nothing survives a restart or a reload,
/// as the next capture starts from where the history's stored log ends, which only a commit moves.
#[derive(Debug)]
pub(super) struct LogCursor {
    /// Where the history's log of the job ends: where the capture started, then each segment's end once committed.
    committed: LogPosition,
    /// The segments sent to the history and not answered yet, oldest first: each starts where the one before ends.
    queued: VecDeque<Queued>,
    /// The page the script was last told the history committed every page before.
    acked: u64,
}

/// A segment the history has not answered for yet.
#[derive(Debug)]
struct Queued {
    /// Where it ends.
    to: LogPosition,
    appended: oneshot::Receiver<Appended>,
}

/// What the history said of the oldest segment sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Answer {
    /// It is committed. `ack`: the history now has every page before this one, which the script may drop.
    Committed { ack: Option<u64> },
    /// The job's log keeps no more: it is cut at `log_max`, or packed.
    Full,
    /// It is not known to be stored: only the history's stored position says where the log ends now.
    Refused,
}

impl LogCursor {
    /// A capture that starts where the history's log of the job ends.
    pub(super) fn at(start: LogPosition) -> LogCursor {
        LogCursor {
            committed: start,
            queued: VecDeque::new(),
            acked: start.page,
        }
    }

    /// Where the next segment starts: where the last one read ends.
    pub(super) fn read(&self) -> LogPosition {
        self.queued
            .back()
            .map_or(self.committed, |queued| queued.to)
    }

    /// Notes a segment sent to the history, from `read` to `to`, whose answer comes on `appended`.
    pub(super) fn sent(&mut self, to: LogPosition, appended: oneshot::Receiver<Appended>) {
        self.queued.push_back(Queued { to, appended });
    }

    /// Whether every segment sent has its answer.
    pub(super) fn is_settled(&self) -> bool {
        self.queued.is_empty()
    }

    /// Waits for the oldest segment's answer, and moves the committed position past it if it was stored. Never
    /// returns while every segment is answered. Cancel safe: an answer not taken stays.
    pub(super) async fn answer(&mut self) -> Answer {
        let Some(oldest) = self.queued.front_mut() else {
            return std::future::pending().await;
        };
        // A writer gone before it answered kept nothing it said it kept.
        let appended = (&mut oldest.appended).await.unwrap_or(Appended::Refused);
        let to = oldest.to;
        self.queued.pop_front();
        match appended {
            Appended::Stored => {
                self.committed = to;
                let ack = (to.page > self.acked).then_some(to.page);
                if let Some(page) = ack {
                    self.acked = page;
                }
                Answer::Committed { ack }
            }
            Appended::Full => Answer::Full,
            Appended::Refused => Answer::Refused,
        }
    }
}
