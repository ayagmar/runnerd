//! The pace of the reads by GitHub's rate limit (ADR 0127), first as the pure function it is, fed readings the way the
//! engine feeds them, then through an engine on the fakes.

use std::time::SystemTime;

use tokio::time::Instant;

use super::invariants::Rng;
use super::*;
use crate::github::{RateLimit, Spend};
use crate::scheduler::engine::governor::{Cause, Governor, Outlook, Step};

const TICK: Duration = Duration::from_secs(15);
const POLL: Duration = Duration::from_secs(15);
/// The least time of readings a step is judged on.
const SPAN: Duration = Duration::from_mins(2);

const STEPS: [Step; 6] = [
    Step::Steady,
    Step::JobReads2,
    Step::JobReads4,
    Step::ActiveListings,
    Step::Listings2,
    Step::Listings4,
];

/// A limit read every tick: `at` since the start, a count that ends `until` after it.
struct Sim {
    governor: Governor,
    base: Instant,
    wall: SystemTime,
    at: Duration,
    reset: SystemTime,
    remaining: u64,
    mutations: u64,
    /// Why the pace last changed.
    cause: Option<Cause>,
}

/// A change of pace as a test sees it: when, from which step, to which.
type Moved = (Duration, Step, Step);

impl Sim {
    /// A limit with `remaining` requests, resetting `until_reset` from the first reading, which is taken now.
    fn new(remaining: u64, until_reset: Duration) -> Sim {
        let wall = SystemTime::UNIX_EPOCH + Duration::from_hours(500_000);
        let mut sim = Sim {
            governor: Governor::default(),
            base: Instant::now(),
            wall,
            at: Duration::ZERO,
            reset: wall + until_reset,
            remaining,
            mutations: 0,
            cause: None,
        };
        assert_eq!(sim.observe(sim.spend()), None);
        sim
    }

    fn spend(&self) -> Spend {
        Spend {
            rate_limit: Some(RateLimit {
                limit: 5000,
                remaining: self.remaining,
                reset: self.reset,
            }),
            mutations: self.mutations,
        }
    }

    /// Time passes by `by`, `spent` requests are made, `mutations` of them registrations or deletes, and the engine
    /// looks.
    fn tick(&mut self, by: Duration, spent: u64, mutations: u64) -> Option<Moved> {
        self.at += by;
        self.remaining = self.remaining.saturating_sub(spent);
        self.mutations += mutations;
        self.observe(self.spend())
    }

    fn observe(&mut self, spend: Spend) -> Option<Moved> {
        let change = self
            .governor
            .observe(self.base + self.at, self.wall + self.at, spend)?;
        self.cause = Some(change.cause);
        Some((self.at, change.from, change.to))
    }

    fn until_reset(&self) -> Duration {
        self.reset
            .duration_since(self.wall + self.at)
            .unwrap_or_default()
    }

    /// Ticks for `span`, each spending `per_tick`: the changes of pace.
    fn run(&mut self, span: Duration, per_tick: u64) -> Vec<Moved> {
        let until = self.at + span;
        let mut moved = Vec::new();
        while self.at < until {
            moved.extend(self.tick(TICK, per_tick, 0));
        }
        moved
    }
}

/// The steps `moved` went through, in order.
fn steps_of(moved: &[Moved]) -> Vec<Step> {
    moved.iter().map(|&(_, _, to)| to).collect()
}

/// The bug it catches: a governor that acts on a limit that is fine, so the reads slow for nothing.
#[test]
fn a_healthy_limit_leaves_the_pace_alone() {
    let mut sim = Sim::new(5000, Duration::from_hours(1));
    // 8 requests a minute is 480 an hour against 5000.
    assert_eq!(sim.run(Duration::from_mins(30), 2), []);
    assert_eq!(sim.governor.reads_every(POLL), POLL);
    assert_eq!(sim.governor.ticks_every(POLL), POLL);
}

