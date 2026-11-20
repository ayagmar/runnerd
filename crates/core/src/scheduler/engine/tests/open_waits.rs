//! Which wait a job is in: one at a time, each with its own start, against the rules two sets used to keep; and the
//! cause each job is held for, against a plain per-job model, without either disturbing the other.

use std::collections::{BTreeMap, BTreeSet};

use super::invariants::Rng;
use crate::api::WaitCause;
use crate::scheduler::engine::open_waits::{
    CAUSE_CHECKPOINT, CauseWaited, OpenWaits, WaitFor, Waited,
};

/// The cause rules one job at a time: each job's open cause and since when, the jobs runners took since the last
/// commit, and the time each job was counted for each cause.
#[derive(Default)]
struct PerJob {
    open: BTreeMap<u64, (WaitCause, u64)>,
    taken: BTreeSet<u64>,
    counted: BTreeMap<(u64, WaitCause), u64>,
}

impl PerJob {
    fn count(&mut self, job: u64, now: u64) -> Option<(WaitCause, u64)> {
        let (cause, since) = self.open.remove(&job)?;
        *self.counted.entry((job, cause)).or_insert(0) += now.saturating_sub(since);
        Some((cause, since))
    }

    fn hold(&mut self, held: &BTreeMap<u64, WaitCause>, going_on: &BTreeSet<u64>, now: u64) {
        let open: Vec<u64> = self.open.keys().copied().collect();
        for job in open {
            if let Some((cause, _)) = self.count(job, now)
                && going_on.contains(&job)
            {
                self.open.insert(job, (cause, now));
            }
        }
        for (job, cause) in held {
            if !self.taken.contains(job) {
                self.open.insert(*job, (*cause, now));
            }
        }
        self.taken.clear();
    }
}

/// Adds what `OpenWaits` said to record to `counted`.
fn add(counted: &mut BTreeMap<(u64, WaitCause), u64>, caused: Vec<CauseWaited>) {
    for CauseWaited { job, cause, ms } in caused {
        *counted.entry((job, cause)).or_insert(0) += ms;
    }
}

