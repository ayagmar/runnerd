//! Whether a runner's job may write, as far as the engine has said, with what hangs on that: the uploads held until it
//! says, and the job's scopes once it has. Only `WriteDecision`'s methods change it. It must not touch the disk or the
//! uploads' room, nor decide anything itself: `Writes` comes from the engine, and what it lets go its caller removes.

use super::{MAX_HELD, Scopes, Upload, Writes};

/// A runner's writes, undecided until the engine decides them, once.
///
/// Nothing of it survives a restart: `attach` opens a runner undecided again, and the uploads directory is emptied at
/// start. A reload leaves it as it is.
#[derive(Debug)]
pub(super) struct WriteDecision {
    state: DecisionState,
}

#[derive(Debug)]
enum DecisionState {
    /// Not known yet.
    Undecided {
        /// Uploads waiting for the decision, at most `MAX_HELD`.
        held: Vec<Upload>,
    },
    Allowed {
        scopes: Scopes,
    },
    Refused {
        /// Its repository's default branch's ref, which is all a job that may not write reads by.
        default: Option<String>,
    },
}

impl WriteDecision {
    pub(super) fn undecided() -> WriteDecision {
        WriteDecision {
            state: DecisionState::Undecided { held: Vec::new() },
        }
    }

    pub(super) fn current(&self) -> Writes {
        match self.state {
            DecisionState::Undecided { .. } => Writes::Undecided,
            DecisionState::Allowed { .. } => Writes::Allowed,
            DecisionState::Refused { .. } => Writes::Refused,
        }
    }

    pub(super) fn is_undecided(&self) -> bool {
        matches!(self.state, DecisionState::Undecided { .. })
    }

    /// The uploads waiting for the decision: none once it is made.
    pub(super) fn held(&self) -> &[Upload] {
        match &self.state {
            DecisionState::Undecided { held } => held,
            DecisionState::Allowed { .. } | DecisionState::Refused { .. } => &[],
        }
    }

    /// Holds `upload` until the decision, or hands it back when the decision is made or `MAX_HELD` are held.
    pub(super) fn hold(&mut self, upload: Upload) -> Result<(), Upload> {
        match &mut self.state {
            DecisionState::Undecided { held } if held.len() < MAX_HELD => {
                held.push(upload);
                Ok(())
            }
            _ => Err(upload),
        }
    }

    /// Makes the decision, with the scopes of the job's GitHub Actions cache entries, and returns the uploads it held.
    /// `None`, changing nothing, when it is made already or `writes` is `Undecided`.
    pub(super) fn decide(&mut self, writes: Writes, scopes: Scopes) -> Option<Vec<Upload>> {
        let decided = match writes {
            Writes::Undecided => return None,
            Writes::Allowed => DecisionState::Allowed { scopes },
            Writes::Refused => DecisionState::Refused {
                default: scopes.default,
            },
        };
        let DecisionState::Undecided { held } = &mut self.state else {
            return None;
        };
        let held = std::mem::take(held);
        self.state = decided;
        Some(held)
    }

    /// The job's scopes, when it may write.
    pub(super) fn allowed(&self) -> Option<&Scopes> {
        match &self.state {
            DecisionState::Allowed { scopes } => Some(scopes),
            DecisionState::Undecided { .. } | DecisionState::Refused { .. } => None,
        }
    }

    /// The ref the job's entries go to, when it may write into one of its own.
    pub(super) fn own(&self) -> Option<&String> {
        self.allowed()?.own.as_ref()
    }

    /// Its repository's default branch's ref, once decided.
    pub(super) fn default_branch(&self) -> Option<&String> {
        match &self.state {
            DecisionState::Undecided { .. } => None,
            DecisionState::Allowed { scopes } => scopes.default.as_ref(),
            DecisionState::Refused { default } => default.as_ref(),
        }
    }
}