/// The bugs it catches: listings slowed before the job reads, a step skipped or taken twice, two steps in one go, and
/// a step judged on readings from before the last one, which does not wait to see what the new pace costs.
#[test]
fn a_draining_limit_slows_job_reads_first_then_listings_one_step_at_a_time() {
    let mut sim = Sim::new(1000, Duration::from_mins(30));
    // Another client spends 60 a minute whatever runnerd does: 1,800 projected against 900 left.
    let moved = sim.run(Duration::from_mins(12), 15);
    assert_eq!(
        steps_of(&moved),
        [
            Step::JobReads2,
            Step::JobReads4,
            Step::ActiveListings,
            Step::Listings2,
            Step::Listings4
        ]
    );
    for pair in moved.windows(2) {
        assert!(pair[1].0.saturating_sub(pair[0].0) >= SPAN, "{pair:?}");
        assert_eq!(pair[1].1, pair[0].2, "one step from where the last left it");
    }
    assert!(moved[0].0 >= SPAN, "judged on two minutes at least");
    assert_eq!(sim.governor.reads_every(POLL), Duration::from_mins(1));
    assert_eq!(sim.governor.ticks_every(POLL), Duration::from_mins(1));
}

/// Each pace in turn: what it means for the two intervals.
#[test]
fn each_step_sets_the_reads_and_the_listings_it_names() {
    let seconds = |step: Step| (POLL * step.reads(), POLL * step.listings());
    let at =
        |reads: u64, listings: u64| (Duration::from_secs(reads), Duration::from_secs(listings));
    assert_eq!(
        STEPS.map(seconds),
        [
            at(15, 15),
            at(30, 15),
            at(60, 15),
            at(60, 15),
            at(60, 30),
            at(60, 60)
        ]
    );
}

/// The bug it catches: a step undone in one go, in another order than it was taken, or before the spend fits.
#[test]
fn a_slowed_pace_eases_one_step_at_a_time_once_the_spend_fits() {
    let mut sim = Sim::new(1000, Duration::from_mins(50));
    sim.run(Duration::from_mins(12), 15);
    assert_eq!(sim.governor.step(), Step::Listings4);

    // The other client stops. The first step waits until the last ten minutes' average fits, the rest two minutes
    // each.
    let moved = sim.run(Duration::from_mins(24), 0);
    assert_eq!(
        steps_of(&moved),
        [
            Step::Listings2,
            Step::ActiveListings,
            Step::JobReads4,
            Step::JobReads2,
            Step::Steady
        ]
    );
    for pair in moved.windows(2) {
        assert!(pair[1].0.saturating_sub(pair[0].0) >= SPAN, "{pair:?}");
    }
}

/// The bugs it catches: a step undone while the spend is still within 10% of the room, or by a margin that ignores
/// what the step saved, which brings the step it undid back; one taken while it only reaches the room.
#[test]
fn a_step_is_taken_over_the_room_and_undone_only_with_ten_percent_spare_after_its_saving_is_spent()
{
    let verdict = |projected: u64, room: u64, step: Step| Outlook { projected, room }.verdict(step);
    // Over the room: one up. Reaching it is not over.
    assert_eq!(verdict(101, 100, Step::Steady), Step::JobReads2);
    assert_eq!(verdict(100, 100, Step::JobReads2), Step::JobReads2);
    // Undoing a step that halved the reads may double the spend: it fits when double the spend is 90% of the room.
    assert_eq!(verdict(45, 100, Step::JobReads2), Step::Steady);
    assert_eq!(verdict(46, 100, Step::JobReads2), Step::JobReads2);
    assert_eq!(verdict(45, 100, Step::Listings4), Step::Listings2);
    assert_eq!(verdict(46, 100, Step::Listings4), Step::Listings4);
    // Listings back to a third of the interval cost up to three times.
    assert_eq!(verdict(30, 100, Step::ActiveListings), Step::JobReads4);
    assert_eq!(verdict(31, 100, Step::ActiveListings), Step::ActiveListings);
    // Nothing above the slowest, nothing below the pace before the governor.
    assert_eq!(verdict(1000, 100, Step::Listings4), Step::Listings4);
    assert_eq!(verdict(0, 100, Step::Steady), Step::Steady);
    // No room and nothing spent: no evidence the pace may ease.
    assert_eq!(verdict(0, 0, Step::JobReads2), Step::JobReads2);
}

