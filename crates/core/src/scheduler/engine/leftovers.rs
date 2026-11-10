//! The containers nothing occupies any more and that still have to go: which are waiting to be read, which to be
//! removed, and when each is asked about again. It owns that state and must not call Docker: `recover` and `runners`
//! do, and say here what Docker answered.

use std::collections::BTreeMap;
use std::time::Duration;

use tokio::time::Instant;

use super::Occupant;
use crate::scheduler::SlotRef;

/// The longest a container that could not be read waits before it is asked about again.
const UNREAD_BACKOFF_MAX: Duration = Duration::from_secs(600);

/// How long a container that could not be read `undecided` times waits before it is asked about again: one poll
/// interval, doubling each time, up to `UNREAD_BACKOFF_MAX`.
fn unread_backoff(poll_interval: Duration, undecided: u32) -> Duration {
    poll_interval
        .saturating_mul(1 << undecided.saturating_sub(1).min(16))
        .min(UNREAD_BACKOFF_MAX)
}

/// A stopped runner container an earlier run left, for which Docker has not yet told whether to keep it as its failed
/// job's record. Never removed in this state.
#[derive(Debug)]
pub(super) struct Unread {
    pub(super) slot: SlotRef,
    pub(super) occupant: Occupant,
    /// How many times Docker could not tell.
    undecided: u32,
}

impl Unread {
    /// One recovery found and has not asked about yet.
    pub(super) fn new(slot: SlotRef, occupant: Occupant) -> Unread {
        Unread {
            slot,
            occupant,
            undecided: 0,
        }
    }
}

/// A container to remove, as `Leftovers::to_remove` lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ToRemove {
    pub(super) id: String,
    pub(super) name: String,
}

/// What is still to do about one container.
#[derive(Debug)]
enum Left {
    /// To read at `due`, when its backoff is over; then to remove, unless it became a record.
    Unread { unread: Box<Unread>, due: Instant },
    /// To remove: each tick tries until Docker says it is gone.
    Removing { name: String },
}

/// The containers still to go, by ID: each is either to read or to remove, never both; only these methods write it.
///
/// A restart keeps none of it: `recover` finds each of them again in Docker's list. A reload keeps all of it.
#[derive(Debug, Default)]
pub(super) struct Leftovers(BTreeMap<String, Left>);

impl Leftovers {
    /// Leaves a container to be removed, whatever was still to do about it.
    pub(super) fn removing(&mut self, id: String, name: String) {
        self.0.insert(id, Left::Removing { name });
    }

    /// Docker could not tell about `unread` again: it waits out a longer backoff, which this returns.
    pub(super) fn undecided(&mut self, mut unread: Unread, poll_interval: Duration) -> Duration {
        unread.undecided += 1;
        let wait = unread_backoff(poll_interval, unread.undecided);
        self.0.insert(
            unread.occupant.container.clone(),
            Left::Unread {
                unread: Box::new(unread),
                due: Instant::now() + wait,
            },
        );
        wait
    }

    /// Takes those whose backoff is over at `now`, by ID, for the caller to ask Docker about.
    pub(super) fn take_due(&mut self, now: Instant) -> Vec<Unread> {
        let due: Vec<String> = self
            .0
            .iter()
            .filter(|(_, left)| matches!(left, Left::Unread { due, .. } if *due <= now))
            .map(|(id, _)| id.clone())
            .collect();
        due.iter()
            .filter_map(|id| match self.0.remove(id) {
                Some(Left::Unread { unread, .. }) => Some(*unread),
                _ => None,
            })
            .collect()
    }

    /// Those to remove now, by ID.
    pub(super) fn to_remove(&self) -> Vec<ToRemove> {
        self.0
            .iter()
            .filter_map(|(id, left)| match left {
                Left::Removing { name } => Some(ToRemove {
                    id: id.clone(),
                    name: name.clone(),
                }),
                Left::Unread { .. } => None,
            })
            .collect()
    }

    /// Docker says the container is gone.
    pub(super) fn removed(&mut self, id: &str) {
        self.0.remove(id);
    }
}

