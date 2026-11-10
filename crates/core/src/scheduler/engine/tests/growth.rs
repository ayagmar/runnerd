//! The growth count on the fakes (ADR 0130): it rests a window after a start, a reload that changes what it reads and
//! below the guard's line, keeps room for the three largest growths and every young runner's, and its brake engages
//! when what runs grew past that room.

use super::admission::{
    first_minute_passes, history_of, host_at, measured, roomy_readings, runners_use, slice_at,
};
use super::*;
use crate::admission::growth::{BRAKE_FOR, GUARD_HOLD};
use crate::admission::pooled::MOST_CREDIT;

const G: u64 = 1 << 30;
const M: u64 = 1 << 20;

/// `build`, `e2e`, `it` and `lint` of o/r, each predicted at 3G, reserving 3.5G and using 512M in a 32G slice, past
/// their first minute: 3G of room above use each, 12G in all, of which the three largest are 9G.
async fn four_settled_jobs(
    github: &FakeGithub,
    docker: &FakeDocker,
    dir: &TestDir,
    config: &str,
) -> (Engine<FakeGithub, FakeDocker>, Handle) {
    let (mut engine, handle) = engine_with(github, docker, dir, config);
    engine.memory_max = Some(32 * G);
    let names = ["build", "e2e", "it", "lint"];
    for (name, first) in names.into_iter().zip([1000, 2000, 3000, 4000]) {
        history_of(&handle.store, name, first, 3 * G, 0).await;
    }
    engine.recover().await.unwrap();
    github.0.lock().unwrap().queued = names
        .into_iter()
        .zip(1..)
        .map(|(name, id)| QueuedJob {
            name: name.to_owned(),
            ..job(id)
        })
        .collect();
    for slot in 1..=4 {
        let capture = format!("@@job {slot} {slot}\n");
        docker.0.lock().unwrap().slot_captures.insert(slot, capture);
    }
    roomy_readings(github, &handle.store, 512 * M);
    engine.tick(Shutdown::No).await;
    assert_eq!(registered(&github.calls()).len(), 4);
    for id in 1..=4 {
        until_state(&handle, id, JobState::Running).await;
    }
    github.0.lock().unwrap().queued = Vec::new();
    roomy_readings(github, &handle.store, 512 * M);
    engine.tick(Shutdown::No).await;
    first_minute_passes(&mut engine);
    roomy_readings(github, &handle.store, 512 * M);
    (engine, handle)
}

/// The growth count's window goes by with the runners using `used`, and a tick looks.
async fn window_passes(
    engine: &mut Engine<FakeGithub, FakeDocker>,
    github: &FakeGithub,
    handle: &Handle,
    used: u64,
) {
    engine
        .measured
        .age_growth(api::GROWTH_WINDOW + Duration::from_secs(1));
    roomy_readings(github, &handle.store, used);
    engine.tick(Shutdown::No).await;
}

/// What the status says the growth count does, and both credits.
fn counted(
    engine: &mut Engine<FakeGithub, FakeDocker>,
    handle: &Handle,
) -> (Option<api::Counted>, u64, u64) {
    engine.publish();
    let budget = handle.status.borrow().budget.clone();
    let credit = budget.pooled.map_or(0, |pooled| pooled.credit);
    let host = budget
        .host
        .and_then(|host| host.credit)
        .map_or(0, |credit| credit.bytes);
    (budget.counted, credit, host)
}

/// The growth count counting the four jobs: 2G used and the three largest growths, 9G, kept.
const COUNTING: api::Counted = api::Counted {
    used: 2 * G,
    kept: Some(9 * G),
    jobs: 3,
    brake: None,
};

fn fakes() -> (FakeGithub, FakeDocker, TestDir) {
    (
        FakeGithub::default(),
        FakeDocker::default(),
        TestDir::new("growth"),
    )
}