/// The bug it catches: a window of readings that is not the last ten minutes: a burst long gone still projected, or a
/// recent one missed.
#[test]
fn the_projected_spend_is_the_last_ten_minutes_of_readings() {
    // So much is left that nothing steps, and every reading is of one pace.
    let mut sim = Sim::new(1_000_000, Duration::from_mins(30));
    sim.run(Duration::from_mins(4), 25);
    sim.run(Duration::from_mins(20), 0);
    // Minute 24: the burst of 100 a minute is 16 minutes back.
    assert_eq!(
        sim.governor.outlook(sim.until_reset()).unwrap().projected,
        0
    );

    let mut sim = Sim::new(1_000_000, Duration::from_mins(30));
    sim.run(Duration::from_mins(15), 0);
    sim.run(Duration::from_mins(9), 25);
    // Minute 24: 900 requests in the last 10 minutes, 6 minutes to the reset.
    assert_eq!(
        sim.governor.outlook(sim.until_reset()).unwrap().projected,
        540
    );
}

/// The bugs it catches: a step judged on a span so short that a burst looks like an hour's pace, and a boundary off by
/// one reading.
#[test]
fn a_pace_is_judged_only_after_two_minutes_of_readings() {
    let mut sim = Sim::new(100_000, Duration::from_mins(30));
    // Seven ticks of 15 s after the first reading: 105 s.
    for _ in 0..7 {
        sim.tick(TICK, 1, 0);
    }
    assert_eq!(sim.governor.outlook(sim.until_reset()), None);
    sim.tick(TICK, 1, 0);
    assert!(
        sim.governor.outlook(sim.until_reset()).is_some(),
        "two minutes from the first reading to the last"
    );
}

/// The bugs it catches: no reserve floor, so a quiet hour leaves nothing for a burst of registrations; the
/// registrations and deletes counted in the projected spend as well as in the reserve; and their rate taken over the
/// readings of the pace, not of the window, which lets a burst of starts right after a step look like an hour's.
#[test]
fn the_reserve_is_what_registrations_and_deletes_need_and_at_least_a_hundred() {
    let outlook = |sim: &Sim| sim.governor.outlook(sim.until_reset()).unwrap();
    // None sent: the floor.
    let mut sim = Sim::new(600, Duration::from_mins(30));
    sim.run(Duration::from_mins(5), 0);
    assert_eq!(outlook(&sim).room, 500);

    // Every request a registration or a delete, 8 a minute for 10 minutes: 80 sent, 20 minutes to the reset, so
    // 160 more. They are the whole of the drop, which leaves no spend of the reads.
    let mut sim = Sim::new(5000, Duration::from_mins(30));
    for _ in 0..40 {
        sim.tick(TICK, 2, 2);
    }
    assert_eq!(
        outlook(&sim),
        Outlook {
            projected: 0,
            room: 4920 - 160
        }
    );

    // 120 s with no registration, then 3 a tick under the step the first 120 s led to: 24 sent in 240 s, the reserve
    // of the window, not of the 120 s since the step (which says 312).
    let mut sim = Sim::new(1000, Duration::from_mins(30));
    let moved = sim.run(Duration::from_mins(2), 15);
    assert_eq!(steps_of(&moved), [Step::JobReads2]);
    for _ in 0..8 {
        sim.tick(TICK, 6, 3);
    }
    assert_eq!(sim.governor.step(), Step::JobReads2);
    assert_eq!(
        outlook(&sim),
        Outlook {
            projected: 312,
            room: 832 - 156
        }
    );
}

