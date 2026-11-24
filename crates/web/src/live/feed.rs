//! A live page's feed (ADR 0177): whether what the page shows is current, is waiting for its first read, is kept
//! while a read that failed is tried again, or will not be updated again and why; and when the stream last delivered
//! the daemon's data. Owns the words of the page's live badge for each state and the wait between failed reads. It
//! must not read the daemon, decide which failures are outages (core's `ClientError::is_outage` does), or know a
//! page's regions.

use std::time::Duration;

use runnerd_core::client::ClientError;
use serde::Serialize;

use crate::format;

/// The first wait after a read fails; each failure in a row doubles it, up to `MAX_WAIT`.
const FIRST_WAIT: Duration = Duration::from_millis(500);

/// The longest wait between reads, as long as Datastar waits at most before it connects a failed stream again.
pub(crate) const MAX_WAIT: Duration = Duration::from_secs(10);

/// What the badge says while the page's stream is not connected, which only the page knows: Datastar connects it
/// again.
const OFFLINE: &str = "Offline";

/// Why a page's stream ended for good: none of these is retried.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Ending {
    /// The job finished: the page shows its final state.
    Finished,
    /// Every device was signed out: a retry would be refused.
    SignedOut,
    /// The daemon answered the page's read with an error, shown as a notice: a retry would most likely fail the same
    /// way.
    Refused,
    /// The dashboard is stopping.
    Stopped,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum State {
    /// Connected; the stream has not delivered the daemon's data yet.
    Awaiting,
    /// The page shows what the daemon said at the last delivery.
    Current,
    /// A read failed as the daemon was away; the page keeps what it shows while it is read again.
    Retrying {
        /// The failures in a row, for the wait.
        failures: u32,
        /// The last failure, worded by core.
        why: String,
    },
    /// The stream ended for good.
    Ended(Ending),
}

/// One page's feed, held by its stream's task for as long as it runs. Nothing survives the stream: a page that
/// connects again starts a new feed from the time its data was rendered or last delivered, which it names.
#[derive(Debug)]
pub(crate) struct Feed {
    state: State,
    /// When the page last got the daemon's data: its render, or the stream's last delivery. Never a keep-alive.
    delivered: Option<u64>,
}

/// The `_feed` signal: what the badge shows, worded here.
#[derive(Debug, Serialize)]
struct Signal {
    state: &'static str,
    /// Whether the stream ended for good, so the page does not say it is retrying.
    ended: bool,
    /// Whether reloading the page is the way back.
    reload: bool,
    label: &'static str,
    title: String,
    /// The badge's label and title while the stream is not connected.
    offline: &'static str,
    offline_title: String,
    /// When the page's data is from, beside the badge when it is not current; empty when it does not matter.
    since: String,
    /// The last delivery, in seconds since the epoch; 0 when there was none.
    at: u64,
}

impl Feed {
    /// A stream just connected for a page whose data is from `delivered`, the time the page names.
    pub(crate) fn connecting(delivered: Option<u64>) -> Feed {
        Feed {
            state: State::Awaiting,
            delivered,
        }
    }

    /// The stream delivered the daemon's data, read at `at`.
    pub(crate) fn delivered(&mut self, at: u64) {
        self.state = State::Current;
        self.delivered = Some(at);
    }

    /// A read failed as the daemon was away; how long to wait before reading again.
    pub(crate) fn failed(&mut self, error: &ClientError) -> Duration {
        let failures = match &self.state {
            State::Retrying { failures, .. } => failures.saturating_add(1),
            _ => 1,
        };
        self.state = State::Retrying {
            failures,
            why: error.to_string(),
        };
        FIRST_WAIT
            .saturating_mul(1 << (failures - 1).min(5))
            .min(MAX_WAIT)
    }

    /// The stream ends for good.
    pub(crate) fn end(&mut self, why: Ending) {
        self.state = State::Ended(why);
    }

    /// The `_feed` signal: the value of Datastar's `data-signals:_feed` on a page, which a stream patches.
    pub(crate) fn json(&self) -> serde_json::Value {
        serde_json::to_value(self.signal()).unwrap_or_default()
    }

    /// The `_feed` signal as a Datastar signal patch.
    pub(crate) fn patch(&self) -> String {
        serde_json::json!({ "_feed": self.json() }).to_string()
    }

