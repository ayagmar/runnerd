//! What every client says the sizing advice means: what it sizes for, how it reads, and what auto sizing does next.

use crate::api::{
    AdmissionPolicy, Advice, AutoPlan, CheckStatus, Peaks, ProfilePlan, ProfileSizing, Sizing,
    SizingHold, Spread, Starts, Tone, need,
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
        },
        wait: Spread::default(),
        starts: Starts::default(),
        busy: 0,
        wants_slots: false,
        advice,
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
