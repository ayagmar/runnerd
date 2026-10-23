//! The wire format of the sizing advice against readers already released: 0.0.7's `Sizing` reads what this one
//! writes, and this one reads what 0.0.7's daemon wrote.

use serde::Deserialize;

use super::{ProfileSizing, Resize, Sizing, SizingChange};
use crate::api::AdmissionPolicy;

/// `Sizing` as 0.0.7 shipped it.
#[derive(Debug, Deserialize)]
struct ReleasedSizing {
    since: u64,
    until: u64,
    margin: u32,
    sample_interval: u64,
    min_jobs: u32,
    memory_max: Option<u64>,
    profiles: Vec<ProfileSizing>,
}

fn sizing() -> Sizing {
    Sizing {
        since: 1_700_000_000,
        until: 1_700_086_400,
        margin: 15,
        sample_interval: 2,
        min_jobs: 10,
        memory_max: Some(33 << 30),
        admission: AdmissionPolicy::Caps,
        profiles: Vec::new(),
        auto: true,
        changes: vec![SizingChange {
            at: 1_700_050_000,
            profile: "node".to_owned(),
            from: Resize {
                memory: 7 << 30,
                slots: 2,
            },
            to: Resize {
                memory: 5376 << 20,
                slots: 2,
            },
            reason: "lower memory to 2.25G from 7G, its largest peak + 15%".to_owned(),
        }],
    }
}

#[test]
fn a_released_reader_reads_advice_with_auto_sizing_changes_as_before() {
    let written = serde_json::to_string(&sizing()).unwrap();
    let read: ReleasedSizing = serde_json::from_str(&written).unwrap();
    assert_eq!(
        (read.since, read.until, read.margin, read.sample_interval),
        (1_700_000_000, 1_700_086_400, 15, 2)
    );
    assert_eq!((read.min_jobs, read.memory_max), (10, Some(33 << 30)));
    assert!(read.profiles.is_empty(), "no profile is made up");
}

#[test]
fn advice_from_a_released_daemon_reads_as_auto_sizing_off_with_no_changes() {
    let written = r#"{"since":1700000000,"until":1700086400,"margin":15,"sample_interval":2,"min_jobs":10,"memory_max":null,"profiles":[]}"#;
    let read: Sizing = serde_json::from_str(written).unwrap();
    assert!(!read.auto, "a released daemon has no auto sizing");
    assert_eq!(read.changes, Vec::new());
}
