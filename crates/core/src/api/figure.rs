//! A figure as every client shows it among others: its label, its value and what it is out of, worded here so the
//! dashboard and the TUI show the same words. Clients lay figures out and never word them. Holds no state.

/// One figure of a group: `Median a job` `120M`, or `Lookups hit, 7 days` `75%` `of 400`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LabelledFigure {
    /// What it counts: `Median a job`.
    pub label: String,
    /// The figure: `120M`.
    pub value: String,
    /// What it is out of, where it says: `of 400`.
    pub of: Option<String>,
}

impl LabelledFigure {
    /// `value`, labelled `label`.
    pub(crate) fn new(label: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            value: value.into(),
            of: None,
        }
    }

    /// It, out of `of`.
    pub(crate) fn of(mut self, of: impl Into<String>) -> Self {
        self.of = Some(of.into());
        self
    }
}