/// Some of the six jobs, each held for one of a few causes.
fn some_causes(rng: &mut Rng) -> BTreeMap<u64, WaitCause> {
    let causes = [WaitCause::Barrier, WaitCause::Host, WaitCause::Slot];
    some_jobs(rng)
        .into_iter()
        .map(|job| (job, causes[usize::try_from(rng.below(3)).unwrap()]))
        .collect()
}

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
fn any_sequence_of_commits_starts_and_causes_records_what_the_models_recorded() {
    for seed in 0..512 {
        let mut rng = Rng(seed);
        let (mut waits, mut sets) = (OpenWaits::default(), TwoSets::default());
        let (mut model, mut counted) = (PerJob::default(), BTreeMap::new());
        // The model counts each plan's stretch on its own; `OpenWaits` counts a cause that goes on as one stretch, cut
        // at checkpoints, and loses what it had not counted at a restart. With the clock monotonic and no restart they
        // agree; otherwise `OpenWaits` counts no more than the model.
        // Half the seeds step the clock back and restart; the other half keep both out, so the two must agree exactly.
        let rough = seed % 2 == 0;
        let mut exact = true;
        let mut now = 1000_u64;
        for _ in 0..60 {
            now = match rng.below(10) {
                0 => now,
                // The wall clock can step back.
                1 if rough => {
                    exact = false;
                    now.saturating_sub(rng.below(50))
                }
                // Past a checkpoint.
                2 => now + CAUSE_CHECKPOINT,
                _ => now + rng.below(100),
            };
            match rng.below(15) {
                0..=3 => {
                    // A job a runner took since the last commit of causes opens no wait.
                    let jobs = some_jobs(&mut rng);
                    let opened = &jobs - &model.taken;
                    assert_eq!(
                        waits.open_wait(WaitFor::Slot, jobs, now),
                        sets.commit(WaitFor::Slot, opened, now),
                        "seed {seed}: a commit of slot waits"
                    );
                }
                4..=7 => {
                    let jobs = some_jobs(&mut rng);
                    let opened = &jobs - &model.taken;
                    assert_eq!(
                        waits.open_wait(WaitFor::Host, jobs, now),
                        sets.commit(WaitFor::Host, opened, now),
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
                9..=11 => {
                    // A plan holds some jobs and starts runners for others; one may be both, which it holds.
                    let (held, going_on) = (some_causes(&mut rng), some_jobs(&mut rng));
                    add(&mut counted, waits.hold(&held, &going_on, now));
                    model.hold(&held, &going_on, now);
                }
                12 => {
                    // A runner that starts for a job a runner took already changes nothing.
                    let job = rng.below(8) + 1;
                    add(&mut counted, waits.booting(job, now).into_iter().collect());
                    if !model.taken.contains(&job) {
                        model.count(job, now);
                        model.open.insert(job, (WaitCause::Booting, now));
                    }
                }
                13 => {
                    // A failed start, or a take: a plan made before the take opens nothing for that job.
                    let job = rng.below(8) + 1;
                    let ended = if rng.below(2) == 0 {
                        waits.end_cause(job, now)
                    } else {
                        model.taken.insert(job);
                        waits.taken(job, now)
                    };
                    add(&mut counted, ended.into_iter().collect());
                    model.count(job, now);
                }
                // A restart: no wait survives one, and what was counted stays counted.
                _ if rough => {
                    (waits, sets) = (OpenWaits::default(), TwoSets::default());
                    model.open.clear();
                    model.taken.clear();
                    exact = false;
                }
                _ => {}
            }
            for job in 1..=8 {
                assert_eq!(
                    waits.cause(job),
                    model.open.get(&job).map(|(cause, _)| *cause),
                    "seed {seed}: job {job}'s cause"
                );
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
        add(
            &mut counted,
            waits.hold(&BTreeMap::new(), &BTreeSet::new(), now + 7),
        );
        model.hold(&BTreeMap::new(), &BTreeSet::new(), now + 7);
        assert_counted(seed, exact, counted, model.counted);
    }
}

/// What `OpenWaits` counted against the model: the same when `exact`, and never more.
fn assert_counted(
    seed: u64,
    exact: bool,
    mut counted: BTreeMap<(u64, WaitCause), u64>,
    mut model: BTreeMap<(u64, WaitCause), u64>,
) {
    counted.retain(|_, ms| *ms > 0);
    model.retain(|_, ms| *ms > 0);
    if exact {
        assert_eq!(
            counted, model,
            "seed {seed}: each job counts each stretch once, to the cause it was held for"
        );
    }
    for (key, ms) in &counted {
        assert!(
            model.get(key).is_some_and(|model| ms <= model),
            "seed {seed}: {key:?} counts {ms} ms, more than it waited: {:?}",
            model.get(key)
        );
    }
}

#[test]
fn a_cause_that_goes_on_is_counted_only_when_it_changes_ends_or_a_checkpoint_passes() {
    // Catches every plan writing every waiting job's row (ADR 0154): the same plan again writes nothing until the
    // checkpoint, which counts every open cause once.
    let mut waits = OpenWaits::default();
    let held = BTreeMap::from([(1, WaitCause::Host), (2, WaitCause::Slot)]);
    let none = BTreeSet::new();
    let start = 10 * CAUSE_CHECKPOINT;
    assert_eq!(waits.hold(&held, &none, start), []);
    for step in 1..10 {
        assert_eq!(
            waits.hold(&held, &none, start + step * 1000),
            [],
            "an identical plan writes nothing"
        );
    }
    let checkpoint = start + CAUSE_CHECKPOINT;
    let counted = |ms| {
        vec![
            CauseWaited {
                job: 1,
                cause: WaitCause::Host,
                ms,
            },
            CauseWaited {
                job: 2,
                cause: WaitCause::Slot,
                ms,
            },
        ]
    };
    assert_eq!(
        waits.hold(&held, &none, checkpoint),
        counted(CAUSE_CHECKPOINT)
    );
    assert_eq!(waits.hold(&held, &none, checkpoint + 1000), []);
    // Job 2 moves behind a barrier: only its stretch is written.
    let moved = BTreeMap::from([(1, WaitCause::Host), (2, WaitCause::Barrier)]);
    assert_eq!(
        waits.hold(&moved, &none, checkpoint + 2000),
        [CauseWaited {
            job: 2,
            cause: WaitCause::Slot,
            ms: 2000
        }]
    );
}
