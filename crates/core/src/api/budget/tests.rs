//! What the overview says of the budget and of a running job's limits, the same in every client.

use crate::api::{
    AdmissionPolicy, Budget, ClosedKind, HostBudget, HostMemory, Job, JobMetrics, JobRunner,
    JobState, JobSummary, LimitSource, LogSummary, Pooled, ProfileStatus, RunnerStatus, SeedMemory,
    SlotStatus, Status, Swap, Timeline,
};

const G: u64 = 1 << 30;

fn runner(reserved: u64) -> RunnerStatus {
    RunnerStatus {
        repository: "halyard/ledger".to_owned(),
        name: "runnerd-jvm-1".to_owned(),
        runner_id: 1,
        container: "c1".to_owned(),
        busy: true,
        started_at: 0,
        reserved,
        warm: false,
    }
}

/// `jvm` with two 8G slots, the first running, in a slice of `memory_max`.
fn status(admission: AdmissionPolicy, memory_max: Option<u64>) -> Status {
    Status {
        version: "0.0.0".to_owned(),
        shutting_down: false,
        stopping: None,
        budget: Budget {
            slice: "ci.slice".to_owned(),
            memory_max,
            memory_in_use: 8 * G,
            pooled: None,
            admission,
            ..Budget::default()
        },
        profiles: vec![ProfileStatus {
            name: "jvm".to_owned(),
            image: "runnerd-runner:base".to_owned(),
            cpus: 4,
            memory: 8 * G,
            reseeding: false,
            slots: vec![
                SlotStatus {
                    slot: 1,
                    runner: Some(runner(8 * G)),
                },
                SlotStatus {
                    slot: 2,
                    runner: None,
                },
            ],
        }],
        repositories: vec![],
        queued: vec![],
    }
}

#[test]
fn a_free_slot_under_caps_is_drawn_but_stays_free_beside_what_runs() {
    let status = status(AdmissionPolicy::Caps, Some(24 * G));
    let parts = status.budget_parts();
    assert_eq!(status.budget.remaining(), Some(16 * G));
    assert_eq!(
        parts
            .slots
            .iter()
            .map(|part| (part.slot, part.reserved))
            .collect::<Vec<_>>(),
        [(1, true), (2, false)]
    );
    let free = parts.free.expect("8G past both slots");
    assert_eq!(free.memory, 8 * G);
    assert_eq!(
        free.reading.line(),
        "8G of 24G free past every slot's cap; 16G is free beside what runs"
    );
}

#[test]
fn measured_admission_draws_only_runners_so_free_is_what_is_left() {
    let parts = status(AdmissionPolicy::Measured, Some(24 * G)).budget_parts();
    assert_eq!(parts.slots.len(), 1);
    assert_eq!(
        parts.free.map(|free| free.reading.line()),
        Some("16G of 24G free".to_owned())
    );
}

#[test]
fn without_memory_max_the_bar_has_room_for_the_mirror_and_seeds() {
    let mut status = status(AdmissionPolicy::Caps, None);
    status.budget.mirror_memory = G;
    status.budget.seed_memory = vec![SeedMemory {
        profile: "jvm".to_owned(),
        memory: 8 * G,
    }];
    let parts = status.budget_parts();
    assert_eq!(parts.whole, 25 * G, "two slots, the mirror and a seed");
    assert!(parts.free.is_none(), "nothing is free without a MemoryMax");
    assert_eq!(
        parts
            .held
            .iter()
            .map(super::HeldPart::name)
            .collect::<Vec<_>>(),
        ["mirror", "jvm seed"]
    );
}

#[test]
fn every_client_says_the_held_measured_budget_admits_no_runner() {
    let mut budget = status(AdmissionPolicy::Measured, Some(24 * G)).budget;
    budget.held = true;
    assert_eq!(
        budget.line(),
        "8G of 24G reserved in ci.slice by predicted peaks; the slice is past 90% of its MemoryMax, so no \
         runner starts"
    );
    budget.memory_max = None;
    budget.held = false;
    assert_eq!(
        budget.line(),
        "8G reserved in ci.slice by predicted peaks, which has no MemoryMax"
    );
}

