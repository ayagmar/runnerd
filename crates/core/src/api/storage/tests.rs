//! What every client says of caches, the build cache and records.

use crate::api::{
    BuildCacheCounts, BuildCacheStats, CacheKind, CacheVolume, Record, RecordLimits, RecordNote,
    RecordRemoval, RecordState, Records, Tone, records_near_full,
};

const G: u64 = 1 << 30;

#[test]
fn a_snapshot_past_its_limit_starts_over_at_its_reseed_and_a_slot_is_emptied() {
    let volume = |kind| CacheVolume {
        name: "v".to_owned(),
        kind,
        bytes: 6 * G,
        limit: Some(5 * G),
    };
    let snapshot = volume(CacheKind::Snapshot {
        profile: "jvm".to_owned(),
    })
    .limit_status()
    .unwrap();
    assert_eq!(snapshot.label, "started over at its next reseed above 5G");
    assert_eq!(snapshot.tone(), Tone::Warn, "past its limit");
    let slot = volume(CacheKind::Slot {
        profile: "jvm".to_owned(),
        slot: 1,
    })
    .limit_status()
    .unwrap();
    assert_eq!(slot.label, "emptied above 5G");
    assert_eq!(
        volume(CacheKind::Build).limit_status().unwrap().label,
        "trimmed to 5G"
    );
}

#[test]
fn a_job_that_only_stored_build_cache_entries_still_shows_it() {
    let writer = BuildCacheCounts {
        written_bytes: G,
        writer: true,
        ..BuildCacheCounts::default()
    };
    let activity = writer.activity().expect("it stored an entry");
    assert_eq!(activity.line(), "no lookups, 0B served, 1G stored");
    let refused = BuildCacheCounts {
        refused: 2,
        ..BuildCacheCounts::default()
    };
    assert_eq!(
        refused
            .activity()
            .map(|activity| activity.line())
            .as_deref(),
        Some("no lookups, 0B served, read only, 2 uploads refused")
    );
    assert!(BuildCacheCounts::default().activity().is_none());
}

#[test]
fn a_week_without_lookups_still_says_what_was_stored_and_refused() {
    let stats = |week| BuildCacheStats {
        bytes: G,
        entries: 120,
        limit: 5 * G,
        week,
    };
    let stored_and_refused = BuildCacheCounts {
        written_bytes: G,
        refused: 2,
        writer: true,
        ..BuildCacheCounts::default()
    };
    assert_eq!(
        stats(stored_and_refused).week_line(),
        "120 entries; no lookups in 7 days, 1G stored, 2 uploads refused"
    );
    assert_eq!(
        stats(BuildCacheCounts::default()).week_line(),
        "120 entries; no lookups in 7 days"
    );
}

#[test]
fn records_near_max_total_say_so_wherever_they_are_listed() {
    assert!(records_near_full(8 * G, 10 * G), "80% is near");
    assert!(!records_near_full(8 * G - 1, 10 * G));
    let record = |job, bytes| Record {
        job,
        repository: "o/app".to_owned(),
        profile: "jvm".to_owned(),
        workflow: None,
        name: None,
        created_at: 0,
        bytes,
        image: format!("runnerd-record:{job}"),
    };
    let records = Records {
        records: vec![record(1, 4 * G), record(2, 5 * G)],
        limits: RecordLimits {
            max_total: 10 * G,
            max_count: 20,
            max_age: 7 * 86_400,
            min_free: 0,
        },
        shells: vec![2],
    };
    assert_eq!(
        records.summary(),
        "2 of 20 records, 9G of 10G, each kept 7 days; near max_total, so new ones remove the oldest"
    );
    assert_eq!(records.summary_tone(), Tone::Warn);
    assert!(
        records.shell_open(2) && !records.shell_open(1),
        "a shell is open in job 2's record"
    );
}

#[test]
fn a_skipped_record_is_never_kept_and_a_removed_one_never_offers_a_shell() {
    let skipped = RecordNote {
        skipped: Some("too little free disk".to_owned()),
        bytes: Some(G),
        ..RecordNote::default()
    };
    assert_eq!(
        skipped.state(),
        RecordState::Skipped {
            why: "too little free disk"
        }
    );
    let removed = RecordNote {
        created_at: Some(1),
        removed_at: Some(9),
        removed_by: Some(RecordRemoval::Age),
        ..RecordNote::default()
    };
    assert_eq!(removed.state().line(), "removed past max_age");
    let kept = RecordNote {
        created_at: Some(1),
        ..RecordNote::default()
    };
    assert_eq!(
        kept.state(),
        RecordState::Kept { bytes: None },
        "kept though its size is unknown"
    );
}
