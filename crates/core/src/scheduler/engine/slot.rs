//! Whether a profile's slot is free, starting a runner or held. It owns that value and nothing else: it must not decide
//! what fills or empties a slot, which the engine's starts, removals and recovery do, nor read or write the occupant's
//! or the start's own fields.

use super::start::Starting;
use super::{Footprint, Occupant};

/// A slot of a profile (ADR 0123, 0125). Only its own methods write it.
///
/// Nothing of it survives a restart: every slot starts free, and recovery places each runner container it adopts in
/// a free one. A start in flight exists only inside a batch of starts, which a restart drops: its registration is
/// deleted by the first runners listing, and its container adopted or removed by recovery. A reload keeps it, but
/// none is answered while a batch runs: a slot past a lowered count stays held until its runner goes, and only then
/// is it dropped.
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
    /// A start in flight: named and admitted, registering its runner or creating its container.
    Starting(Starting),
    /// A runner, or the container of a failed start that Docker kept.
    Held(Occupant),
}

/// What is in a slot, for a reader that tells all three apart.
#[derive(Debug, Clone, Copy)]
pub(super) enum Holder<'slot> {
    Free,
    Starting(&'slot Starting),
    Runner(&'slot Occupant),
}

impl Slot {
    /// Whether nothing holds it, or starts in it.
    pub(super) fn is_free(&self) -> bool {
        matches!(self.state, Holding::Free)
    }

    /// What is in it.
    pub(super) fn holder(&self) -> Holder<'_> {
        match &self.state {
            Holding::Free => Holder::Free,
            Holding::Starting(starting) => Holder::Starting(starting),
            Holding::Held(occupant) => Holder::Runner(occupant),
        }
    }

    /// What holds it, if a runner does. A start in flight is not one yet.
    pub(super) fn occupant(&self) -> Option<&Occupant> {
        match &self.state {
            Holding::Free | Holding::Starting(_) => None,
            Holding::Held(occupant) => Some(occupant),
        }
    }

    /// What holds it, to note what that runner is doing.
    pub(super) fn occupant_mut(&mut self) -> Option<&mut Occupant> {
        match &mut self.state {
            Holding::Free | Holding::Starting(_) => None,
            Holding::Held(occupant) => Some(occupant),
        }
    }

    /// What its runner, or the start in it, takes of the budgets.
    pub(super) fn footprint(&self) -> Option<Footprint<'_>> {
        match &self.state {
            Holding::Free => None,
            Holding::Starting(starting) => Some(starting.footprint()),
            Holding::Held(occupant) => Some(occupant.footprint()),
        }
    }

    /// Starts `starting` in it.
    pub(super) fn launch(&mut self, starting: Starting) {
        self.state = Holding::Starting(starting);
    }

    /// Ends the start in flight in it, if one is, giving it back: it is free until its runner is placed.
    pub(super) fn end_start(&mut self) -> Option<Starting> {
        match std::mem::take(&mut self.state) {
            Holding::Starting(starting) => Some(starting),
            other => {
                self.state = other;
                None
            }
        }
    }

    /// Puts `occupant` in it.
    pub(super) fn place(&mut self, occupant: Occupant) {
        self.state = Holding::Held(occupant);
    }

    /// Frees it of its runner, giving that back. A start in flight stays: only its batch ends it.
    pub(super) fn vacate(&mut self) -> Option<Occupant> {
        match std::mem::take(&mut self.state) {
            Holding::Held(occupant) => Some(occupant),
            other => {
                self.state = other;
                None
            }
        }
    }
}
