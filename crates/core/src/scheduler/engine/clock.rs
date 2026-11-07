//! The clock a reading's age goes by. Whether a sample or a host reading is fresh decides what the budget counts, so
//! a test must be able to stop that clock: a slow machine then ages nothing the test did not write old. It must not
//! date what is stored, which goes by `time::now_ms`.

use crate::time;

/// The system's clock; in a test, one it stopped.
#[derive(Debug, Clone, Copy, Default)]
pub(super) struct Clock {
    /// What it reads in a test that stopped it, in milliseconds since the epoch.
    #[cfg(test)]
    stopped: Option<u64>,
}

impl Clock {
    /// Milliseconds since the Unix epoch, now.
    #[cfg_attr(
        not(test),
        expect(clippy::unused_self, reason = "only a test's clock has a state")
    )]
    pub(super) fn now_ms(self) -> u64 {
        #[cfg(test)]
        if let Some(stopped) = self.stopped {
            return stopped;
        }
        time::now_ms()
    }

    /// A clock stopped at the system's time.
    #[cfg(test)]
    pub(super) fn stopped() -> Clock {
        Clock {
            stopped: Some(time::now_ms()),
        }
    }
}