#[test]
fn a_host_near_the_edge_lacks_room_even_for_a_job_that_would_fit() {
    // Catches `host-held` staying silent however long a small job waits on a host down to 6G, and a VM that can still
    // grow to its ceiling reading as short because it shrank while idle.
    let at = |total: u64, available: u64, ceiling: Option<u64>| HostBudget {
        memory: HostMemory {
            total,
            available,
            ceiling,
            ..host(0, false).memory
        },
        ..host(0, false)
    };
    // 62G keeps a 3.875G margin, so twice that is 7.75G.
    let edge = 7 * G + 768 * (1 << 20);
    assert!(
        at(62 * G, 6 * G, None).short_of(G),
        "2.125G of room is under one margin"
    );
    assert!(
        at(62 * G, edge - 1, None).short_of(G),
        "just under the line"
    );
    assert!(
        !at(62 * G, edge, None).short_of(G),
        "on the line, and the job fits"
    );
    assert!(
        at(62 * G, edge, None).short_of(4 * G),
        "the job does not fit"
    );
    assert!(
        !at(62 * G, 42 * G, None).short_of(7 * G),
        "the sandbox's 42G"
    );
    assert!(
        !at(2 * G, G + G / 2, Some(24 * G)).short_of(G),
        "1.5G now, and 22G more to come"
    );
}

/// A reading of a 64G host that leaves the slice `limit`, so `spare` beside the 8G running.
fn host(limit: u64, stalled: bool) -> HostBudget {
    HostBudget {
        memory: HostMemory {
            at: 0,
            total: 64 * G,
            available: 20 * G,
            slice: 8 * G,
            some: None,
            full: None,
            holders: vec![],
            ceiling: None,
            swap: None,
        },
        limit,
        spare: limit.saturating_sub(8 * G),
        stalled,
    }
}

#[test]
fn free_is_what_the_host_leaves_when_it_leaves_less_than_memory_max() {
    let mut status = status(AdmissionPolicy::Measured, Some(46 * G));
    status.budget.host = Some(host(13 * G, false));
    let parts = status.budget_parts();
    assert_eq!(
        parts.free.map(|free| (free.memory, free.reading.line())),
        Some((
            5 * G,
            "5G free; the host leaves the slice 13G of its 46G".to_owned()
        ))
    );
    assert_eq!(
        parts
            .closed
            .map(|closed| (closed.memory, closed.reading.line())),
        Some((
            33 * G,
            "33G held by the host for its other programs and page cache, which leave the slice 13G"
                .to_owned()
        ))
    );
    assert_eq!(
        status.budget.host_line().as_deref(),
        Some("host: 20G of 64G available; leaves the slice 13G of its 46G, so 5G is free")
    );
}

#[test]
fn the_host_holds_back_the_end_of_the_bar_past_idle_slots_under_caps() {
    let mut status = status(AdmissionPolicy::Caps, Some(24 * G));
    // 12G beside the running slot: the host holds back 4G of the 8G past both slots.
    status.budget.host = Some(host(20 * G, false));
    let parts = status.budget_parts();
    assert_eq!(
        parts.free.map(|free| free.reading.line()),
        Some(
            "4G free past every slot's cap; 12G is free beside what runs, as the host leaves the slice 20G of its \
             24G"
                .to_owned()
        )
    );
    assert_eq!(parts.closed.map(|closed| closed.memory), Some(4 * G));
    // 2G beside the running slot: less than the idle slot's 8G cap, so the host holds all 8G past both slots.
    status.budget.host = Some(host(10 * G, false));
    let parts = status.budget_parts();
    assert_eq!(parts.free, None, "nothing past the idle slot is free");
    assert_eq!(parts.closed.map(|closed| closed.memory), Some(8 * G));
    // Only what runs: past it the host leaves nothing.
    status.budget.host = Some(host(8 * G, false));
    assert_eq!(
        status.budget.host_line().as_deref(),
        Some("host: 20G of 64G available; leaves the slice 8G of its 24G, so nothing is free")
    );
}

