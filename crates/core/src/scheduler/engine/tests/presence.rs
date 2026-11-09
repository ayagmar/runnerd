//! The rules of a repository's presence, and that they do what the two maps they replaced did.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use tokio::time::Instant;

use super::super::presence::{Repositories, RunnersListing};
use super::invariants::Rng;

const DRAIN_FAILING_FOR: Duration = Duration::from_hours(1);

const ANSWERED_CLEAN: RunnersListing = RunnersListing::Answered { left: false };
const ANSWERED_LEFT: RunnersListing = RunnersListing::Answered { left: true };
const FAILED: RunnersListing = RunnersListing::Failed { gone: false };
const GONE: RunnersListing = RunnersListing::Failed { gone: true };

fn names(names: &[&str]) -> BTreeSet<String> {
    names.iter().map(|name| (*name).to_owned()).collect()
}

/// `kept` listed, and `removed` draining after a reload took it away.
fn draining(removed: &str, kept: &str) -> Repositories {
    let mut repositories = Repositories::new(names(&[removed, kept]));
    repositories.reload(&names(&[kept]));
    repositories
}

/// Marks `name`'s state, so a test can tell it was kept from one made afresh.
fn mark(repositories: &mut Repositories, name: &str) {
    repositories
        .listed_mut(name)
        .unwrap()
        .listing
        .answer(Vec::new());
}

fn is_marked(repositories: &Repositories, name: &str) -> bool {
    repositories
        .listed(name)
        .is_some_and(|state| state.listing.is_listed())
}

#[test]
fn runners_are_listed_for_the_listed_repositories_before_the_draining_ones() {
    // By name alone the draining `a/gone` comes first: a map keyed by name mixes them.
    let repositories = draining("a/gone", "z/kept");
    assert_eq!(repositories.runners_to_list(), ["z/kept", "a/gone"]);
}

#[test]
fn a_drain_ends_with_its_second_clean_listing_in_a_row() {
    let mut repositories = draining("o/gone", "o/kept");
    assert!(!repositories.runners_listed("o/gone", ANSWERED_CLEAN));
    assert!(!repositories.runners_listed("o/gone", ANSWERED_LEFT));
    assert!(
        !repositories.runners_listed("o/gone", ANSWERED_CLEAN),
        "the runner seen in between started the count over"
    );
    assert!(repositories.runners_listed("o/gone", ANSWERED_CLEAN));
    assert!(!repositories.is_known("o/gone"), "its drain is over");
}