/// The bugs it catches: a reading of a count that already ended acted on, and one that does not wait for the next.
#[test]
fn a_reading_whose_reset_has_passed_is_not_acted_on() {
    let mut sim = Sim::new(1000, Duration::from_mins(30));
    sim.run(Duration::from_mins(3), 15);
    assert_eq!(sim.governor.step(), Step::JobReads2);

    // The answers still carry the old count, whose reset is behind.
    sim.at += Duration::from_mins(28);
    assert_eq!(sim.observe(sim.spend()), None);
    assert_eq!(sim.governor.step(), Step::JobReads2);
    sim.at += Duration::from_mins(1);
    sim.reset = sim.wall + sim.at;
    assert_eq!(sim.observe(sim.spend()), None, "reset at this very moment");
    assert_eq!(sim.governor.step(), Step::JobReads2);
}

/// The bugs it catches: a new count read with the old one's readings, so the rise in `remaining` is no spend and the
/// pace stays slow into an hour that has all its requests; and a pace kept across the reset.
#[test]
fn a_new_count_starts_from_the_pace_before_the_governor_and_with_no_readings() {
    let mut sim = Sim::new(1000, Duration::from_mins(30));
    sim.run(Duration::from_mins(12), 15);
    assert_eq!(sim.governor.step(), Step::Listings4);

    sim.at += Duration::from_mins(19);
    sim.reset += Duration::from_hours(1);
    sim.remaining = 4990;
    let change = sim
        .governor
        .observe(sim.base + sim.at, sim.wall + sim.at, sim.spend())
        .unwrap();
    assert_eq!(
        (change.from, change.to, change.cause),
        (Step::Listings4, Step::Steady, Cause::Reset)
    );
    assert_eq!(sim.governor.outlook(sim.until_reset()), None);
    assert_eq!(sim.governor.reads_every(POLL), POLL);
    // The new count is judged on its own readings: 15 a tick is 60 a minute against 4,990 for an hour.
    assert_eq!(sim.run(Duration::from_mins(3), 15), []);
}

/// The bug it catches: a limit missing from an answer taken for a reading, or for a reason to forget the earlier ones.
#[test]
fn an_answer_with_no_limit_changes_nothing_and_the_readings_go_on_after_it() {
    let mut sim = Sim::new(1000, Duration::from_mins(30));
    sim.run(Duration::from_mins(1), 15);
    sim.at += Duration::from_mins(5);
    let none = Spend {
        rate_limit: None,
        mutations: sim.mutations,
    };
    assert_eq!(sim.observe(none), None);
    assert_eq!(sim.governor.step(), Step::Steady);
    // The next reading closes a window that goes back to the first: 6 minutes, of which the gap spent nothing seen.
    sim.remaining -= 15 * 24;
    let moved = sim.observe(sim.spend());
    assert_eq!(
        moved.map(|(_, from, to)| (from, to)),
        Some((Step::Steady, Step::JobReads2))
    );
}

