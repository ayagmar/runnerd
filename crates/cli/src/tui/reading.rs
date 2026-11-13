//! What the TUI knows of one read of the daemon: not read yet, read, not served by an older daemon, or failed. It
//! decides nothing about when to read (`app`) or how a state is drawn (`draw`).

/// Why a read gave nothing to show.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Unread {
    /// No answer yet.
    Waiting,
    /// The daemon predates the route or field: shown as not supported, never as empty.
    Unsupported,
    /// The read failed; the text says why.
    Failed(String),
}

/// One read of the daemon. A later answer replaces it whole; a failed read after a good one keeps the good one, so a
/// blip does not blank a view. Nothing of it survives leaving the TUI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Reading<T> {
    /// Nothing to show yet, and why.
    Unread(Unread),
    /// The last answer.
    Read(T),
}

impl<T> Default for Reading<T> {
    fn default() -> Self {
        Reading::Unread(Unread::Waiting)
    }
}

impl<T> Reading<T> {
    /// The answer, once there is one.
    pub(crate) fn get(&self) -> Option<&T> {
        match self {
            Reading::Read(value) => Some(value),
            Reading::Unread(_) => None,
        }
    }

    /// Takes an answer: a value replaces what was there; a failure replaces only a state without a value, and a
    /// daemon that does not serve the route always says so.
    pub(crate) fn take(&mut self, answer: Result<T, Unread>) {
        match answer {
            Ok(value) => *self = Reading::Read(value),
            Err(Unread::Unsupported) => *self = Reading::Unread(Unread::Unsupported),
            Err(why) => {
                if let Reading::Unread(_) = self {
                    *self = Reading::Unread(why);
                }
            }
        }
    }
}
