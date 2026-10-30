//! What every client says the sizing advice means: what it sizes for, how it reads, and what auto sizing does next.

use crate::api::{
    AdmissionPolicy, Advice, AutoPlan, CheckStatus, Peaks, ProfilePlan, ProfileSizing, Resize,
    Sizing, SizingChange, SizingHold, SizingRefused, Spread, Starts, Tone, need,
};

const G: u64 = 1 << 30;

fn profile(name: &str, max: u64, advice: Advice) -> ProfileSizing {
    ProfileSizing {
        name: name.to_owned(),
        memory: 4 * G,
        cpus: 4,
        slots: 2,
        peaks: Peaks {
            jobs: 3,
            at_cap: 0,
            memory: Spread {
                p50: max,
                p95: max,
                p99: max,
                max,
            },
            cpu: Spread::default(),
            cpu_runs: 0,
            on_cap: 0,
        },
        wait: Spread::default(),
        starts: Starts::default(),
        busy: 0,
        wants_slots: false,
        advice,
        revert: None,
        jobs: vec![],
    }
}

fn sizing(profiles: Vec<ProfileSizing>) -> Sizing {
    Sizing {
        since: 1_000,
        until: 2_000,
        margin: 15,
        sample_interval: 2,
        min_jobs: 10,
        memory_max: Some(64 * G),
        admission: AdmissionPolicy::Caps,
        profiles,
        auto: true,
        changes: vec![],
        auto_plan: None,
    }
}

#[test]
fn too_few_runs_are_marked_at_what_the_advice_would_size_for() {
    // A 1G peak + 15% is 1.15G, but advice keeps 1G for page cache and rounds to 256M: 2G.
    let profile = profile("jvm", G, Advice::TooFew { jobs: 3 });
    let marked = sizing(vec![]).need(&profile.peaks, profile.advice);
    assert_eq!(marked.memory, 2 * G);
    assert_eq!(marked.memory, need(G, 15), "the daemon's own arithmetic");
    assert_eq!(marked.reason, "its largest peak + 1G for page cache");
    assert_eq!(need(10 * G, 15), 11776 << 20, "11.5G, a multiple of 256M");
}

#[test]
fn a_hold_is_judged_by_the_readers_clock_not_by_when_the_plan_was_made() {
    let plan = AutoPlan {
        every: 300,
        interval: 86_400,
        raise_interval: 3_600,
        max_step: 25,
        next: vec![],
        held: vec![SizingHold {
            profile: "jvm".to_owned(),
            changed: 0,
            until: 3_600,
            lower_until: 86_400,
        }],
        blocked: None,
        refused: Vec::new(),
    };
    assert!(
        matches!(plan.profile_plan("jvm", 60), ProfilePlan::Held { .. }),
        "an hour holds every change"
    );
    let later = plan.profile_plan("jvm", 7_200);
    assert!(
        matches!(later, ProfilePlan::LowerHeld { .. }),
        "past the raise interval, only a lowering waits"
    );
    assert_eq!(later.until(), Some(86_400));
    assert_eq!(later.headline(&plan), "No lowering");
    assert_eq!(
        plan.profile_plan("jvm", 90_000),
        ProfilePlan::Nothing,
        "a hold that ended holds nothing"
    );
}

#[test]
fn a_cpu_raise_auto_sizing_cannot_make_shows_beside_the_memory_change_it_makes() {
    let plan = AutoPlan {
        every: 300,
        interval: 86_400,
        raise_interval: 3_600,
        max_step: 25,
        next: vec![SizingChange {
            at: 0,
            profile: "jvm".to_owned(),
            from: Resize {
                memory: 9 * G,
                slots: 2,
                cpus: None,
            },
            to: Resize {
                memory: 10 * G,
                slots: 2,
                cpus: None,
            },
            reason: "raise memory".to_owned(),
            uncertain: false,
        }],
        held: Vec::new(),
        blocked: None,
        refused: vec![SizingRefused {
            profile: "jvm".to_owned(),
            reason: "3 runs of halyard/ledger CI / test averaged 7.9 of its 8 CPUs (median): the cap holds them \
                     back; 9 CPUs for each of its 2 slots do not fit the CPU budget beside the other profiles'"
                .to_owned(),
        }],
    };
    assert!(
        matches!(plan.profile_plan("jvm", 60), ProfilePlan::Next { .. }),
        "the memory change is due"
    );
    assert_eq!(
        plan.refusal("jvm").as_deref(),
        Some(
            "No CPU raise: 3 runs of halyard/ledger CI / test averaged 7.9 of its 8 CPUs (median): the cap holds \
             them back; 9 CPUs for each of its 2 slots do not fit the CPU budget beside the other profiles'"
        ),
        "and why its cpus cannot rise, beside it"
    );
    assert_eq!(plan.refusal("node"), None);
}

#[test]
fn doctor_warns_only_when_a_profile_needs_more_than_its_cap() {
    let lower = profile(
        "node",
        G,
        Advice::Lower {
            memory: 2 * G,
            slots: 2,
        },
    );
    let check = sizing(vec![lower.clone()]).doctor_check();
    assert_eq!(check.status, CheckStatus::Info, "{check:?}");
    assert_eq!(lower.advice.tone(), Tone::Warn);
    let raise = profile(
        "jvm",
        6 * G,
        Advice::Raise {
            memory: 7 * G,
            slots: 2,
        },
    );
    let check = sizing(vec![lower, raise.clone()]).doctor_check();
    assert_eq!(check.status, CheckStatus::Warn, "{check:?}");
    assert_eq!(raise.advice.tone(), Tone::Bad, "red in every client");
}

#[test]
fn cpu_peaks_say_which_runs_they_are_over_and_none_reads_as_none_not_zero() {
    // A profile cut to 9 CPUs: its recent runs ran at 31, none yet at 9. Every client shows no CPU p99 and says so.
    let mut peaks = profile("jvm", G, Advice::Keep { memory: G }).peaks;
    assert_eq!(peaks.cpu_p99(), None);
    assert_eq!(peaks.cpu_runs_line(9), "no run at 9 CPUs yet");
    peaks.cpu_runs = 4;
    peaks.cpu.p99 = 9160;
    assert_eq!(peaks.cpu_p99(), Some(9160));
    assert_eq!(peaks.cpu_runs_line(9), "of the 4 at 9 CPUs");
}