/// The bug it catches: a rule broken by a sequence no one wrote by hand: a step taken or undone against what the
/// projection said, two at once, a pace past either end, a reading that does not hold, an hour that starts over
/// without the pace before the governor, and a panic on any of them.
#[test]
fn generated_limits_never_break_the_governors_rules() {
    for seed in 0..400 {
        let mut rng = Rng(seed);
        let mut sim = Sim::new(
            200 + rng.below(4800),
            Duration::from_mins(5 + rng.below(55)),
        );
        for _ in 0..600 {
            let by = Duration::from_secs(1 + rng.below(45));
            let was = sim.governor.step();
            let moved = match rng.below(24) {
                // Headers missing: a transport error, an unknown resource.
                0 => {
                    sim.at += by;
                    let none = Spend {
                        rate_limit: None,
                        mutations: sim.mutations,
                    };
                    assert_eq!(sim.observe(none), None, "seed {seed}");
                    None
                }
                // The count passes its reset, answers still carrying it; then the next hour's, full.
                1 => {
                    sim.at += sim.until_reset() + by;
                    assert_eq!(sim.observe(sim.spend()), None, "seed {seed}");
                    assert_eq!(sim.governor.step(), was, "seed {seed}");
                    sim.reset = sim.wall + sim.at + Duration::from_mins(60);
                    sim.remaining = 5000;
                    sim.observe(sim.spend())
                }
                // Another client gave requests back, or a 304 carried an older count.
                2 => {
                    sim.remaining += rng.below(500);
                    sim.tick(by, 0, 0)
                }
                _ => {
                    let spent = rng.below(60);
                    let mutations = rng.below(7).min(spent);
                    sim.tick(by, spent, mutations)
                }
            };
            let paces = [
                sim.governor.reads_every(POLL),
                sim.governor.ticks_every(POLL),
            ];
            assert!(
                paces.iter().all(|pace| (POLL..=POLL * 4).contains(pace)),
                "seed {seed}: {paces:?}"
            );
            let Some((_, from, to)) = moved else {
                assert_eq!(sim.governor.step(), was, "seed {seed}");
                continue;
            };
            assert_eq!(from, was, "seed {seed}");
            let place = |step: Step| STEPS.iter().position(|&s| s == step).unwrap();
            match sim.cause {
                Some(Cause::Reset) => {
                    assert_eq!(to, Step::Steady, "seed {seed}");
                    assert_eq!(sim.governor.outlook(sim.until_reset()), None, "seed {seed}");
                }
                _ => assert_eq!(
                    place(to).abs_diff(place(from)),
                    1,
                    "seed {seed}: {from:?} to {to:?}"
                ),
            }
        }
    }
}

/// Every change the governor reports, checked against what it was told: taken only over the room, undone only with
/// the spend, doubled, within 90% of it.
#[test]
fn a_change_carries_the_projection_that_called_for_it() {
    for seed in 0..200 {
        let mut rng = Rng(seed);
        let mut sim = Sim::new(
            500 + rng.below(4500),
            Duration::from_mins(20 + rng.below(40)),
        );
        for _ in 0..400 {
            let by = Duration::from_secs(5 + rng.below(40));
            let burst = rng.below(4) == 0;
            let spent = rng.below(if burst { 80 } else { 8 });
            sim.at += by;
            sim.remaining = sim.remaining.saturating_sub(spent);
            sim.mutations += rng.below(3).min(spent);
            let Some(change) =
                sim.governor
                    .observe(sim.base + sim.at, sim.wall + sim.at, sim.spend())
            else {
                continue;
            };
            match change.cause {
                Cause::Projected { outlook, .. } if change.to > change.from => {
                    assert!(outlook.projected > outlook.room, "seed {seed}: {change:?}");
                }
                Cause::Projected { outlook, .. } => {
                    assert!(
                        outlook.room > 0
                            && u128::from(outlook.projected) * 20 <= u128::from(outlook.room) * 9,
                        "seed {seed}: undone within the band: {change:?}"
                    );
                }
                Cause::Reset => assert_eq!(change.to, Step::Steady, "seed {seed}"),
            }
        }
    }
}

