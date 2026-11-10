//! The growth brake: it engages when what ran grew past the room a look kept, holds for its time, releases, and starts
//! engaged after a restart.

use std::time::{Duration, SystemTime};

use crate::admission::growth::{BRAKE_FOR, Braked, GROWTH_WINDOW, Growth, Guard, Use};

const G: u64 = 1 << 30;

fn at(seconds: u64) -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000 + seconds)
}

/// What runners 1 to 3 use, by `memory` each in GiB.
fn using(memory: [u64; 3]) -> Vec<Use<u64>> {
    (1..)
        .zip(memory)
        .map(|(runner, memory)| Use {
            runner,
            memory: memory * G,
        })
        .collect()
}

#[test]
fn growth_past_the_room_a_look_kept_engages_the_brake_until_its_time_is_up() {
    let mut growth = Growth::default();
    growth.look(at(0), using([1, 1, 1]), 6 * G, Guard::Clear);
    growth.look(at(60), using([3, 3, 1]), 6 * G, Guard::Clear);
    assert_eq!(
        growth.braked(at(60)),
        None,
        "4G of growth is within 6G kept"
    );
    growth.look(at(120), using([4, 3, 3]), 6 * G, Guard::Clear);
    assert_eq!(
        growth.braked(at(120)),
        Some(Braked::Grew {
            until: at(120) + BRAKE_FOR,
            grew: 7 * G,
            kept: 6 * G,
        }),
        "7G since the first look, past its 6G"
    );
    growth.look(at(180), using([4, 3, 3]), 6 * G, Guard::Clear);
    assert_eq!(
        growth.braked(at(180)).map(Braked::until),
        Some(at(180) + BRAKE_FOR),
        "the first look is still in the window: the growth since it holds the brake up"
    );
    let quiet = 180 + GROWTH_WINDOW.as_secs() + 1;
    growth.look(at(quiet), using([4, 3, 3]), 6 * G, Guard::Clear);
    let until = at(180) + BRAKE_FOR;
    assert!(
        growth.braked(until - Duration::from_secs(1)).is_some(),
        "it stands for its time"
    );
    assert_eq!(growth.braked(until), None, "and then releases");
    growth.look(until, using([4, 3, 3]), 6 * G, Guard::Clear);
    assert_eq!(growth.braked(until), None, "no new growth engages it again");
}

#[test]
fn growth_counts_only_looks_inside_the_window_and_runners_present_at_both() {
    let mut growth = Growth::default();
    growth.look(at(0), using([1, 1, 1]), 2 * G, Guard::Clear);
    let past = GROWTH_WINDOW.as_secs() + 1;
    growth.look(at(past), using([3, 3, 1]), 2 * G, Guard::Clear);
    assert_eq!(
        growth.braked(at(past)),
        None,
        "the first look fell out of the window"
    );
    // Runner 4 starts at 9G and runner 1 ends: neither is growth of what was there.
    let mut later = using([0, 3, 2]);
    later.remove(0);
    later.push(Use {
        runner: 4,
        memory: 9 * G,
    });
    growth.look(at(past + 60), later, 2 * G, Guard::Clear);
    assert_eq!(
        growth.braked(at(past + 60)),
        None,
        "1G grew of the runners at both looks"
    );
    growth.look(
        at(past + 120),
        [2, 3, 4].map(|runner| Use {
            runner,
            memory: 5 * G,
        }),
        2 * G,
        Guard::Clear,
    );
    assert!(
        growth.braked(at(past + 120)).is_some(),
        "runners 2 and 3 grew 5G past the 2G kept"
    );
}

#[test]
fn a_runner_that_ended_hides_no_growth() {
    let mut growth = Growth::default();
    growth.look(at(0), using([4, 1, 1]), 2 * G, Guard::Clear);
    // Runner 1 ends with its 4G; runners 2 and 3 grow 3G together.
    growth.look(
        at(60),
        [2, 3].map(|runner| Use {
            runner,
            memory: 5 * G / 2,
        }),
        2 * G,
        Guard::Clear,
    );
    assert!(growth.braked(at(60)).is_some(), "3G grew past the 2G kept");
}

#[test]
fn shrinking_runners_offset_growing_ones() {
    let mut growth = Growth::default();
    growth.look(at(0), using([1, 5, 1]), 3 * G, Guard::Clear);
    growth.look(at(60), using([5, 1, 1]), 3 * G, Guard::Clear);
    assert_eq!(growth.braked(at(60)), None, "together they did not grow");
}

#[test]
fn a_restart_counts_plainly_until_a_window_of_looks_can_judge_growth() {
    let mut growth = Growth::restarted(at(0));
    growth.look(at(0), using([1, 1, 1]), 6 * G, Guard::Clear);
    let full = GROWTH_WINDOW.as_secs();
    assert_eq!(
        growth.braked(at(full - 1)),
        Some(Braked::Restarted { until: at(full) })
    );
    growth.look(at(full), using([1, 1, 1]), 6 * G, Guard::Clear);
    assert_eq!(growth.braked(at(full)), None, "the window is full");
}

#[test]
fn growth_during_a_restart_brakes_for_the_whole_time() {
    let mut growth = Growth::restarted(at(0));
    growth.look(at(0), using([1, 1, 1]), G, Guard::Clear);
    growth.look(at(60), using([2, 2, 2]), G, Guard::Clear);
    assert_eq!(
        growth.braked(at(60)).map(Braked::until),
        Some(at(60) + BRAKE_FOR)
    );
}
