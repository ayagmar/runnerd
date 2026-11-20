//! What every client reports an action did.

use crate::api::{Paused, RemovedRecords, Resize, RunningJob};

#[test]
fn a_forced_pause_names_the_runners_it_stopped() {
    let paused = Paused {
        stopped: vec![RunningJob {
            repository: "o/app".to_owned(),
            runner: "runnerd-jvm-1".to_owned(),
            job: Some(42),
        }],
    };
    assert_eq!(
        paused.line("o/app"),
        "o/app paused; stopped job 42 on runnerd-jvm-1"
    );
    assert_eq!(
        Paused { stopped: vec![] }.line("o/app"),
        "o/app paused: it gets no new runners"
    );
}

#[test]
fn removing_no_record_says_so_rather_than_freeing_nothing() {
    assert_eq!(RemovedRecords::default().line(), "no record removed");
}

#[test]
fn a_written_resize_names_its_cpus_only_when_it_changes_them() {
    let resize = Resize {
        memory: 6 << 30,
        slots: 2,
        cpus: None,
    };
    assert_eq!(
        resize.written_line("jvm"),
        "jvm: memory 6G, 2 slots written; the daemon runs with them"
    );
    let cpus = Resize {
        cpus: Some(8),
        ..resize
    };
    assert_eq!(
        cpus.written_line("jvm"),
        "jvm: memory 6G, 2 slots, 8 cpus written; the daemon runs with them"
    );
}

/// A reseed that started a layer over goes on to seed the others, so its last line would hide the start-over.
#[test]
fn a_reseed_reports_its_start_over_rather_than_its_last_seeded_layer() {
    let started_tools_over = "the snapshot would hold 10931923458 bytes, over its limit of 10737418240: \
        started tools over from the image, keeping 9131923458 bytes\nseeded cache: 7.3G\nseeded android: 299M\n";
    assert_eq!(
        crate::api::reseed_outcome(started_tools_over),
        "the snapshot would hold 10931923458 bytes, over its limit of 10737418240: \
        started tools over from the image, keeping 9131923458 bytes"
    );
    // What an older image's seed script prints, a whole start-over, still reads as one.
    let whole = "the snapshot would hold 11445207681 bytes, over its limit of 10737418240: started over from the image";
    assert_eq!(crate::api::reseed_outcome(whole), whole);
    assert_eq!(
        crate::api::reseed_outcome("seeded cache: 5.2G\nseeded tools: 1.7G\nseeded android: 0"),
        "seeded android: 0"
    );
    assert_eq!(crate::api::reseed_outcome("  \n"), "");
}
