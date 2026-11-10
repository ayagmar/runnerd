//! What each repository's listings may show queued that this machine's runners took (ADR 0018, 0095), by
//! repository. It owns that value and must not call GitHub or log: `tick` lists the jobs and plans from what is left.

use std::collections::BTreeMap;

use crate::github::QueuedJob;

/// By repository, the jobs its listings may show queued that this machine's runners took.
///
/// Nothing of it survives a restart. A repository's claims outlive its removal from the config, as a listing begun
/// before the removal may answer after it is added back, and end once neither the config nor a drain lists it
/// (`forget_unlisted`), as nothing lists it after that.
#[derive(Debug, Default)]
pub(super) struct Claims(BTreeMap<String, RepositoryClaims>);

/// What one repository's listings may show that this machine's runners took.
#[derive(Debug, Default)]
struct RepositoryClaims {
    /// Its runners gone that may have taken a listed job without the engine knowing which: one ended before its
    /// log capture named its job, or one whose start failed may have run. Each counts against one listed job of its
    /// pool until a listing begun after it went answers.
    unnamed: Vec<Unnamed>,
    /// The jobs this machine's runners took whose runners are gone, by the `listing` begun last when they went:
    /// GitHub lists a job queued for a while after a runner took it, so a listing begun before that, or one GitHub
    /// lags on, still shows it. A listing begun after that and showing it no longer drops it.
    consumed: BTreeMap<u64, u64>,
}

/// A runner gone that may have taken one of its repository's listed jobs, which no capture named.
#[derive(Debug)]
struct Unnamed {
    /// The profile of its pool.
    profile: String,
    /// The `Engine::listing` begun last when it went.
    listing: u64,
}

impl Claims {
    /// A runner of `repository`'s pool in `profile` that may have taken one of its listed jobs went, the engine not
    /// knowing which, while `listing` was the last listing begun.
    pub(super) fn spend(&mut self, repository: &str, profile: &str, listing: u64) {
        self.0
            .entry(repository.to_owned())
            .or_default()
            .spend(profile, listing);
    }

    /// A runner of this machine that took `job` of `repository` went, while `listing` was the last listing begun.
    pub(super) fn consume(&mut self, repository: &str, job: u64, listing: u64) {
        self.0
            .entry(repository.to_owned())
            .or_default()
            .consume(job, listing);
    }

    /// A listing of `repository` begun as `listing` answered with `queued`: the claims older than it end, but for
    /// the jobs it still shows; a repository left with none has no entry.
    pub(super) fn listing_answered(
        &mut self,
        repository: &str,
        listing: u64,
        queued: &[QueuedJob],
    ) {
        if let Some(claims) = self.0.get_mut(repository)
            && claims.answered(listing, queued) == Left::None
        {
            self.0.remove(repository);
        }
    }

    /// Forgets the claims of each repository `listed` no longer says the config or a drain lists.
    pub(super) fn forget_unlisted(&mut self, listed: impl Fn(&str) -> bool) {
        self.0.retain(|name, _| listed(name));
    }

    /// How many runners of `repository`'s pool in `profile` that went may have taken a listed job no capture named.
    pub(super) fn unnamed(&self, repository: &str, profile: &str) -> usize {
        self.0.get(repository).map_or(0, |claims| {
            claims
                .unnamed
                .iter()
                .filter(|claim| claim.profile == profile)
                .count()
        })
    }

    /// Whether a runner of this machine that went took `job` of `repository`.
    pub(super) fn consumed(&self, repository: &str, job: u64) -> bool {
        self.0
            .get(repository)
            .is_some_and(|claims| claims.consumed.contains_key(&job))
    }
}

/// Whether a repository has claims left after a listing answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Left {
    None,
    Some,
}

impl RepositoryClaims {
    fn spend(&mut self, profile: &str, listing: u64) {
        self.unnamed.push(Unnamed {
            profile: profile.to_owned(),
            listing,
        });
    }

    fn consume(&mut self, job: u64, listing: u64) {
        self.consumed.insert(job, listing);
    }

    fn answered(&mut self, listing: u64, queued: &[QueuedJob]) -> Left {
        self.unnamed.retain(|claim| claim.listing >= listing);
        self.consumed
            .retain(|job, since| *since >= listing || queued.iter().any(|q| q.id == *job));
        if self.unnamed.is_empty() && self.consumed.is_empty() {
            Left::None
        } else {
            Left::Some
        }
    }
}

#[cfg(test)]
impl Claims {
    pub(super) fn has(&self, repository: &str) -> bool {
        self.0.contains_key(repository)
    }

    /// How many runners of `repository`, of any pool, that went may have taken a listed job no capture named.
    pub(super) fn unnamed_of(&self, repository: &str) -> usize {
        self.0
            .get(repository)
            .map_or(0, |claims| claims.unnamed.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job(id: u64) -> QueuedJob {
        QueuedJob {
            id,
            ..QueuedJob::default()
        }
    }

    #[test]
    fn an_unnamed_claim_counts_against_its_own_pool_only() {
        let mut claims = Claims::default();
        claims.spend("o/r", "light", 1);
        assert_eq!(claims.unnamed("o/r", "light"), 1);
        assert_eq!(claims.unnamed("o/r", "heavy"), 0, "another pool of o/r");
        assert_eq!(claims.unnamed("o/s", "light"), 0, "another repository");
    }

    #[test]
    fn a_listing_begun_after_a_claim_ends_it_unless_it_still_shows_the_job() {
        let mut claims = Claims::default();
        claims.spend("o/r", "app", 1);
        claims.consume("o/r", 5, 1);
        claims.consume("o/r", 6, 1);
        claims.listing_answered("o/r", 1, &[]);
        assert_eq!(claims.unnamed("o/r", "app"), 1, "a listing begun with it");
        assert!(claims.consumed("o/r", 5), "a listing begun with it");

        claims.listing_answered("o/r", 2, &[job(6)]);
        assert_eq!(claims.unnamed("o/r", "app"), 0, "a later listing answered");
        assert!(
            !claims.consumed("o/r", 5),
            "a later listing no longer shows it"
        );
        assert!(claims.consumed("o/r", 6), "GitHub still lists it");

        claims.listing_answered("o/r", 3, &[]);
        assert!(!claims.has("o/r"), "nothing is left of o/r's claims");
    }

    #[test]
    fn claims_end_once_nothing_lists_their_repository() {
        let mut claims = Claims::default();
        claims.consume("o/r", 5, 1);
        claims.consume("o/s", 6, 1);
        claims.forget_unlisted(|name| name == "o/r");
        assert!(claims.consumed("o/r", 5), "o/r is listed");
        assert!(!claims.has("o/s"), "nothing lists o/s");
    }
}