#[test]
fn a_host_that_leaves_as_much_changes_nothing() {
    let plain = status(AdmissionPolicy::Measured, Some(24 * G));
    let mut status = plain.clone();
    status.budget.host = Some(host(30 * G, false));
    let parts = status.budget_parts();
    assert_eq!(parts.free, plain.budget_parts().free);
    assert_eq!(parts.closed, None);
    assert_eq!(status.budget.host_withheld(), None);
}

#[test]
fn nothing_is_free_while_the_host_stalls() {
    let mut status = status(AdmissionPolicy::Caps, Some(24 * G));
    status.budget.host = Some(host(30 * G, true));
    let parts = status.budget_parts();
    assert_eq!(parts.free, None, "no runner starts, so nothing is free");
    let closed = parts.closed.expect("the room past both slots is closed");
    assert_eq!(
        (closed.memory, closed.kind, closed.reading.line()),
        (
            8 * G,
            ClosedKind::Stalled,
            "8G closed: the host's programs stall waiting for memory, so no runner starts"
                .to_owned()
        )
    );
    assert_eq!(
        status.budget.host_line().as_deref(),
        Some("host: 20G of 64G available; stalling, so no runner starts")
    );
}

#[test]
fn nothing_is_free_while_the_slice_is_held_though_the_host_leaves_room() {
    let mut status = status(AdmissionPolicy::Measured, Some(46 * G));
    status.budget.held = true;
    status.budget.host = Some(host(13 * G, false));
    let parts = status.budget_parts();
    assert_eq!(parts.free, None, "no runner starts, so nothing is free");
    let closed = parts.closed.expect("the room past the runner is closed");
    assert_eq!(
        (closed.memory, closed.kind, closed.reading.line()),
        (
            38 * G,
            ClosedKind::Held,
            "38G closed: the slice is past 90% of its MemoryMax, so no runner starts".to_owned()
        )
    );
    assert_eq!(
        status.budget.host_line().as_deref(),
        Some("host: 20G of 64G available; leaves the slice 13G")
    );
}

#[test]
fn a_pooled_budget_reads_its_count_and_says_what_was_taken_off() {
    let mut status = status(AdmissionPolicy::Measured, Some(46 * G));
    status.budget.memory_in_use = 42 * G;
    status.budget.pooled = Some(Pooled {
        credit: 6 * G,
        counted: 36 * G,
        runners: 9,
    });
    let reading = status.budget.memory_reading();
    assert_eq!(reading.figure, "36G");
    assert_eq!(
        reading.text,
        "of 46G counted in ci.slice by predicted peaks: 42G reserved, 6G pooled"
    );
    assert_eq!(
        status.budget.remaining(),
        Some(10 * G),
        "what a runner fits in"
    );
    let free = status.budget_parts().free.expect("past the runner's block");
    assert_eq!(
        (free.memory, free.reading.text),
        (38 * G, "of 46G free, and pooling adds 6G".to_owned()),
        "the blocks stay what runners reserve"
    );
}

