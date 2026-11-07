//! A replay by a `Trace`: each run uses what its samples say, is known when the daemon knew it, and the pooled count
//! reads what the daemon's would at that moment.

use super::*;
use crate::admission::pooled::{self, MOST_CREDIT};
use crate::admission::trace::{Outside, Point, Reading, RunId, RunTrace, STEP, Trace};

/// A slice with 1.5G left of two 3.5G reservations and the 512M mirror, under the mark with room for what the credit
/// adds; a host with memory to spare.
const SLICE: u64 = 8 * G + G / 2;

/// The memory of `check` and `site`, 3.5G each once known, as they use it.
const USED: u64 = G / 2;

/// A sample of `memory` every `STEP` for `minutes`.
fn flat(memory: u64, minutes: u64) -> Vec<Point> {
    (0..minutes * MINUTE / STEP)
        .map(|step| Point {
            at: step * STEP,
            memory,
        })
        .collect()
}

fn check(queued: u64, minutes: u64) -> Ran {
    run("halyard/ledger", "check", "jvm", queued, minutes, 3 * G)
}

fn site(queued: u64, minutes: u64) -> Ran {
    run("halyard/storefront", "site", "node", queued, minutes, 3 * G)
}

fn page(queued: u64, minutes: u64) -> Ran {
    run("halyard/storefront", "page", "node", queued, minutes, 2 * G)
}

/// Ten earlier runs of each job, so the replay predicts them from their own.
fn known() -> History {
    let before = NOW - 900 * MINUTE;
    let jobs = (0..10)
        .flat_map(|i| {
            let at = before + i * 40 * MINUTE;
            [check(at, 30), site(at, 30), page(at, 5)]
        })
        .collect();
    History {
        jobs,
        ..History::default()
    }
}

/// `check` and `site` from `NOW` for half an hour, and `page` from `page_at`.
fn jobs(page_at: u64) -> Vec<Ran> {
    let mut jobs = vec![
        in_run(check(NOW, 30), 1, 1),
        in_run(site(NOW, 30), 2, 1),
        in_run(page(page_at, 5), 3, 1),
    ];
    jobs.sort_by_key(|run| run.queued_at);
    jobs
}

/// Every one of `jobs` using `USED` from the start, on a host that keeps well clear of its guards.
fn trace_of(jobs: &[Ran]) -> Trace {
    let runs = jobs
        .iter()
        .map(|run| {
            let trace = RunTrace {
                samples: flat(USED, 40),
                named_after: 0,
            };
            (RunId::of(run), trace)
        })
        .collect();
    Trace {
        runs,
        outside: vec![
            Outside {
                at: NOW - MINUTE,
                memory: 8 * G,
            },
            Outside {
                at: NOW + 60 * MINUTE,
                memory: 8 * G,
            },
        ],
        total: 64 * G,
        overhead: 0,
        guard_overhead: 0,
        host_overhead: 0,
    }
}

fn replay(jobs: &[Ran], trace: &Trace, most_credit: u64) -> Replay {
    let known = known();
    simulate(
        &config(),
        Some(SLICE),
        None,
        jobs,
        Policy::Measured,
        &BTreeMap::new(),
        Replaying {
            barriers: Barriers::On,
            arrivals: Arrivals::AsQueued,
            known: Some(&known),
            trace: Some(trace),
            most_credit,
            host_reserve: HostReserve::Top2,
            credit_room: CreditRoom::Counted,
            band: Band::NoWaiterFits,
        },
    )
}

/// How long `page` waited, in seconds.
fn page_waited(replay: &Replay) -> u64 {
    let pool = pool("halyard/storefront", "node");
    *replay.pools[&pool].waits.last().unwrap()
}

