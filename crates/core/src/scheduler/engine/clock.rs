//! The clock a reading's age goes by, and the one that says how long the host slept. Whether a sample or a host
//! reading is fresh decides what the budget counts, so a test must be able to stop that clock: a slow machine then
//! ages nothing the test did not write old. A test must also be able to put the host to sleep, or step its wall clock,
//! without waiting. It must not date what is stored, which goes by `time::now_ms`.

use crate::time;

/// The system's clocks; in a test, ones it stopped, which every copy of it reads and which it may move on.
#[derive(Debug, Clone, Default)]
pub(super) struct Clock {
    /// What it reads in a test that stopped it, in milliseconds since the epoch, shared by its copies: a hook of a
    /// fake can age the readings while the engine starts runners.
    #[cfg(test)]
    stopped: Option<std::sync::Arc<std::sync::atomic::AtomicU64>>,
    /// What a test that stopped it says the host was suspended for since it booted, in ms, shared by its copies.
    #[cfg(test)]
    suspended: Option<std::sync::Arc<std::sync::atomic::AtomicU64>>,
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

    /// How long the host was suspended since it booted, in ms (`time::suspended_ms`): what grows by the time it slept.
    #[cfg_attr(
        not(test),
        expect(clippy::unused_self, reason = "only a test's clock has a state")
    )]
    pub(super) fn suspended_ms(&self) -> u64 {
        #[cfg(test)]
        if let Some(suspended) = &self.suspended {
            return suspended.load(std::sync::atomic::Ordering::SeqCst);
        }
        time::suspended_ms()
    }
}

#[cfg(test)]
impl Clock {
    /// A clock stopped at the system's time, on a host that never slept.
    pub(super) fn stopped() -> Clock {
        Clock {
            stopped: Some(std::sync::Arc::new(time::now_ms().into())),
            suspended: Some(std::sync::Arc::new(0.into())),
        }
    }

    /// Moves a stopped clock, and each copy of it, `by` on.
    pub(super) fn advance(&self, by: std::time::Duration) {
        if let Some(stopped) = &self.stopped {
            stopped.fetch_add(ms(by), std::sync::atomic::Ordering::SeqCst);
        }
    }

    /// The host sleeps `by`: the wall clock moves on and so does the time suspended, as Linux keeps them.
    pub(super) fn sleep(&self, by: std::time::Duration) {
        self.advance(by);
        if let Some(suspended) = &self.suspended {
            suspended.fetch_add(ms(by), std::sync::atomic::Ordering::SeqCst);
        }
    }
}

#[cfg(test)]
fn ms(by: std::time::Duration) -> u64 {
    u64::try_from(by.as_millis()).unwrap_or(u64::MAX)
}
