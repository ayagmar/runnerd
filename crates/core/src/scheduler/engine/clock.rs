//! The clock a reading's age goes by. Whether a sample or a host reading is fresh decides what the budget counts, so
//! a test must be able to stop that clock: a slow machine then ages nothing the test did not write old. It must not
//! date what is stored, which goes by `time::now_ms`.

use crate::time;

/// The system's clock; in a test, one it stopped, which every copy of it reads and which it may move on.
#[derive(Debug, Clone, Default)]
pub(super) struct Clock {
    /// What it reads in a test that stopped it, in milliseconds since the epoch, shared by its copies: a hook of a
    /// fake can age the readings while the engine starts runners.
    #[cfg(test)]
    stopped: Option<std::sync::Arc<std::sync::atomic::AtomicU64>>,
}

impl Clock {
    /// Milliseconds since the Unix epoch, now.
    #[cfg_attr(
        not(test),
        expect(clippy::unused_self, reason = "only a test's clock has a state")
    )]
    pub(super) fn now_ms(&self) -> u64 {
        #[cfg(test)]
        if let Some(stopped) = &self.stopped {
            return stopped.load(std::sync::atomic::Ordering::SeqCst);
        }
        time::now_ms()
    }
}

#[cfg(test)]
impl Clock {
    /// A clock stopped at the system's time.
    pub(super) fn stopped() -> Clock {
        Clock {
            stopped: Some(std::sync::Arc::new(time::now_ms().into())),
        }
    }

    /// Moves a stopped clock, and each copy of it, `by` on.
    pub(super) fn advance(&self, by: std::time::Duration) {
        if let Some(stopped) = &self.stopped {
            let by = u64::try_from(by.as_millis()).unwrap_or(u64::MAX);
            stopped.fetch_add(by, std::sync::atomic::Ordering::SeqCst);
        }
    }
}