#[test]
fn a_pooled_replay_starts_what_only_the_pooled_count_fits() {
    let jobs = jobs(NOW + 5 * MINUTE);
    let trace = trace_of(&jobs);
    let plain = replay(&jobs, &trace, 0);
    assert_eq!(page_waited(&plain), 25 * 60, "until one of the two ends");
    assert_eq!(plain.hazard.most_credit, 0);
    let pooled = replay(&jobs, &trace, MOST_CREDIT);
    assert!(page_waited(&pooled) <= 15, "at the next plan");
    let (check, site, page) = (Key::of(&jobs[0]), Key::of(&jobs[1]), Key::of(&jobs[2]));
    let runner = |key, takes| pooled::Runner {
        key: Some(key),
        takes,
        used: Some(USED),
        ran: Some(pooled::POOL_AFTER),
        basis: Some(Basis::Job {
            runs: 10,
            peak: JobPeak::P95,
        }),
    };
    let direct = pooled::count(&pooled::Snapshot {
        runners: &[
            runner(&check, 3584 * M),
            runner(&site, 3584 * M),
            runner(&page, 2560 * M),
        ],
        committed: 0,
        slice: Some(pooled::Slice {
            memory: 3 * USED,
            memory_max: SLICE,
        }),
        host: Some(pooled::Host {
            available: 50 * G,
            keeps: 20 * G,
            stalled: false,
        }),
        most: MOST_CREDIT,
        host_reserve: true,
    });
    assert_eq!(
        pooled.hazard.most_credit, direct.credit,
        "the replay's count is the daemon's"
    );
}

#[test]
fn a_replayed_run_counts_in_full_for_its_first_minute() {
    let jobs = jobs(NOW + 20_000);
    let trace = trace_of(&jobs);
    let waited = page_waited(&replay(&jobs, &trace, MOST_CREDIT));
    assert!(
        (40..=60).contains(&waited),
        "from 20 s to a plan past the minute, waited {waited}"
    );
}

#[test]
fn a_replayed_run_is_not_pooled_before_the_daemon_could_name_its_job() {
    let jobs = jobs(NOW + 5 * MINUTE);
    let mut trace = trace_of(&jobs);
    let check = RunId::of(&jobs[0]);
    trace.runs.get_mut(&check).unwrap().named_after = 10 * MINUTE;
    let waited = page_waited(&replay(&jobs, &trace, MOST_CREDIT));
    assert!(
        (300..=320).contains(&waited),
        "until check is known at minute 10, waited {waited}"
    );
}

#[test]
fn a_sampler_gap_takes_the_credit_back_and_the_replay_says_so() {
    let jobs = jobs(NOW + 5 * MINUTE);
    let mut trace = trace_of(&jobs);
    let check = RunId::of(&jobs[0]);
    let run = trace.runs.get_mut(&check).unwrap();
    run.samples
        .retain(|point| !(8 * MINUTE..9 * MINUTE).contains(&point.at));
    let hazard = replay(&jobs, &trace, MOST_CREDIT).hazard;
    assert!(hazard.credit_falls >= 1, "the gap took it back: {hazard:?}");
}

#[test]
fn a_credit_the_host_takes_back_leaves_the_count_past_the_budget_and_the_replay_says_so() {
    let jobs = jobs(NOW + 5 * MINUTE);
    let mut trace = trace_of(&jobs);
    // At minute 8, with page running in what the credit left, the desktop takes what the host kept.
    trace.outside.insert(
        1,
        Outside {
            at: NOW + 8 * MINUTE,
            memory: 43 * G,
        },
    );
    let hazard = replay(&jobs, &trace, MOST_CREDIT).hazard;
    assert_eq!(hazard.overdrawn, 1, "{hazard:?}");
}

#[test]
fn a_host_short_of_memory_keeps_a_replay_from_pooling() {
    let jobs = jobs(NOW + 5 * MINUTE);
    let mut trace = trace_of(&jobs);
    for reading in &mut trace.outside {
        // 21G available, under the margin and a quarter of the host with the two runners' 1G.
        reading.memory = 43 * G;
    }
    let replay = replay(&jobs, &trace, MOST_CREDIT);
    assert!(replay.hazard.most_credit < G, "{:?}", replay.hazard);
    assert_eq!(page_waited(&replay), 25 * 60);
}

#[test]
fn a_replay_finds_the_slice_past_its_lines_and_how_long() {
    let jobs = jobs(NOW + 40 * MINUTE);
    let mut trace = trace_of(&jobs);
    let check = RunId::of(&jobs[0]);
    let run = trace.runs.get_mut(&check).unwrap();
    for point in &mut run.samples {
        if (10 * MINUTE..11 * MINUTE).contains(&point.at) {
            point.memory = 9 * G;
        }
    }
    let hazard = replay(&jobs, &trace, 0).hazard;
    assert_eq!(hazard.over_mark.count, 1, "{hazard:?}");
    assert_eq!(hazard.over_max.count, 1, "{hazard:?}");
    assert_eq!(hazard.over_max.time, MINUTE);
    assert_eq!(hazard.over_max.worst, 9 * G + USED - SLICE);
    assert_eq!(hazard.slice_peak, 9 * G + USED);
}