#[cfg(test)]
impl Leftovers {
    /// Whether `id` is to be removed.
    pub(super) fn is_removing(&self, id: &str) -> bool {
        matches!(self.0.get(id), Some(Left::Removing { .. }))
    }

    /// How many wait to be read.
    pub(super) fn unread(&self) -> usize {
        self.0
            .values()
            .filter(|left| matches!(left, Left::Unread { .. }))
            .count()
    }

    /// As if every backoff ran out.
    pub(super) fn make_due(&mut self) {
        let now = Instant::now();
        for left in self.0.values_mut() {
            if let Left::Unread { due, .. } = left {
                *due = now;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::SystemTime;

    use super::super::Engagement;
    use super::*;

    const POLL: Duration = Duration::from_secs(30);

    fn unread(id: &str) -> Unread {
        let occupant = Occupant {
            repository: "o/app".to_owned(),
            runner_name: format!("runner-{id}"),
            runner_id: 1,
            container: id.to_owned(),
            started_at: SystemTime::UNIX_EPOCH,
            memory: 0,
            cpus: 0,
            admission: None,
            labels: None,
            image: None,
            spec: None,
            engagement: Engagement::adopted(None),
        };
        let slot = SlotRef {
            profile: "small".to_owned(),
            index: 0,
        };
        Unread::new(slot, occupant)
    }

    #[test]
    fn a_container_is_to_read_or_to_remove_never_both() {
        let mut leftovers = Leftovers::default();
        leftovers.undecided(unread("a"), POLL);
        leftovers.removing("a".to_owned(), "runner-a".to_owned());
        assert_eq!(leftovers.unread(), 0, "removing ends the wait to read it");
        assert!(leftovers.is_removing("a"), "a is to remove");
        assert_eq!(leftovers.to_remove().len(), 1, "and listed once");
    }

    #[tokio::test(start_paused = true)]
    async fn each_time_docker_cannot_tell_the_wait_doubles_up_to_ten_minutes() {
        let mut leftovers = Leftovers::default();
        let mut waits = Vec::new();
        let mut left = unread("a");
        for _ in 0..7 {
            waits.push(leftovers.undecided(left, POLL).as_secs());
            leftovers.make_due();
            left = leftovers
                .take_due(Instant::now())
                .pop()
                .expect("just made due");
        }
        assert_eq!(waits, [30, 60, 120, 240, 480, 600, 600]);
    }

    #[tokio::test(start_paused = true)]
    async fn only_those_whose_backoff_is_over_are_taken_and_what_stays_is_not_to_remove() {
        let mut leftovers = Leftovers::default();
        leftovers.undecided(unread("soon"), POLL);
        tokio::time::advance(Duration::from_secs(1)).await;
        leftovers.undecided(unread("later"), POLL);
        leftovers.removing("gone".to_owned(), "runner-gone".to_owned());

        let at_its_end = Instant::now() + POLL - Duration::from_secs(1);
        let taken = leftovers.take_due(at_its_end);
        assert_eq!(taken.len(), 1, "the backoff of the later one is not over");
        assert_eq!(taken[0].occupant.container, "soon", "the earlier is");
        assert_eq!(leftovers.unread(), 1, "the later one stays to read");
        assert_eq!(
            leftovers.to_remove(),
            [ToRemove {
                id: "gone".to_owned(),
                name: "runner-gone".to_owned()
            }],
            "only what is to remove is listed so"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_taken_container_keeps_its_count_of_failures() {
        let mut leftovers = Leftovers::default();
        leftovers.undecided(unread("a"), POLL);
        leftovers.make_due();
        let taken = leftovers.take_due(Instant::now()).pop().expect("due");
        assert_eq!(leftovers.undecided(taken, POLL).as_secs(), 60, "its second");
    }

    #[test]
    fn a_removed_container_is_forgotten_and_an_unknown_one_changes_nothing() {
        let mut leftovers = Leftovers::default();
        assert!(leftovers.to_remove().is_empty(), "none yet");
        leftovers.removing("a".to_owned(), "runner-a".to_owned());
        leftovers.removed("unknown");
        assert!(leftovers.is_removing("a"), "another's removal leaves it");
        leftovers.removed("a");
        assert!(!leftovers.is_removing("a"), "a is gone");
    }
}
