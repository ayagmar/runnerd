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
    replay_held(jobs, trace, most_credit, SLICE, Band::NoWaiterFits)
}

/// A replay under `memory_max`, whose larger margin `band` holds up.
fn replay_held(
    jobs: &[Ran],
    trace: &Trace,
    most_credit: u64,
    memory_max: u64,
    band: Band,
) -> Replay {
    let known = known();
    simulate(
        &config(),
        Some(memory_max),
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
            band,
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
            memory_max: Some(SLICE),
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

/// `jobs` on a 64G host whose other programs leave the runners `limit` by its plain margin of 4G, 2G less by the larger.
fn host_limited(jobs: &[Ran], limit: u64) -> Trace {
    let mut trace = trace_of(jobs);
    for reading in &mut trace.outside {
        reading.memory = 60 * G - limit;
    }
    trace
}

/// A job of a repository with no runs yet: it reserves what its profile's jobs do, 3.5G, more than these hosts leave.
fn report(queued: u64) -> Ran {
    run("halyard/other", "report", "jvm", queued, 30, 3 * G)
}

/// `check` from `NOW`, beside which a 7G limit leaves 3G; `report` waiting from minute 5; `page`, 2.5G, from minute 10,
/// which fits the plain margin and not the larger.
fn one_job_never_fits() -> (Vec<Ran>, Trace) {
    let jobs = vec![
        in_run(check(NOW, 30), 1, 1),
        in_run(report(NOW + 5 * MINUTE), 2, 1),
        in_run(page(NOW + 10 * MINUTE, 5), 3, 1),
    ];
    let trace = host_limited(&jobs, 7 * G);
    (jobs, trace)
}

#[test]
fn a_job_no_margin_fits_holds_up_the_larger_margin_until_a_waiter_fits_the_plain_one() {
    let (jobs, trace) = one_job_never_fits();
    let replay = replay_held(&jobs, &trace, 0, 40 * G, Band::NoWaiterFits);
    let waited = page_waited(&replay);
    assert!(
        (1..=30).contains(&waited),
        "refused by the larger margin, then it fits at the next plan, waited {waited}"
    );
    assert!(
        replay.hazard.in_band >= 20 * MINUTE,
        "the margin stood from minute 5 on: {:?}",
        replay.hazard
    );
}

#[test]
fn any_job_waiting_for_the_host_holds_up_the_larger_margin_when_a_replay_is_asked_to() {
    let (jobs, trace) = one_job_never_fits();
    let replay = replay_held(&jobs, &trace, 0, 40 * G, Band::AnyWaiter);
    assert_eq!(
        page_waited(&replay),
        50 * 60,
        "report starts into the empty slice at minute 30, and while anything waits the margin stands, so page waits it out"
    );
    assert!(
        replay.hazard.in_band >= 25 * MINUTE,
        "the margin stood from minute 5 on: {:?}",
        replay.hazard
    );
}

#[test]
fn a_replay_starts_at_the_plain_margin() {
    let jobs = vec![in_run(check(NOW, 30), 1, 1), in_run(page(NOW, 5), 2, 1)];
    let trace = host_limited(&jobs, 7 * G);
    let replay = replay_held(&jobs, &trace, 0, 40 * G, Band::NoWaiterFits);
    assert_eq!(
        page_waited(&replay),
        0,
        "page fits beside check by the plain margin at the first plan"
    );
}

#[test]
fn the_first_runner_into_an_empty_slice_needs_only_the_plain_margin() {
    let jobs = vec![
        in_run(check(NOW, 30), 1, 1),
        in_run(site(NOW + 5 * MINUTE, 30), 2, 1),
    ];
    // Beside the mirror a 5G limit leaves 4.5G by the plain margin, past site's 3.5G, and 2.5G by the larger.
    let trace = host_limited(&jobs, 5 * G);
    let replay = replay_held(&jobs, &trace, 0, 40 * G, Band::NoWaiterFits);
    assert_eq!(
        page_waited(&replay),
        25 * 60,
        "the plan that finds the slice empty starts it, though the larger margin stood"
    );
}

#[test]
fn a_host_credits_fall_is_judged_by_the_margin_that_stands() {
    let jobs = vec![
        in_run(check(NOW, 30), 1, 1),
        in_run(site(NOW, 30), 2, 1),
        in_run(page(NOW, 30), 3, 1),
        in_run(report(NOW + 3 * MINUTE), 4, 1),
    ];
    // Page comes to use all it takes at minute 8, which ends the host's 2G credit: 10G counted, up from 8G, under the
    // plain limit of 11G and past the 9G that stands while report waits.
    let mut trace = host_limited(&jobs, 11 * G);
    let page_run = trace.runs.get_mut(&RunId::of(&jobs[2])).unwrap();
    for point in &mut page_run.samples {
        if point.at >= 8 * MINUTE {
            point.memory = 2560 * M;
        }
    }
    let hazard = replay_held(&jobs, &trace, 0, 40 * G, Band::NoWaiterFits).hazard;
    assert_eq!(hazard.overdrawn, 1, "{hazard:?}");
}

#[test]
fn a_replays_credit_keeps_clear_of_the_larger_margin_while_it_stands() {
    let jobs = vec![
        in_run(check(NOW, 30), 1, 1),
        in_run(site(NOW, 30), 2, 1),
        in_run(report(NOW), 3, 1),
    ];
    // A 24G host with 10G available beside the two runners, and report waiting from the first plan: the credit is
    // what is left past the larger margin of 3G and a quarter of the host, 1G of the 1.75G their use would allow.
    let mut trace = trace_of(&jobs);
    trace.total = 24 * G;
    for reading in &mut trace.outside {
        reading.memory = 13 * G;
    }
    let hazard = replay_held(&jobs, &trace, MOST_CREDIT, 40 * G, Band::NoWaiterFits).hazard;
    assert_eq!(hazard.most_credit, G, "{hazard:?}");
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