    fn signal(&self) -> Signal {
        let since = self
            .delivered
            .map(|at| format!("as of {}", format::timestamp(at)))
            .unwrap_or_default();
        let shown = if since.is_empty() {
            String::new()
        } else {
            format!(" Showing what runnerd said {since}.")
        };
        let (state, label, title, reload) = match &self.state {
            State::Awaiting => (
                "awaiting",
                "Connecting",
                format!("Reading runnerd.{shown}"),
                false,
            ),
            State::Current => (
                "current",
                "Live",
                format!("Updates as it happens.{shown}"),
                false,
            ),
            State::Retrying { why, .. } => (
                "retrying",
                "Retrying",
                format!("{}; reading it again.{shown}", format::sentence(why)),
                false,
            ),
            State::Ended(Ending::Finished) => (
                "finished",
                "Finished",
                "The job finished: this is its final state.".to_owned(),
                false,
            ),
            State::Ended(Ending::SignedOut) => (
                "signed-out",
                "Signed out",
                format!("This browser was signed out; reload to sign in again.{shown}"),
                true,
            ),
            State::Ended(Ending::Refused) => (
                "refused",
                "Not updating",
                format!("runnerd refused this page's updates; reload to try again.{shown}"),
                true,
            ),
            State::Ended(Ending::Stopped) => (
                "stopped",
                "Stopped",
                format!("The dashboard stopped; reload once it is back.{shown}"),
                true,
            ),
        };
        let ended = matches!(self.state, State::Ended(_));
        Signal {
            state,
            ended,
            reload,
            label,
            title,
            offline: OFFLINE,
            offline_title: format!("Not connected; connecting again.{shown}"),
            since: if self.state == State::Ended(Ending::Finished) {
                String::new()
            } else {
                since
            },
            at: self.delivered.unwrap_or_default(),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "tests")]
mod tests {
    use super::*;

    fn signal(feed: &Feed) -> serde_json::Value {
        feed.json()
    }

    /// Waits that grew without bound would leave a page stale long after the daemon is back; ones that never grew
    /// would hammer a daemon that is restarting.
    #[test]
    fn failures_in_a_row_wait_longer_up_to_the_cap_and_a_delivery_starts_over() {
        let mut feed = Feed::connecting(None);
        let away = ClientError::Transport("connection reset".to_owned());
        let waits: Vec<Duration> = (0..8).map(|_| feed.failed(&away)).collect();
        assert_eq!(waits[0], FIRST_WAIT);
        assert_eq!(waits[1], FIRST_WAIT * 2);
        assert!(waits.windows(2).all(|pair| pair[0] <= pair[1]), "{waits:?}");
        assert_eq!(waits.last(), Some(&MAX_WAIT));
        feed.delivered(10);
        assert_eq!(feed.failed(&away), FIRST_WAIT);
    }

    /// The badge said "retrying" on a stream that had ended for good, and kept data looked current while reads failed.
    #[test]
    fn retrying_keeps_the_last_delivery_and_an_end_never_claims_a_retry() {
        let mut feed = Feed::connecting(Some(1_792_677_787));
        assert_eq!(signal(&feed)["state"], "awaiting");
        feed.delivered(1_792_677_800);
        feed.failed(&ClientError::Timeout(Duration::from_secs(30)));
        let retrying = signal(&feed);
        assert_eq!(retrying["state"], "retrying");
        assert_eq!(retrying["at"], 1_792_677_800);
        assert_eq!(retrying["since"], "as of 2026-10-22 14:03 UTC");
        assert_eq!(retrying["ended"], false);
        for (why, state) in [
            (Ending::SignedOut, "signed-out"),
            (Ending::Refused, "refused"),
            (Ending::Stopped, "stopped"),
            (Ending::Finished, "finished"),
        ] {
            feed.end(why);
            let ended = signal(&feed);
            assert_eq!(ended["state"], state);
            assert_eq!(ended["ended"], true);
            assert_eq!(ended["reload"], why != Ending::Finished, "{state}");
            assert!(
                !ended["title"]
                    .as_str()
                    .unwrap()
                    .contains("reading it again")
                    && ended["label"] != "Retrying",
                "{state} claims a retry: {ended}"
            );
        }
    }
}