#[test]
fn a_cold_window_gives_every_run_its_jobs_heaviest_samples() {
    let first = in_run(check(NOW, 30), 1, 1);
    let second = in_run(check(NOW + MINUTE, 30), 2, 1);
    let other = in_run(site(NOW, 30), 3, 1);
    let jobs = [first, second, other];
    let mut trace = trace_of(&jobs);
    trace.runs.get_mut(&RunId::of(&jobs[1])).unwrap().samples = flat(4 * G, 1);
    let cold = trace.cold(&jobs);
    let peak = |at: usize| cold.runs[&RunId::of(&jobs[at])].at(0);
    assert_eq!(peak(0), Reading::Used(4 * G), "the job's heaviest run");
    assert_eq!(peak(1), Reading::Used(4 * G));
    assert_eq!(peak(2), Reading::Used(USED), "another job keeps its own");
}

#[test]
fn a_cold_run_shorter_than_the_heaviest_reaches_its_peak_in_the_time_it_has() {
    // A job's short runs are most of them: a peak past their end would leave the stress with nothing to stress.
    let long = in_run(check(NOW, 30), 1, 1);
    let short = in_run(check(NOW + MINUTE, 5), 2, 1);
    let jobs = [long, short];
    let mut trace = trace_of(&jobs);
    let heavy = &mut trace.runs.get_mut(&RunId::of(&jobs[0])).unwrap().samples;
    heavy
        .iter_mut()
        .find(|point| point.at == 20 * MINUTE)
        .unwrap()
        .memory = 4 * G;
    let cold = trace.cold(&jobs);
    let lasted = jobs[1].finished_at.unwrap() - jobs[1].started_at.unwrap();
    let run = &cold.runs[&RunId::of(&jobs[1])];
    let at = run.samples.iter().find(|point| point.memory == 4 * G);
    assert!(
        at.is_some_and(|point| point.at < lasted),
        "the peak falls inside its {lasted} ms: {at:?}"
    );
    assert_eq!(run.at(0), Reading::Used(USED), "it reads from its start");
}

#[test]
fn a_shifted_host_keeps_every_reading_and_comes_round_the_window() {
    let trace = Trace {
        outside: [(0, 1), (10, 2), (20, 3), (30, 4)]
            .map(|(at, memory)| Outside { at, memory })
            .to_vec(),
        ..Trace::default()
    };
    let shifted = trace.shifted(10);
    assert_eq!(shifted.outside_at(0), Some(4), "what ended the window");
    assert_eq!(shifted.outside_at(10), Some(1));
    assert_eq!(shifted.outside_at(30), Some(3));
    assert_eq!(shifted.outside.len(), 4);
}

#[test]
fn a_slice_that_reads_low_at_a_decision_goes_past_the_mark_it_did_not_see() {
    // The hazard is read at what the slice holds, the guards at what a sample showed them.
    let jobs = jobs(NOW + 40 * MINUTE);
    let mut trace = trace_of(&jobs);
    trace.overhead = 8 * G;
    trace.guard_overhead = 0;
    let replay = replay(&jobs, &trace, 0);
    assert!(replay.hazard.over_mark.count > 0, "{:?}", replay.hazard);
    assert_eq!(replay.held_back, 0, "the guards saw room");
}

#[test]
fn a_run_between_samples_reads_the_last_and_a_gap_reads_nothing() {
    let run = RunTrace {
        samples: vec![
            Point {
                at: 1000,
                memory: G,
            },
            Point {
                at: 3000,
                memory: 2 * G,
            },
            Point {
                at: 30_000,
                memory: 3 * G,
            },
        ],
        named_after: 0,
    };
    assert_eq!(run.at(0), Reading::NotYet);
    assert_eq!(run.at(2000), Reading::Used(G));
    assert_eq!(run.at(9000), Reading::Used(2 * G), "at the edge of fresh");
    assert_eq!(run.at(9001), Reading::Stale);
    assert_eq!(run.at(90_000), Reading::Used(3 * G), "its end is not a gap");
}
