//! The questions the footer asks, which any key answers: one is shown at a time, and only `Prompts`' methods change
//! which. It decides nothing about what an answer does (`app`) or how a question reads (`draw`).

use runnerd_core::api::ActionPreview;

use super::app::{Refused, Removal, Resizing};

/// A question waiting for its answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Prompt {
    /// Whether to force a pause the daemon refused because jobs run.
    Force(Refused),
    /// Whether to remove records.
    Remove(Removal),
    /// Whether to write the sizing advice into the daemon's config.
    Resize(Resizing),
    /// Whether to empty a cache volume.
    Empty(String),
    /// Whether to cancel or re-run a job, as core's preview says what that does.
    Action(Box<ActionPreview>),
}

impl Prompt {
    /// Which is shown first: the lowest. No two of one kind wait.
    fn rank(&self) -> u8 {
        match self {
            Prompt::Force(_) => 0,
            Prompt::Remove(_) => 1,
            Prompt::Resize(_) => 2,
            Prompt::Empty(_) => 3,
            Prompt::Action(_) => 4,
        }
    }
}

/// The questions waiting, the one shown first. A daemon's answer can ask over a question on the screen, which comes
/// back once the new one is answered.
///
/// Nothing of it survives leaving the TUI, and a reload of the daemon's config leaves it as it is.
#[derive(Debug, Default)]
pub(crate) struct Prompts {
    waiting: Vec<Prompt>,
}

impl Prompts {
    /// Asks `prompt`, in the place of one of its kind that waits.
    pub(crate) fn push(&mut self, prompt: Prompt) {
        self.waiting.retain(|other| other.rank() != prompt.rank());
        let at = self
            .waiting
            .partition_point(|other| other.rank() < prompt.rank());
        self.waiting.insert(at, prompt);
    }

    /// Takes the question shown, to answer it; the next waiting is shown from now on.
    pub(crate) fn answer(&mut self) -> Option<Prompt> {
        (!self.waiting.is_empty()).then(|| self.waiting.remove(0))
    }

    /// The question shown.
    pub(crate) fn top(&self) -> Option<&Prompt> {
        self.waiting.first()
    }
}
