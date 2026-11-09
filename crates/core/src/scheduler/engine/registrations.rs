//! What the engine remembers of a registration whose runner has no slot: that it ended the runner itself, and that a
//! warning about GitHub holding it was already given. It owns that record and must not call GitHub or log: `tick`
//! deletes the registrations and words what this decides.

use std::collections::{BTreeMap, BTreeSet};

/// A registration with no slot, known for at least one of two reasons: an entry with neither is dropped. They are
/// independent on purpose: GitHub can refuse to delete a runner this engine ended, and a runner is released while
/// `remove` deletes its registration in the background.
#[derive(Debug, Default)]
struct Ended {
    /// The repository of a runner this engine released or killed, until a listing of it no longer has the runner:
    /// GitHub holding one busy then is no orphan to warn of.
    kept: Option<String>,
    /// Whether GitHub already refused to delete it, so that nothing warns twice.
    reported: bool,
}

impl Ended {
    fn keep(&mut self, repository: &str) {
        self.kept = Some(repository.to_owned());
    }

    /// Records GitHub's refusal, and says whether this one is news to warn of.
    fn refused(&mut self) -> bool {
        let first = !std::mem::replace(&mut self.reported, true);
        first && self.kept.is_none()
    }

    /// Updates it from a listing of `repository`, and says whether it is still worth keeping.
    fn listed(&mut self, repository: &str, present: bool) -> bool {
        if !present && self.kept.as_deref() == Some(repository) {
            self.kept = None;
        }
        self.kept.is_some() || self.reported
    }
}

/// The registrations with no slot, by runner ID. The background deletes of released runners are not here: `Engine`
/// holds those tasks, which do not reach it. Nothing survives a restart, as a new engine has ended no runner and
/// warns again of one GitHub still holds. A reload changes nothing.
#[derive(Debug, Default)]
pub(super) struct Registrations(BTreeMap<u64, Ended>);

impl Registrations {
    /// Notes that this engine released or killed `runner`, so GitHub may hold its registration busy a while.
    pub(super) fn ended_here(&mut self, runner: u64, repository: &str) {
        self.0.entry(runner).or_default().keep(repository);
    }

    /// Whether this engine ended `runner`, and no listing of its repository has left it out since.
    pub(super) fn is_ended_here(&self, runner: u64) -> bool {
        self.0
            .get(&runner)
            .is_some_and(|ended| ended.kept.is_some())
    }

    /// Notes that GitHub refused to delete `runner` because it holds it busy, and says whether to warn: only of one
    /// this engine did not end, and only once.
    pub(super) fn refused(&mut self, runner: u64) -> bool {
        self.0.entry(runner).or_default().refused()
    }

    /// Takes in a listing of `repository`, which has the runners `present`.
    pub(super) fn listed(&mut self, repository: &str, present: &BTreeSet<u64>) {
        self.0
            .retain(|runner, ended| ended.listed(repository, present.contains(runner)));
    }

    /// How many registrations GitHub refused to delete.
    #[cfg(test)]
    pub(super) fn reported(&self) -> usize {
        self.0.values().filter(|ended| ended.reported).count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RUNNER: u64 = 7;

    fn none() -> BTreeSet<u64> {
        BTreeSet::new()
    }

    fn only_runner() -> BTreeSet<u64> {
        BTreeSet::from([RUNNER])
    }

    #[test]
    fn a_runner_ended_here_stays_known_until_a_listing_of_its_repository_no_longer_has_it() {
        let mut ledger = Registrations::default();
        ledger.ended_here(RUNNER, "o/r");
        assert!(ledger.is_ended_here(RUNNER), "known as soon as it ends");

        ledger.listed("o/other", &none());
        assert!(
            ledger.is_ended_here(RUNNER),
            "another repository's listing says nothing of it"
        );
        ledger.listed("o/r", &only_runner());
        assert!(ledger.is_ended_here(RUNNER), "still listed, so kept");

        ledger.listed("o/r", &none());
        assert!(
            !ledger.is_ended_here(RUNNER),
            "its repository's listing no longer has it"
        );
    }

    #[test]
    fn an_orphan_github_holds_is_warned_of_once() {
        let mut ledger = Registrations::default();
        assert!(ledger.refused(RUNNER), "the first refusal is news");
        assert!(!ledger.refused(RUNNER), "the next says nothing new");
    }

    #[test]
    fn github_holding_a_runner_ended_here_warns_of_nothing_and_it_stays_ended_here() {
        let mut ledger = Registrations::default();
        ledger.ended_here(RUNNER, "o/r");
        assert!(!ledger.refused(RUNNER), "this engine ended it");
        assert!(ledger.is_ended_here(RUNNER), "a refusal does not end that");
    }

    #[test]
    fn a_refusal_outlives_the_listing_that_ends_what_this_engine_knew() {
        let mut ledger = Registrations::default();
        ledger.ended_here(RUNNER, "o/r");
        ledger.refused(RUNNER);
        ledger.listed("o/r", &none());
        assert!(!ledger.is_ended_here(RUNNER), "no longer listed");
        assert!(
            !ledger.refused(RUNNER),
            "shown again by a listing whose pages shifted, it was already seen"
        );
        assert_eq!(ledger.reported(), 1);
    }

    #[test]
    fn a_runner_ended_here_and_never_refused_leaves_nothing_behind() {
        let mut ledger = Registrations::default();
        ledger.ended_here(RUNNER, "o/r");
        ledger.listed("o/r", &none());
        assert_eq!(ledger.0.len(), 0, "neither reason remains");
        assert!(
            ledger.refused(RUNNER),
            "forgotten, so one GitHub holds later is news"
        );
    }

    #[test]
    fn ending_a_runner_keeps_a_refusal_already_seen() {
        let mut ledger = Registrations::default();
        ledger.refused(RUNNER);
        ledger.ended_here(RUNNER, "o/r");
        ledger.listed("o/r", &none());
        assert_eq!(ledger.reported(), 1, "the refusal stays");
    }

    #[test]
    fn a_runner_nothing_is_known_of_is_not_ended_here() {
        let mut ledger = Registrations::default();
        ledger.listed("o/r", &none());
        assert!(!ledger.is_ended_here(RUNNER), "never seen");
        assert_eq!(ledger.reported(), 0);
    }
}
