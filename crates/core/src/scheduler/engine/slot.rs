//! Whether a profile's slot is free or held. It owns that value and nothing else: it must not decide what fills or
//! empties a slot, which the engine's starts, removals and recovery do, nor read or write the occupant's own fields.

use super::Occupant;

/// A slot of a profile (ADR 0123). Only its own methods write it.
///
/// Nothing of it survives a restart: every slot starts free, and recovery places each runner container it adopts in
/// a free one. A reload keeps it: a slot past a lowered count stays held until its runner goes, and only then is it
/// dropped.
#[derive(Debug, Default)]
pub(super) struct Slot {
    state: Holding,
}

#[derive(Debug, Default)]
#[allow(
    clippy::large_enum_variant,
    reason = "a profile's slots are a few, mostly held, and kept inline as `Option<Occupant>` was"
)]
enum Holding {
    #[default]
    Free,
    /// A runner, or the container of a failed start that Docker kept.
    Held(Occupant),
}

impl Slot {
    /// Whether nothing holds it.
    pub(super) fn is_free(&self) -> bool {
        matches!(self.state, Holding::Free)
    }

    /// What holds it, if anything does.
    pub(super) fn occupant(&self) -> Option<&Occupant> {
        match &self.state {
            Holding::Free => None,
            Holding::Held(occupant) => Some(occupant),
        }
    }

    /// What holds it, to note what that runner is doing.
    pub(super) fn occupant_mut(&mut self) -> Option<&mut Occupant> {
        match &mut self.state {
            Holding::Free => None,
            Holding::Held(occupant) => Some(occupant),
        }
    }

    /// Puts `occupant` in it.
    pub(super) fn place(&mut self, occupant: Occupant) {
        self.state = Holding::Held(occupant);
    }

    /// Frees it, giving back what held it.
    pub(super) fn vacate(&mut self) -> Option<Occupant> {
        match std::mem::take(&mut self.state) {
            Holding::Free => None,
            Holding::Held(occupant) => Some(occupant),
        }
    }
}
