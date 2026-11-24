//! What is drawn over a view and takes the keys while it is open: the help, a list to pick a filter or an order from,
//! or a line to type a search in. One at a time; the footer's questions (`prompts`) are apart, as the daemon asks
//! those. It decides which option a key moves to; what a choice does is `app`'s, how it looks `draw`'s.

use runnerd_core::api::{self, JobOrder, JobRange, RecordFilter};

/// What is open over the view. Nothing of it survives leaving the TUI.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) enum Overlay {
    #[default]
    None,
    /// The keys, at `page` of them for a terminal too short for all: the drawing takes it modulo the pages there
    /// are, so `j` past the last page comes back to the first.
    Help {
        page: usize,
    },
    Pick(Picker),
    Input(Input),
}

/// What a list picks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Pick {
    /// Which filter of the jobs to set.
    Filters,
    Outcome,
    Repository,
    Workflow,
    Branch,
    Record,
    Range,
    Order,
    /// Which machine the TUI reads (ADR 0166).
    Machine,
}

/// What choosing an option does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Value {
    /// Opens the list of that filter's values.
    Filter(Pick),
    /// A text filter's value, `None` for any.
    Text(Option<String>),
    Record(Option<RecordFilter>),
    Range(JobRange),
    Order(JobOrder),
    /// A machine of `machines.toml` by its name, or `None` for this machine's daemon.
    Machine(Option<String>),
}

/// One line of a list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Choice {
    pub(crate) label: String,
    /// What is set now, drawn beside the label in the list of filters; a machine's state in the list of machines.
    pub(crate) note: String,
    /// What the note means, for its colour: quiet but for a machine's state.
    pub(crate) tone: api::Tone,
    pub(crate) value: Value,
}

/// A list to pick from, with the option the keys are on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Picker {
    pub(crate) pick: Pick,
    choices: Vec<Choice>,
    at: usize,
}

impl Choice {
    /// An option without a note.
    pub(crate) fn of(label: impl Into<String>, value: Value) -> Choice {
        Choice {
            label: label.into(),
            note: String::new(),
            tone: api::Tone::Quiet,
            value,
        }
    }
}

impl Picker {
    /// The list of `choices`, on the one whose value is set now, `current`, or else the first.
    pub(crate) fn new(pick: Pick, choices: Vec<Choice>, current: &Value) -> Picker {
        let at = choices
            .iter()
            .position(|choice| choice.value == *current)
            .unwrap_or(0);
        Picker { pick, choices, at }
    }

    pub(crate) fn choices(&self) -> &[Choice] {
        &self.choices
    }

    /// The option the keys are on.
    pub(crate) fn at(&self) -> usize {
        self.at
    }

    /// Moves `by` options, staying in the list.
    pub(crate) fn step(&mut self, by: isize) {
        self.at = self
            .at
            .saturating_add_signed(by)
            .min(self.choices.len().saturating_sub(1));
    }

    /// Puts the keys on option `at`, when there is one.
    pub(crate) fn point(&mut self, at: usize) {
        if at < self.choices.len() {
            self.at = at;
        }
    }

    /// The option chosen.
    pub(crate) fn chosen(&self) -> Option<&Choice> {
        self.choices.get(self.at)
    }

    /// Replaces the option of `choice`'s value with it, the keys staying where they are; one the list lacks is
    /// dropped.
    pub(crate) fn replace(&mut self, choice: Choice) {
        if let Some(kept) = self
            .choices
            .iter_mut()
            .find(|kept| kept.value == choice.value)
        {
            *kept = choice;
        }
    }

    /// Replaces the options with `choices`, the keys staying on the same line when there still is one.
    pub(crate) fn fill(&mut self, choices: Vec<Choice>) {
        self.choices = choices;
        self.at = self.at.min(self.choices.len().saturating_sub(1));
    }
}

/// Where typed text goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Typing {
    /// A search of the jobs' names, workflows and branches.
    Jobs,
    /// A search of the open job's log messages.
    Log,
}

/// A line being typed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Input {
    pub(crate) typing: Typing,
    pub(crate) text: String,
}
