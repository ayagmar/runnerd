//! What each repository's listings may show queued that this machine's runners took (ADR 0018, 0095), by
//! repository. It owns that value and must not call GitHub or log: `tick` lists the jobs and plans from what is left.

use std::collections::BTreeMap;
use std::time::Duration;

use super::engagement::Mark;
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
    /// pool until a listing that ends its mark answers (`Mark::ends`): one begun at least `poll_interval` after the
    /// listing begun last when it went.
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
    /// The listing begun last when it went.
    went: Mark,
}

impl Claims {
    /// A runner of `repository`'s pool in `profile` that may have taken one of its listed jobs went while `went` was the
    /// listing begun last, the engine not knowing which.
    pub(super) fn spend(&mut self, repository: &str, profile: &str, went: Mark) {
        self.0
            .entry(repository.to_owned())
            .or_default()
            .spend(profile, went);
    }

    /// A runner of this machine that took `job` of `repository` went, while `listing` was the last listing begun.
    pub(super) fn consume(&mut self, repository: &str, job: u64, listing: u64) {
        self.0
            .entry(repository.to_owned())
            .or_default()
            .consume(job, listing);
    }

    /// A listing of `repository` begun at `listing` answered with `queued`: the unnamed claims it ends end
    /// (`Mark::ends`, by `floor`), and the consumed ones older than it but for the jobs it still shows; a repository
    /// left with none has no entry.
    pub(super) fn listing_answered(
        &mut self,
        repository: &str,
        listing: Mark,
        floor: Duration,
        queued: &[QueuedJob],
    ) {
        if let Some(claims) = self.0.get_mut(repository)
            && claims.answered(listing, floor, queued) == Left::None
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
    fn spend(&mut self, profile: &str, went: Mark) {
        self.unnamed.push(Unnamed {
            profile: profile.to_owned(),
            went,
        });
    }

    fn consume(&mut self, job: u64, listing: u64) {
        self.consumed.insert(job, listing);
    }

    fn answered(&mut self, listing: Mark, floor: Duration, queued: &[QueuedJob]) -> Left {
        self.unnamed
            .retain(|claim| !listing.ends(claim.went, floor));
        self.consumed
            .retain(|job, since| *since >= listing.listing || queued.iter().any(|q| q.id == *job));
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

    /// As if every runner whose claim is unnamed went `by` earlier.
    pub(super) fn age(&mut self, by: Duration) {
        for claim in self.0.values_mut().flat_map(|claims| &mut claims.unnamed) {
            if let Some(at) = claim.went.at.checked_sub(by) {
                claim.went.at = at;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use tokio::time::Instant;

    use super::*;

    const FLOOR: Duration = Duration::from_secs(15);

    fn job(id: u64) -> QueuedJob {
        QueuedJob {
            id,
            ..QueuedJob::default()
        }
    }

    /// The listing begun as `listing`, `secs` after `start`.
    fn mark(start: Instant, listing: u64, secs: u64) -> Mark {
        Mark {
            listing,
            at: start + Duration::from_secs(secs),
        }
    }

    #[test]
    fn an_unnamed_claim_counts_against_its_own_pool_only() {
        let mut claims = Claims::default();
        claims.spend("o/r", "light", mark(Instant::now(), 1, 0));
        assert_eq!(claims.unnamed("o/r", "light"), 1);
        assert_eq!(claims.unnamed("o/r", "heavy"), 0, "another pool of o/r");
        assert_eq!(claims.unnamed("o/s", "light"), 0, "another repository");
    }

    #[test]
    fn a_listing_begun_after_a_claim_ends_it_unless_it_still_shows_the_job() {
        let start = Instant::now();
        let mut claims = Claims::default();
        claims.spend("o/r", "app", mark(start, 1, 0));
        claims.consume("o/r", 5, 1);
        claims.consume("o/r", 6, 1);
        claims.listing_answered("o/r", mark(start, 1, 15), FLOOR, &[]);
        assert_eq!(claims.unnamed("o/r", "app"), 1, "a listing begun with it");
        assert!(claims.consumed("o/r", 5), "a listing begun with it");

        claims.listing_answered("o/r", mark(start, 2, 15), FLOOR, &[job(6)]);
        assert_eq!(claims.unnamed("o/r", "app"), 0, "a later listing answered");
        assert!(
            !claims.consumed("o/r", 5),
            "a later listing no longer shows it"
        );
        assert!(claims.consumed("o/r", 6), "GitHub still lists it");

        claims.listing_answered("o/r", mark(start, 3, 30), FLOOR, &[]);
        assert!(!claims.has("o/r"), "nothing is left of o/r's claims");
    }

    #[test]
    fn an_unnamed_claim_outlives_listings_sooner_than_a_poll_interval_after_the_one_before_its_runner_went()
     {
        // The runner went while listing 1 was the last begun; o/r is listed every 5 s while active.
        let start = Instant::now();
        let mut claims = Claims::default();
        claims.spend("o/r", "app", mark(start, 1, 0));
        for (listing, secs) in [(2, 5), (3, 10)] {
            claims.listing_answered("o/r", mark(start, listing, secs), FLOOR, &[]);
            assert_eq!(
                claims.unnamed("o/r", "app"),
                1,
                "listing {listing}, {secs} s after listing 1, is within GitHub's lag"
            );
        }
        claims.listing_answered("o/r", mark(start, 4, 15), FLOOR, &[]);
        assert_eq!(
            claims.unnamed("o/r", "app"),
            0,
            "the first listing a poll interval after listing 1 ends it"
        );
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