/// The bug it catches: a pace that flaps. A plant with the same demand all hour, whose cost falls with each step, and
/// no other change: once a step is undone, none is taken again, because what the step saved was counted before it was
/// undone.
#[test]
fn with_the_same_demand_all_hour_no_step_is_taken_after_one_is_undone() {
    for seed in 0..300 {
        let mut rng = Rng(seed);
        // Requests a minute at the pace before the governor, in multiples of 16 so every pace's tick is whole.
        let reads = 16 * (1 + rng.below(12));
        let listings = 16 * (1 + rng.below(12));
        let others = 4 * rng.below(15);
        let mutations = 4 * rng.below(5);
        let mut sim = Sim::new(5000, Duration::from_hours(1));
        let mut undone = false;
        while sim.at < Duration::from_mins(59) {
            let reads_factor = sim.governor.reads_every(POLL).as_secs() / POLL.as_secs();
            let listings_factor = sim.governor.ticks_every(POLL).as_secs() / POLL.as_secs();
            let per_minute = reads / reads_factor + listings / listings_factor + others + mutations;
            if let Some((_, from, to)) = sim.tick(TICK, per_minute / 4, mutations / 4) {
                assert!(
                    !(undone && to > from),
                    "seed {seed}: reads {reads}, listings {listings}, others {others}, registrations {mutations}: \
                     {from:?} to {to:?} at {:?} after one was undone",
                    sim.at
                );
                undone |= to < from;
            }
        }
    }
}

/// The bug it catches: a window of ten minutes that ticks further apart than that leave with one reading, so no pace
/// is ever judged again and a slowed one is held until the reset. With `poll_interval = 3m` the slowest ticks are 12
/// minutes apart.
#[test]
fn with_ticks_further_apart_than_the_window_the_pace_still_eases() {
    let poll = Duration::from_mins(3);
    let mut sim = Sim::new(2500, Duration::from_mins(59));
    let mut slowest = Step::Steady;
    while sim.at < Duration::from_mins(58) {
        let by = sim.governor.ticks_every(poll);
        // Another client spends 60 a minute for the first 20 minutes.
        let minutes = by.as_secs() / 60;
        let spent = if sim.at < Duration::from_mins(20) {
            60 * minutes
        } else {
            0
        };
        sim.tick(by, spent, 0);
        slowest = slowest.max(sim.governor.step());
    }
    assert_eq!(slowest, Step::Listings4, "it slowed all the way");
    assert_eq!(
        sim.governor.step(),
        Step::Steady,
        "and eased once the other client stopped, with {} left",
        sim.remaining
    );
}

/// The bug it catches: the previous reading dropped from a window it is the only one to reach back across: two
/// readings 12 minutes apart are a span of 12 minutes, so the spend between them is projected.
#[test]
fn the_window_reaches_back_to_the_last_reading_at_or_before_ten_minutes_ago() {
    let mut sim = Sim::new(1_000_000, Duration::from_mins(60));
    sim.tick(Duration::from_mins(12), 120, 0);
    // 120 in 12 minutes, 48 minutes to the reset.
    assert_eq!(
        sim.governor.outlook(sim.until_reset()).unwrap().projected,
        480
    );
    // A third, 12 minutes on: the first is out of the window, the second is the one before the cutoff.
    sim.tick(Duration::from_mins(12), 0, 0);
    assert_eq!(
        sim.governor.outlook(sim.until_reset()).unwrap().projected,
        0
    );
}

/// The bug it catches: the step that holds active repositories' listings at `poll_interval` not telling PR 6 anything:
/// before it, their own pace; from it on, every listing's.
#[test]
fn active_listings_keep_their_own_pace_until_the_step_that_holds_them() {
    let third = POLL / 3;
    let mut sim = Sim::new(1000, Duration::from_mins(30));
    let mut paces = vec![(
        sim.governor.step(),
        sim.governor.active_listings_every(POLL, third),
    )];
    // 60 a minute against 900 left: five steps up.
    for _ in 0..(12 * 4) {
        if sim.tick(TICK, 15, 0).is_some() {
            paces.push((
                sim.governor.step(),
                sim.governor.active_listings_every(POLL, third),
            ));
        }
    }
    assert_eq!(
        paces,
        [
            (Step::Steady, third),
            (Step::JobReads2, third),
            (Step::JobReads4, third),
            (Step::ActiveListings, POLL),
            (Step::Listings2, POLL * 2),
            (Step::Listings4, POLL * 4),
        ]
    );
}