#[tokio::test]
async fn the_growth_count_rests_a_window_after_a_start_then_keeps_room_for_the_three_largest_growths()
 {
    // Catches the growth count counting before a window of looks, its restart begun by another clock than the
    // engine's, and a room other than the three largest growths.
    let (github, docker, dir) = fakes();
    let (mut engine, handle) = four_settled_jobs(&github, &docker, &dir, &measured()).await;
    let (shown, credit, host) = counted(&mut engine, &handle);
    let shown = shown.expect("the count under measured admission");
    assert!(
        matches!(shown.brake, Some(api::CountBrake::Restarted { .. })),
        "{shown:?}"
    );
    assert_eq!(shown.kept, None, "rests on the credits");
    assert_eq!((credit, host), (6 * G, 6 * G), "0099's and 0108's credits");

    window_passes(&mut engine, &github, &handle, 512 * M).await;
    let (shown, credit, host) = counted(&mut engine, &handle);
    assert_eq!(shown, Some(COUNTING));
    assert_eq!(
        (credit, host),
        (3 * G, 3 * G),
        "14G reserved, less 2G used and 9G kept"
    );
    // The stored budget's host credit is the growth count's, which the host-hazard insights count as credited.
    handle.store.flush().await;
    let readings = handle.store.insights(0).await.unwrap().readings;
    assert_eq!(readings.last().map(|reading| reading.credit), Some(3 * G));
}

#[tokio::test]
async fn growth_past_the_room_kept_brakes_the_count_for_half_an_hour() {
    // Catches a tick that takes no look, growth read against another room than the one kept, and a brake that ends
    // before `BRAKE_FOR`.
    let (github, docker, dir) = fakes();
    let (mut engine, handle) = four_settled_jobs(&github, &docker, &dir, &measured()).await;
    window_passes(&mut engine, &github, &handle, 512 * M).await;
    assert_eq!(counted(&mut engine, &handle).0, Some(COUNTING));

    // Each grows 2.5G: 10G together, past the 9G kept.
    runners_use(&github, &handle.store, 3 * G);
    engine.tick(Shutdown::No).await;
    let (shown, credit, _) = counted(&mut engine, &handle);
    let shown = shown.unwrap();
    assert!(
        matches!(
            shown.brake,
            Some(api::CountBrake::Grew { grew, kept, .. }) if grew == 10 * G && kept == 9 * G
        ),
        "{shown:?}"
    );
    assert_eq!(shown.kept, None);
    assert!(credit <= MOST_CREDIT, "the credits count: {credit}");

    // Back down, it still rests until the half hour is out.
    roomy_readings(&github, &handle.store, 512 * M);
    engine.tick(Shutdown::No).await;
    assert_eq!(counted(&mut engine, &handle).0.unwrap().kept, None);
    engine.measured.age_growth(BRAKE_FOR);
    roomy_readings(&github, &handle.store, 512 * M);
    engine.tick(Shutdown::No).await;
    assert_eq!(counted(&mut engine, &handle).0, Some(COUNTING));

    // Growth within the room kept takes no brake: 2G together.
    runners_use(&github, &handle.store, G);
    engine.tick(Shutdown::No).await;
    assert_eq!(counted(&mut engine, &handle).0.unwrap().brake, None);
}

#[tokio::test]
async fn below_the_guards_line_the_count_rests_five_minutes_from_the_crossing() {
    // Catches a growth count over a host short of its margins, a guard that lets go at the first plan back above its
    // line, and one that a plan below the line during the hold extends (the replay chose the hold from the crossing).
    let (github, docker, dir) = fakes();
    let (mut engine, handle) = four_settled_jobs(&github, &docker, &dir, &measured()).await;
    window_passes(&mut engine, &github, &handle, 512 * M).await;
    // 12G available, under the 4G margin and a quarter of the host's 64G.
    let below = || {
        runners_use(&github, &handle.store, 512 * M);
        host_at(&handle.store, G, 12 * G, 0);
    };

    below();
    engine.tick(Shutdown::No).await;
    let shown = counted(&mut engine, &handle).0.unwrap();
    assert!(
        matches!(shown.brake, Some(api::CountBrake::Guard { until: Some(_) })),
        "{shown:?}"
    );
    assert_eq!(shown.kept, None);

    // Below again three minutes on, then back above the line: it rests until five minutes from the crossing.
    engine.measured.age_growth(GUARD_HOLD * 3 / 5);
    below();
    engine.tick(Shutdown::No).await;
    roomy_readings(&github, &handle.store, 512 * M);
    engine.tick(Shutdown::No).await;
    assert_eq!(
        counted(&mut engine, &handle).0.unwrap().kept,
        None,
        "back above the line, it still rests"
    );
    engine.measured.age_growth(GUARD_HOLD * 2 / 5);
    roomy_readings(&github, &handle.store, 512 * M);
    engine.tick(Shutdown::No).await;
    assert_eq!(counted(&mut engine, &handle).0, Some(COUNTING));
}

