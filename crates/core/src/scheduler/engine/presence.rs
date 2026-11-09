//! Which repositories the engine serves or still cleans up after: each is listed by the config or draining, in one
//! value. It owns that value and a drain's rules, and must not call GitHub or log: `tick` lists the runners and words
//! what this decides.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use tokio::time::Instant;

use super::RepositoryState;

/// How long a removed repository's runners listings may fail in a row before it stops draining.
const DRAIN_FAILING_FOR: Duration = Duration::from_hours(1);

/// What a runners listing of a repository came to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RunnersListing {
    /// GitHub's rate limit stopped it, which says nothing of the repository.
    RateLimited,
    /// It failed; `gone` when GitHub says the repository is gone, as when the token lost it.
    Failed { gone: bool },
    /// GitHub answered; `left` when this machine still has a runner registered there.
    Answered { left: bool },
}

/// A repository a reload removed, polled until two runners listings in a row have none of this machine's runners: a
/// runner can go unseen in one listing whose pages shift while they are read.
#[derive(Debug, Default)]
struct Drain {
    /// Whether the last runners listing had none of this machine's runners. A listing that failed leaves it.
    clean_once: bool,
    /// Since when its runners listings fail, on Tokio's clock; `None` once one answers or the rate limit stops one.
    failing_since: Option<Instant>,
}

impl Drain {
    /// Takes in a runners listing and says whether the drain is over.
    fn on(&mut self, listing: RunnersListing) -> bool {
        match listing {
            RunnersListing::RateLimited => {
                self.failing_since = None;
                false
            }
            RunnersListing::Failed { gone } => {
                let since = *self.failing_since.get_or_insert_with(Instant::now);
                gone || since.elapsed() >= DRAIN_FAILING_FOR
            }
            RunnersListing::Answered { left } => {
                self.failing_since = None;
                if left {
                    self.clean_once = false;
                    return false;
                }
                let ended = self.clean_once;
                self.clean_once = true;
                ended
            }
        }
    }
}

/// One repository: the config lists it, or a reload removed it and it drains.
#[derive(Debug)]
enum Presence {
    Listed(RepositoryState),
    Draining(Drain),
}

impl Presence {
    fn listed() -> Self {
        Self::Listed(RepositoryState::default())
    }

    fn draining() -> Self {
        Self::Draining(Drain::default())
    }
}

/// The repositories the engine knows, by name, each listed or draining and never both.
///
/// On a restart the listed ones come from the config with nothing of their state, and the draining ones are those the
/// runners found still run for, starting their drain over. A reload lists what the new config has, with a repository
/// added back starting afresh, and drains what it lost, from the start. One listed before and after keeps its state,
/// and one draining before and after keeps its drain.
#[derive(Debug)]
pub(super) struct Repositories(BTreeMap<String, Presence>);

impl Repositories {
    /// The repositories of a config, all listed.
    pub(super) fn new(names: impl IntoIterator<Item = String>) -> Self {
        Self(
            names
                .into_iter()
                .map(|name| (name, Presence::listed()))
                .collect(),
        )
    }

    /// The state of `name`, if the config lists it.
    pub(super) fn listed(&self, name: &str) -> Option<&RepositoryState> {
        match self.0.get(name) {
            Some(Presence::Listed(state)) => Some(state),
            Some(Presence::Draining(_)) | None => None,
        }
    }

    /// The state of `name` to change, if the config lists it.
    pub(super) fn listed_mut(&mut self, name: &str) -> Option<&mut RepositoryState> {
        match self.0.get_mut(name) {
            Some(Presence::Listed(state)) => Some(state),
            Some(Presence::Draining(_)) | None => None,
        }
    }

    /// The repositories the config lists, by name.
    pub(super) fn iter_listed(&self) -> impl Iterator<Item = (&String, &RepositoryState)> {
        self.0.iter().filter_map(|(name, presence)| match presence {
            Presence::Listed(state) => Some((name, state)),
            Presence::Draining(_) => None,
        })
    }

    /// The repositories the config lists, by name, to change.
    pub(super) fn iter_listed_mut(
        &mut self,
    ) -> impl Iterator<Item = (&String, &mut RepositoryState)> {
        self.0
            .iter_mut()
            .filter_map(|(name, presence)| match presence {
                Presence::Listed(state) => Some((name, state)),
                Presence::Draining(_) => None,
            })
    }

    /// The names the config lists.
    pub(super) fn listed_names(&self) -> impl Iterator<Item = &String> {
        self.iter_listed().map(|(name, _)| name)
    }

    /// Whether the config lists `name` or it still drains.
    pub(super) fn is_known(&self, name: &str) -> bool {
        self.0.contains_key(name)
    }

    /// Those whose runners a tick lists: the listed ones by name, then the draining ones by name, which is the order
    /// GitHub is asked in.
    pub(super) fn runners_to_list(&self) -> Vec<String> {
        let draining = self.0.iter().filter_map(|(name, presence)| match presence {
            Presence::Listed(_) => None,
            Presence::Draining(_) => Some(name),
        });
        self.listed_names().chain(draining).cloned().collect()
    }

    /// Takes in the names a new config lists: see the type for what each repository becomes.
    pub(super) fn reload(&mut self, names: &BTreeSet<String>) {
        for (name, presence) in &mut self.0 {
            match (names.contains(name), &presence) {
                (true, Presence::Draining(_)) => *presence = Presence::listed(),
                (false, Presence::Listed(_)) => *presence = Presence::draining(),
                (true, Presence::Listed(_)) | (false, Presence::Draining(_)) => {}
            }
        }
        for name in names {
            self.0.entry(name.clone()).or_insert_with(Presence::listed);
        }
    }

    /// Drains `name` for a running runner a restart found, unless the config lists it or it already drains.
    pub(super) fn found_running(&mut self, name: &str) {
        self.0
            .entry(name.to_owned())
            .or_insert_with(Presence::draining);
    }

    /// Takes in a runners listing of `name`, and says whether that ended its drain. Only a draining repository cares:
    /// a listed or an unknown one is left as it is.
    pub(super) fn runners_listed(&mut self, name: &str, listing: RunnersListing) -> bool {
        let Some(Presence::Draining(drain)) = self.0.get_mut(name) else {
            return false;
        };
        let ended = drain.on(listing);
        if ended {
            self.0.remove(name);
        }
        ended
    }
}