#[tokio::test(start_paused = true)]
async fn a_failed_listing_or_a_rate_limit_between_two_clean_ones_does_not_undo_the_first() {
    for between in [FAILED, RunnersListing::RateLimited] {
        let mut repositories = draining("o/gone", "o/kept");
        repositories.runners_listed("o/gone", ANSWERED_CLEAN);
        repositories.runners_listed("o/gone", between);
        assert!(
            repositories.runners_listed("o/gone", ANSWERED_CLEAN),
            "{between:?} leaves the first clean listing standing"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn a_drain_gives_up_after_an_hour_of_failures_unless_the_clock_starts_over() {
    for (between, ends) in [
        (None, true),
        (Some(RunnersListing::RateLimited), false),
        (Some(ANSWERED_LEFT), false),
        (Some(FAILED), true),
    ] {
        let mut repositories = draining("o/gone", "o/kept");
        assert!(!repositories.runners_listed("o/gone", FAILED));
        tokio::time::advance(Duration::from_mins(59)).await;
        if let Some(between) = between {
            assert!(!repositories.runners_listed("o/gone", between));
        }
        tokio::time::advance(Duration::from_mins(1)).await;
        assert_eq!(
            repositories.runners_listed("o/gone", FAILED),
            ends,
            "an hour after the first failure, with {between:?} at 59 minutes"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn a_drain_ends_at_once_when_github_says_the_repository_is_gone() {
    let mut repositories = draining("o/gone", "o/kept");
    assert!(repositories.runners_listed("o/gone", GONE));
    assert!(!repositories.is_known("o/gone"), "nothing lists it now");
}

#[tokio::test(start_paused = true)]
async fn a_listing_says_nothing_of_a_listed_or_an_unknown_repository() {
    let mut repositories = draining("o/gone", "o/kept");
    mark(&mut repositories, "o/kept");
    for name in ["o/kept", "o/never", ""] {
        for listing in [GONE, FAILED, RunnersListing::RateLimited, ANSWERED_CLEAN] {
            assert!(!repositories.runners_listed(name, listing), "{name:?}");
        }
    }
    assert!(is_marked(&repositories, "o/kept"), "its state is untouched");
    assert!(!repositories.is_known("o/never"), "a listing adds no name");
    assert!(!repositories.is_known(""), "a listing adds no name");
}

#[test]
fn a_reload_lists_what_the_config_has_and_drains_what_it_lost() {
    let mut repositories = Repositories::new(names(&["o/stays", "o/leaves"]));
    mark(&mut repositories, "o/stays");
    mark(&mut repositories, "o/leaves");

    repositories.reload(&names(&["o/stays", "o/new"]));

    assert!(is_marked(&repositories, "o/stays"), "it keeps its state");
    assert!(repositories.listed("o/new").is_some(), "new, so listed");
    assert!(
        repositories.listed("o/leaves").is_none(),
        "no longer listed"
    );
    assert_eq!(
        repositories.runners_to_list(),
        ["o/new", "o/stays", "o/leaves"],
        "it drains after the listed ones"
    );
}

#[test]
fn a_repository_added_back_while_it_drains_is_listed_afresh_and_drains_from_the_start_when_removed_again()
 {
    let mut repositories = draining("o/gone", "o/kept");
    repositories.runners_listed("o/gone", ANSWERED_CLEAN);

    repositories.reload(&names(&["o/gone", "o/kept"]));
    assert!(repositories.listed("o/gone").is_some(), "added back");
    assert!(
        !is_marked(&repositories, "o/gone"),
        "with nothing of before"
    );
    assert_eq!(repositories.runners_to_list(), ["o/gone", "o/kept"]);

    repositories.reload(&names(&["o/kept"]));
    assert!(
        !repositories.runners_listed("o/gone", ANSWERED_CLEAN),
        "its first drain's clean listing is forgotten"
    );
    assert!(repositories.runners_listed("o/gone", ANSWERED_CLEAN));
}

#[test]
fn a_reload_that_does_not_touch_a_drain_keeps_its_progress() {
    let mut repositories = draining("o/gone", "o/kept");
    repositories.runners_listed("o/gone", ANSWERED_CLEAN);
    repositories.reload(&names(&["o/kept"]));
    assert!(repositories.runners_listed("o/gone", ANSWERED_CLEAN));
}

#[tokio::test(start_paused = true)]
async fn a_reload_that_does_not_touch_a_drain_keeps_its_failure_clock() {
    let mut repositories = draining("o/gone", "o/kept");
    repositories.runners_listed("o/gone", FAILED);
    tokio::time::advance(Duration::from_mins(59)).await;
    repositories.reload(&names(&["o/kept"]));
    tokio::time::advance(Duration::from_mins(1)).await;
    assert!(repositories.runners_listed("o/gone", FAILED));
}

#[test]
fn a_running_runner_found_at_a_restart_drains_only_a_repository_nothing_else_has() {
    let mut repositories = draining("o/gone", "o/kept");
    repositories.runners_listed("o/gone", ANSWERED_CLEAN);
    mark(&mut repositories, "o/kept");

    repositories.found_running("o/kept");
    repositories.found_running("o/gone");
    repositories.found_running("o/found");

    assert!(
        is_marked(&repositories, "o/kept"),
        "still listed, as it was"
    );
    assert!(
        repositories.runners_listed("o/gone", ANSWERED_CLEAN),
        "a second runner found changes nothing of its drain"
    );
    assert!(repositories.is_known("o/found"), "now drains");
    assert!(
        repositories.listed("o/found").is_none(),
        "and is not listed"
    );
    assert_eq!(repositories.runners_to_list(), ["o/kept", "o/found"]);
}

/// What the two maps did before they became one value: a name is in at most one, which the model does not get for
/// free and `a_generated_sequence_behaves_as_the_two_maps_did` checks too.
#[derive(Default)]
struct Model {
    listed: BTreeSet<String>,
    draining: BTreeMap<String, ModelDrain>,
}

#[derive(Clone, Copy, Default)]
struct ModelDrain {
    clean_once: bool,
    failing_since: Option<Instant>,
}

impl Model {
    fn reload(&mut self, names: &BTreeSet<String>) {
        for name in self.listed.iter().filter(|name| !names.contains(*name)) {
            self.draining.entry(name.clone()).or_default();
        }
        self.draining.retain(|name, _| !names.contains(name));
        self.listed.clone_from(names);
    }

    fn found_running(&mut self, name: &str) {
        if !self.listed.contains(name) {
            self.draining.entry(name.to_owned()).or_default();
        }
    }

    fn runners_listed(&mut self, name: &str, listing: RunnersListing) -> bool {
        let Some(drain) = self.draining.get_mut(name) else {
            return false;
        };
        let ended = match listing {
            RunnersListing::RateLimited => {
                drain.failing_since = None;
                false
            }
            RunnersListing::Failed { gone } => {
                let since = *drain.failing_since.get_or_insert_with(Instant::now);
                gone || since.elapsed() >= DRAIN_FAILING_FOR
            }
            RunnersListing::Answered { left } => {
                drain.failing_since = None;
                if left {
                    drain.clean_once = false;
                    false
                } else if drain.clean_once {
                    true
                } else {
                    drain.clean_once = true;
                    false
                }
            }
        };
        if ended {
            self.draining.remove(name);
        }
        ended
    }

    fn runners_to_list(&self) -> Vec<String> {
        self.listed
            .iter()
            .chain(self.draining.keys())
            .cloned()
            .collect()
    }
}

/// Reloads, restarts' finds and every kind of listing, on names that come and go, an empty and an unknown one
/// among them. A restart begins again from the config, with what the runners found.
#[tokio::test(start_paused = true)]
async fn a_generated_sequence_behaves_as_the_two_maps_did() {
    const NAMES: [&str; 5] = ["a/first", "m/middle", "z/last", "", "o/never-configured"];
    const CONFIGURABLE: usize = 3;
    for seed in 1..=64 {
        let mut rng = Rng(seed);
        let mut repositories = Repositories::new(names(&NAMES[..CONFIGURABLE]));
        let mut model = Model {
            listed: names(&NAMES[..CONFIGURABLE]),
            ..Model::default()
        };
        for step in 0..200 {
            let name = NAMES[usize::try_from(rng.below(5)).unwrap()];
            match rng.below(9) {
                0 => {
                    let kept: BTreeSet<String> = NAMES[..CONFIGURABLE]
                        .iter()
                        .filter(|_| rng.below(2) == 0)
                        .map(|name| (*name).to_owned())
                        .collect();
                    repositories.reload(&kept);
                    model.reload(&kept);
                }
                1 => {
                    repositories.found_running(name);
                    model.found_running(name);
                }
                2 => {
                    let listed = model.listed.clone();
                    let found: Vec<&str> = model
                        .draining
                        .keys()
                        .map(String::as_str)
                        .filter(|_| rng.below(2) == 0)
                        .collect();
                    repositories = Repositories::new(listed.clone());
                    let mut restarted = Model {
                        listed,
                        ..Model::default()
                    };
                    for name in found {
                        repositories.found_running(name);
                        restarted.found_running(name);
                    }
                    model = restarted;
                }
                3 => tokio::time::advance(Duration::from_mins(25)).await,
                kind => {
                    let listing = match kind {
                        4 => RunnersListing::RateLimited,
                        5 => FAILED,
                        6 => GONE,
                        7 => ANSWERED_LEFT,
                        _ => ANSWERED_CLEAN,
                    };
                    assert_eq!(
                        repositories.runners_listed(name, listing),
                        model.runners_listed(name, listing),
                        "seed {seed} step {step}: {name:?} after {listing:?}"
                    );
                }
            }
            assert_eq!(
                repositories.runners_to_list(),
                model.runners_to_list(),
                "seed {seed} step {step}"
            );
            for name in NAMES {
                assert_eq!(
                    repositories.listed(name).is_some(),
                    model.listed.contains(name),
                    "seed {seed} step {step}: {name:?} is listed"
                );
                assert_eq!(
                    repositories.is_known(name),
                    model.listed.contains(name) || model.draining.contains_key(name),
                    "seed {seed} step {step}: {name:?} is known"
                );
                assert!(
                    !(model.listed.contains(name) && model.draining.contains_key(name)),
                    "seed {seed} step {step}: {name:?} in both maps"
                );
            }
        }
    }
}