#[tokio::test]
async fn a_reload_rests_the_count_only_when_it_changes_what_the_count_reads() {
    // Catches a reload that keeps the looks of a `MemoryMax` or a ceiling it changed, or of a count it turned on, and
    // one that rests the count for nothing it reads.
    let (github, docker, dir) = fakes();
    let (mut engine, handle) = four_settled_jobs(&github, &docker, &dir, &measured()).await;
    window_passes(&mut engine, &github, &handle, 512 * M).await;
    let resting = |shown: Option<api::Counted>| {
        matches!(
            shown.and_then(|shown| shown.brake),
            Some(api::CountBrake::Restarted { .. })
        )
    };

    let more_slots = measured().replace("slots = 4", "slots = 5");
    engine.reload(Config::parse(&more_slots).unwrap(), Some(32 * G), None);
    assert_eq!(
        counted(&mut engine, &handle).0,
        Some(COUNTING),
        "slots are not what it reads"
    );

    engine.reload(Config::parse(&measured()).unwrap(), Some(30 * G), None);
    assert!(resting(counted(&mut engine, &handle).0), "a new MemoryMax");
    window_passes(&mut engine, &github, &handle, 512 * M).await;

    let ceiling = measured().replace(
        "admission = \"measured\"",
        "admission = \"measured\"\nmemory_ceiling = \"80G\"",
    );
    engine.reload(Config::parse(&ceiling).unwrap(), Some(30 * G), None);
    assert!(resting(counted(&mut engine, &handle).0), "a new ceiling");
    window_passes(&mut engine, &github, &handle, 512 * M).await;

    let off = ceiling.replace(
        "admission = \"measured\"",
        "admission = \"measured\"\npooled_count = false",
    );
    engine.reload(Config::parse(&off).unwrap(), Some(30 * G), None);
    let (shown, credit, host) = counted(&mut engine, &handle);
    assert_eq!(
        (shown, credit, host),
        (None, 0, 0),
        "off, every take counts"
    );
    engine.reload(Config::parse(&ceiling).unwrap(), Some(30 * G), None);
    assert!(
        resting(counted(&mut engine, &handle).0),
        "turned on, it has no looks of its own"
    );

    let caps = ceiling.replace("admission = \"measured\"\n", "");
    engine.reload(Config::parse(&caps).unwrap(), Some(30 * G), None);
    engine.publish();
    let budget = handle.status.borrow().budget.clone();
    assert_eq!(budget.counted, None);
    assert_eq!(budget.memory_in_use, 16 * G, "every runner at its cap");
}

#[tokio::test]
async fn runners_a_restart_adopts_count_in_full_until_the_window_and_keep_all_their_room_after() {
    // Catches a restart that counts at once (challenge finding 3), and adopted runners ranked with the rest for the
    // room kept, as if this daemon had seen their jobs start and knew how far they had grown.
    let (github, docker, dir) = fakes();
    let (engine, handle) = four_settled_jobs(&github, &docker, &dir, &measured()).await;
    handle.store.flush().await;
    drop((engine, handle));

    let (mut engine, handle) = engine_with(&github, &docker, &dir, &measured());
    engine.memory_max = Some(32 * G);
    engine.recover().await.unwrap();
    roomy_readings(&github, &handle.store, 512 * M);
    engine.tick(Shutdown::No).await;
    first_minute_passes(&mut engine);
    roomy_readings(&github, &handle.store, 512 * M);
    let (shown, credit, host) = counted(&mut engine, &handle);
    assert!(
        matches!(
            shown.and_then(|shown| shown.brake),
            Some(api::CountBrake::Restarted { .. })
        ),
        "a restart rests the count"
    );
    assert_eq!(
        (credit, host),
        (0, 0),
        "and the credits count no runner it adopted"
    );

    window_passes(&mut engine, &github, &handle, 512 * M).await;
    let (shown, credit, host) = counted(&mut engine, &handle);
    assert_eq!(
        shown.and_then(|shown| shown.kept),
        Some(12 * G),
        "all four's room"
    );
    assert_eq!((credit, host), (0, 0));
}

