//! What a figure or label means for the reader, as every client colours it: core decides the meaning, clients only
//! pick the colour (a CSS class, an ANSI code, a ratatui style).

/// What a shown fact means for the reader.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    /// All is well, or a change for the better.
    Good,
    /// Worth a look: something waits, or nears a limit.
    Warn,
    /// Something failed or is wrong.
    Bad,
    /// A plain fact.
    Neutral,
    /// A fact in the background, such as an idle slot or a note.
    Quiet,
}

/// Text and what it means, for a client to colour.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Toned {
    /// What it says.
    pub text: String,
    /// What it means.
    pub tone: Tone,
}
