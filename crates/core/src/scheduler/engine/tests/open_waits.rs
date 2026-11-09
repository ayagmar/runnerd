//! Which wait a job is in: one at a time, each with its own start, against the rules two sets used to keep.

use std::collections::BTreeSet;

use super::invariants::Rng;
use crate::scheduler::engine::open_waits::{OpenWaits, WaitFor, Waited};

/// The rules with one set and one commit time per kind, as `OpenWaits` must go on answering them.
#[derive(Default)]
struct TwoSets {
    slot: (BTreeSet<u64>, u64),
    host: (BTreeSet<u64>, u64),
}

impl TwoSets {
    fn of(&mut self, kind: WaitFor) -> &mut (BTreeSet<u64>, u64) {
        match kind {
            WaitFor::Slot => &mut self.slot,
            WaitFor::Host => &mut self.host,
        }
    }

    fn end(&mut self, kind: WaitFor, jobs: &BTreeSet<u64>, now: u64) -> Option<Waited> {
        let (waiting, since) = self.of(kind);
        let ended: Vec<u64> = waiting.intersection(jobs).copied().collect();
        if ended.is_empty() {
            return None;
        }
        waiting.retain(|job| !jobs.contains(job));
        Some(Waited {
            kind,
            jobs: ended,
            ms: now.saturating_sub(*since),
        })
    }

    fn commit(&mut self, kind: WaitFor, waiting: BTreeSet<u64>, now: u64) -> Vec<Waited> {
        let other = match kind {
            WaitFor::Slot => WaitFor::Host,
            WaitFor::Host => WaitFor::Slot,
        };
        let moved = self.end(other, &waiting, now);
        let (waited, since) = std::mem::replace(self.of(kind), (waiting, now));
        let own = (!waited.is_empty()).then(|| Waited {
            kind,
            jobs: waited.into_iter().collect(),
            ms: now.saturating_sub(since),
        });
        moved.into_iter().chain(own).collect()
    }

    fn started(&mut self, job: u64, now: u64) -> Vec<Waited> {
        let jobs = BTreeSet::from([job]);
        [WaitFor::Slot, WaitFor::Host]
            .into_iter()
            .filter_map(|kind| self.end(kind, &jobs, now))
            .collect()
    }
}

fn waited(kind: WaitFor, jobs: &[u64], ms: u64) -> Waited {
    Waited {
        kind,
        jobs: jobs.to_vec(),
        ms,
    }
}

#[test]
fn a_wait_that_moves_to_the_host_and_back_counts_each_part_to_its_own_kind() {
    let job = BTreeSet::from([3]);
    let mut waits = OpenWaits::default();
    assert!(
        waits.open_wait(WaitFor::Slot, job.clone(), 10).is_empty(),
        "a new wait has nothing to count yet"
    );

    assert_eq!(
        waits.open_wait(WaitFor::Host, job.clone(), 25),
        [waited(WaitFor::Slot, &[3], 15)],
        "the slot wait ends where the host wait starts"
    );
    assert_eq!(waits.jobs(WaitFor::Slot), BTreeSet::new());
    assert_eq!(waits.jobs(WaitFor::Host), job);

    assert_eq!(
        waits.open_wait(WaitFor::Slot, job.clone(), 40),
        [waited(WaitFor::Host, &[3], 15)],
        "the host wait counts from its own start, not the first slot wait's"
    );
    assert_eq!(waits.jobs(WaitFor::Host), BTreeSet::new());
    assert_eq!(waits.jobs(WaitFor::Slot), job);

    assert_eq!(
        waits.end_wait(3, 50),
        Some(waited(WaitFor::Slot, &[3], 10)),
        "a start ends the wait it is in"
    );
    assert_eq!(waits.end_wait(3, 60), None, "and there is none left to end");
}

/// A set of the six jobs the sequences use, empty one time in four.
fn some_jobs(rng: &mut Rng) -> BTreeSet<u64> {
    if rng.below(4) == 0 {
        return BTreeSet::new();
    }
    (1..=6).filter(|_| rng.below(2) == 0).collect()
}

#[test]
fn any_sequence_of_commits_and_starts_records_what_the_two_sets_recorded() {
    for seed in 0..512 {
        let mut rng = Rng(seed);
        let (mut waits, mut sets) = (OpenWaits::default(), TwoSets::default());
        let mut now = 1000_u64;
        for _ in 0..40 {
            now = match rng.below(8) {
                0 => now,
                // The wall clock can step back.
                1 => now.saturating_sub(rng.below(50)),
                _ => now + rng.below(100),
            };
            match rng.below(10) {
                0..=3 => {
                    let jobs = some_jobs(&mut rng);
                    assert_eq!(
                        waits.open_wait(WaitFor::Slot, jobs.clone(), now),
                        sets.commit(WaitFor::Slot, jobs, now),
                        "seed {seed}: a commit of slot waits"
                    );
                }
                4..=7 => {
                    let jobs = some_jobs(&mut rng);
                    assert_eq!(
                        waits.open_wait(WaitFor::Host, jobs.clone(), now),
                        sets.commit(WaitFor::Host, jobs, now),
                        "seed {seed}: a commit of host waits"
                    );
                }
                8 => {
                    // Jobs 7 and 8 are never in a wait: GitHub can name one no plan had waiting.
                    let job = rng.below(8) + 1;
                    assert_eq!(
                        waits.end_wait(job, now).into_iter().collect::<Vec<_>>(),
                        sets.started(job, now),
                        "seed {seed}: job {job} started"
                    );
                }
                // A restart: no wait survives one.
                _ => (waits, sets) = (OpenWaits::default(), TwoSets::default()),
            }
            assert_eq!(waits.jobs(WaitFor::Slot), sets.slot.0, "seed {seed}");
            assert_eq!(waits.jobs(WaitFor::Host), sets.host.0, "seed {seed}");
        }
        for kind in [WaitFor::Slot, WaitFor::Host] {
            assert_eq!(
                waits.open_wait(kind, BTreeSet::new(), now + 7),
                sets.commit(kind, BTreeSet::new(), now + 7),
                "seed {seed}: every wait still open counts from its own start"
            );
        }
    }
}
