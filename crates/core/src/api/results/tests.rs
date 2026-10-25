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