#[tokio::test]
async fn the_hosts_and_the_slices_room_bound_the_credits() {
    // Catches the growth count's credits left unbounded by what the host has past its margin and a quarter of it, or
    // `MemoryMax`'s by what the slice has under its high-water mark: 0099's clamps, which the replay needed.
    let (github, docker, dir) = fakes();
    let (mut engine, handle) = four_settled_jobs(&github, &docker, &dir, &measured()).await;
    window_passes(&mut engine, &github, &handle, 512 * M).await;
    // 21G available: 1G past the 4G margin and a quarter of 64G.
    host_at(&handle.store, G, 21 * G, 0);
    engine.tick(Shutdown::No).await;
    let (shown, credit, host) = counted(&mut engine, &handle);
    assert_eq!(shown, Some(COUNTING), "above the line, it counts");
    assert_eq!((credit, host), (G, G), "3G each, held to the host's 1G");

    // A slice of 25G leaves 1.8G under 90% of its 32G, less 2G: MemoryMax's credit only.
    roomy_readings(&github, &handle.store, 512 * M);
    slice_at(&handle.store, 25 * G);
    engine.tick(Shutdown::No).await;
    let (_, credit, host) = counted(&mut engine, &handle);
    assert_eq!((credit, host), (32 * G * 9 / 10 - 27 * G, 3 * G));
}

#[tokio::test]
async fn a_plan_counts_again_after_each_start_and_starts_what_the_count_leaves() {
    // Catches a plan that takes each start's reservation off what is left: under the growth count a start adds only
    // what it changes of the room kept, here nothing, as the three largest growths already stand for it.
    let (github, docker, dir) = fakes();
    let six = measured().replace("slots = 4", "slots = 6");
    let (mut engine, handle) = four_settled_jobs(&github, &docker, &dir, &six).await;
    window_passes(&mut engine, &github, &handle, 512 * M).await;
    assert_eq!(counted(&mut engine, &handle).0, Some(COUNTING));
    // 11G counted of 15G: room for one fmt runner's 3.5G by the reservations, for both by the count.
    engine.memory_max = Some(15 * G);
    github.0.lock().unwrap().queued = (5..=6)
        .map(|id| QueuedJob {
            name: "fmt".to_owned(),
            ..job(id)
        })
        .collect();
    roomy_readings(&github, &handle.store, 512 * M);
    engine.tick(Shutdown::No).await;
    assert_eq!(registered(&github.calls()).len(), 6, "both fmt jobs start");
    engine.publish();
    let budget = handle.status.borrow().budget.clone();
    assert!(
        budget.counted() <= 15 * G,
        "and the count stays within MemoryMax: {budget:?}"
    );
}

#[tokio::test]
async fn raised_waiting_runners_stop_until_the_count_without_them_fits() {
    // Catches `over_budget` taking a stopped runner's take off the count: under the growth count a waiting runner's
    // stop frees what it uses, as the room kept for growth stays the three largest headrooms.
    let (github, docker, dir) = fakes();
    let six = measured().replace("slots = 4", "slots = 6");
    let (mut engine, handle) = four_settled_jobs(&github, &docker, &dir, &six).await;
    window_passes(&mut engine, &github, &handle, 512 * M).await;
    github.0.lock().unwrap().queued = (5..=6)
        .map(|id| QueuedJob {
            name: "fmt".to_owned(),
            ..job(id)
        })
        .collect();
    engine.tick(Shutdown::No).await;
    assert_eq!(registered(&github.calls()).len(), 6);
    github.0.lock().unwrap().queued = Vec::new();
    let waiting: Vec<String> = engine
        .occupied()
        .filter(|(_, occupant)| occupant.named_job().is_none())
        .map(|(_, occupant)| occupant.runner_name.clone())
        .collect();
    assert_eq!(waiting.len(), 2, "both fmt runners wait for their jobs");
    for runner in waiting {
        engine.raise(runner);
    }
    // Six runners using 1.5G each, the three largest headrooms 6G: 15G counted, of 21G reserved, with the credit
    // under the slice's room. Each waiting runner's stop frees its 1.5G, not its 3.5G.
    roomy_readings(&github, &handle.store, 3 * G / 2);
    engine.memory_max = Some(13 * G);
    assert_eq!(engine.count(engine.host_remembered()).pooled(), 15 * G);
    assert_eq!(
        engine.over_budget(&[]).len(),
        2,
        "one stop leaves 13.5G, past 13G"
    );
}