#[test]
fn a_host_limit_past_memory_max_is_not_said_to_leave_the_slice_more_than_it_has() {
    // Catches `leaves the slice 47.46G of its 46G`: a credit lets the room `MemoryMax` leaves pass what the host's
    // limit, which reads the plain count, leaves beside what runs.
    let mut status = status(AdmissionPolicy::Measured, Some(12 * G));
    status.budget.memory_in_use = 12 * G;
    status.budget.pooled = Some(Pooled {
        credit: 4 * G,
        counted: 8 * G,
        runners: 2,
    });
    status.budget.host = Some(HostBudget {
        spare: G,
        ..host(13 * G, false)
    });
    assert_eq!(status.budget.host_withheld(), Some(3 * G));
    assert_eq!(
        status.budget.host_line().as_deref(),
        Some(
            "host: 20G of 64G available; leaves the slice 13G for the 12G reserved there, so 1G is \
             free"
        )
    );
    let parts = status.budget_parts();
    let free = parts.free.expect("what the host leaves");
    assert_eq!(
        free.reading.line(),
        "1G free; the host leaves the slice 13G for the 12G reserved there, and pooling adds 4G"
    );
    // With 20G available, the host's programs are not what holds this back.
    let held = parts.closed.expect("what the plain count holds back");
    assert_eq!(held.kind, ClosedKind::Host);
    assert_eq!(
        held.reading.line(),
        "3G held back by what runs reserves: the host leaves the slice 13G for the 12G reserved \
         there"
    );
}

#[test]
fn without_memory_max_the_host_line_says_what_the_host_leaves_free() {
    let mut budget = status(AdmissionPolicy::Measured, None).budget;
    budget.host = Some(host(13 * G, false));
    assert_eq!(
        budget.host_line().as_deref(),
        Some("host: 20G of 64G available; leaves the slice 13G, so 5G is free")
    );
}

#[test]
fn the_host_line_says_how_much_swap_is_used_and_nothing_on_a_host_without() {
    // Catches a swap clause on a host without swap, and the used figure read as the free one.
    let mut budget = status(AdmissionPolicy::Measured, Some(13 * G)).budget;
    let mut reading = host(13 * G, false);
    reading.memory.swap = Some(Swap {
        total: 15 * G + G / 2,
        free: 3 * G + G / 2,
    });
    budget.host = Some(reading);
    assert_eq!(
        budget.host_line().as_deref(),
        Some("host: 20G of 64G available, swap 12G of 15.5G used; leaves the slice 13G")
    );
    // Swap is never counted: the room is as without it.
    let mut without = budget.clone();
    if let Some(host) = &mut without.host {
        host.memory.swap = None;
    }
    assert_eq!(
        without.host_line().as_deref(),
        Some("host: 20G of 64G available; leaves the slice 13G")
    );
}

fn job(memory_cap: Option<u64>, cpu_cap: Option<u64>) -> Job {
    Job {
        summary: JobSummary {
            id: 7,
            repository: "halyard/ledger".to_owned(),
            run_id: 1,
            run_attempt: 1,
            workflow: None,
            name: "build".to_owned(),
            branch: None,
            url: None,
            labels: vec![],
            state: JobState::Running,
            conclusion: None,
            runner: Some(JobRunner {
                name: "runnerd-jvm-1".to_owned(),
                profile: "jvm".to_owned(),
                slot: 1,
                memory_cap,
                cpu_cap,
            }),
            timeline: Timeline::default(),
            metrics: JobMetrics::default(),
            log: LogSummary::default(),
        },
        steps: vec![],
        record: None,
        admission: None,
    }
}

#[test]
fn a_reload_never_replaces_the_cap_a_running_container_started_with() {
    // The container started with 8G; the profile was reloaded to 4G and 2 CPUs. The record has no CPU cap.
    let mut profile = status(AdmissionPolicy::Caps, None).profiles.remove(0);
    profile.memory = 4 * G;
    profile.cpus = 2;
    let limits = job(Some(8 * G), None).limits(Some(&profile));
    let memory = limits.memory.expect("a memory limit");
    assert_eq!((memory.value, memory.source), (8 * G, LimitSource::Cap));
    let cpu = limits.cpu.expect("a CPU limit");
    assert_eq!((cpu.value, cpu.source), (2000, LimitSource::ProfileNow));
    assert_eq!(
        limits.line().as_deref(),
        Some("capped at 8G; profile now 2 CPUs")
    );
    assert_eq!(
        job(Some(8 * G), Some(4000)).limits(None).line().as_deref(),
        Some("capped at 8G and 4 CPUs")
    );
}
