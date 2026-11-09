//! What the engine remembers of a registration whose runner has no slot: that it ended the runner itself, and that a
//! warning about GitHub holding it was already given. It owns that record and must not call GitHub or log: `tick`
//! deletes the registrations and words what this decides.

use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, BTreeSet};

/// A registration with no slot that a listing of its repository still shows.
#[derive(Debug)]
struct Ended {
    repository: String,
    /// Whether this engine released or killed the runner, so that GitHub holding it is no orphan to warn of. If not,
    /// it is an orphan GitHub refused to delete, already warned of.
    kept: bool,
}

/// The registrations with no slot, by runner ID. An entry ends with the first listing of its repository that leaves
/// the runner out, or once nothing lists that repository. A listing whose pages shifted can end one early, which costs
/// a repeated warning at worst. The background deletes of released runners are not here: `Engine` holds those tasks,
/// which do not reach it. Nothing survives a restart, as a new engine has ended no runner and warns again of one GitHub
/// still holds. A reload changes nothing by itself: a repository it removes drains, and its listings go on.
#[derive(Debug, Default)]
pub(super) struct Registrations(BTreeMap<u64, Ended>);

impl Registrations {
    /// Notes that this engine released or killed `runner`, so GitHub may hold its registration busy a while.
    pub(super) fn ended_here(&mut self, runner: u64, repository: &str) {
        self.0.insert(
            runner,
            Ended {
                repository: repository.to_owned(),
                kept: true,
            },
        );
    }

    /// Whether this engine ended `runner`, and no listing of its repository has left it out since.
    pub(super) fn is_ended_here(&self, runner: u64) -> bool {
        self.0.get(&runner).is_some_and(|ended| ended.kept)
    }

    /// Notes that GitHub refused to delete `runner` of `repository` because it holds it busy, and says whether to
    /// warn: only of one this engine did not end, and only once.
    pub(super) fn refused(&mut self, runner: u64, repository: &str) -> bool {
        match self.0.entry(runner) {
            Entry::Occupied(_) => false,
            Entry::Vacant(entry) => {
                entry.insert(Ended {
                    repository: repository.to_owned(),
                    kept: false,
                });
                true
            }
        }
    }

    /// Takes in a listing of `repository`, which has the runners `present`.
    pub(super) fn listed(&mut self, repository: &str, present: &BTreeSet<u64>) {
        self.0
            .retain(|runner, ended| ended.repository != repository || present.contains(runner));
    }

    /// Forgets the registrations of repositories nothing lists any more, whose listings would have ended them.
    pub(super) fn forget_unlisted(&mut self, listed: impl Fn(&str) -> bool) {
        self.0.retain(|_, ended| listed(&ended.repository));
    }

    /// How many orphans GitHub refused to delete.
    #[cfg(test)]
    pub(super) fn orphans(&self) -> usize {
        self.0.values().filter(|ended| !ended.kept).count()
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
        assert!(ledger.refused(RUNNER, "o/r"), "the first refusal is news");
        assert!(!ledger.refused(RUNNER, "o/r"), "the next says nothing new");
        assert!(!ledger.is_ended_here(RUNNER), "this engine never ran it");
    }

    #[test]
    fn github_holding_a_runner_ended_here_warns_of_nothing_and_it_stays_ended_here() {
        let mut ledger = Registrations::default();
        ledger.ended_here(RUNNER, "o/r");
        assert!(!ledger.refused(RUNNER, "o/r"), "this engine ended it");
        assert!(ledger.is_ended_here(RUNNER), "a refusal does not end that");
    }

    #[test]
    fn an_orphan_is_forgotten_with_the_listing_of_its_own_repository_that_leaves_it_out() {
        let mut ledger = Registrations::default();
        ledger.refused(RUNNER, "o/r");

        ledger.listed("o/other", &none());
        assert_eq!(ledger.orphans(), 1, "another repository's listing");
        ledger.listed("o/r", &only_runner());
        assert_eq!(ledger.orphans(), 1, "still listed");

        ledger.listed("o/r", &none());
        assert_eq!(ledger.orphans(), 0, "no longer listed");
        assert!(
            ledger.refused(RUNNER, "o/r"),
            "held again, it is news again"
        );
    }

    #[test]
    fn nothing_is_left_of_a_repository_nothing_lists_and_the_rest_stays() {
        let mut ledger = Registrations::default();
        ledger.ended_here(1, "o/gone");
        ledger.refused(2, "o/gone");
        ledger.ended_here(3, "o/r");
        ledger.refused(4, "o/r");

        ledger.forget_unlisted(|repository| repository == "o/r");

        assert!(!ledger.is_ended_here(1), "ended here, repository gone");
        assert!(ledger.is_ended_here(3), "its repository is still listed");
        assert_eq!(ledger.orphans(), 1, "only the listed repository's orphan");
        assert!(ledger.refused(2, "o/gone"), "forgotten, so news again");
    }

    #[test]
    fn a_runner_nothing_is_known_of_is_not_ended_here() {
        let mut ledger = Registrations::default();
        ledger.listed("o/r", &none());
        ledger.forget_unlisted(|_| false);
        assert!(!ledger.is_ended_here(RUNNER), "never seen");
        assert_eq!(ledger.orphans(), 0);
    }
}
