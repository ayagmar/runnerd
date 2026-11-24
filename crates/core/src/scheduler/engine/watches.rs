//! How the engine watches each runner's container: not yet, as Docker failed to inspect it; through a live log
//! capture; waiting to capture again after one broke off; spent, when one started again after a break ended without
//! breaking; or done, with nothing left to capture. It owns that value
//! and must not call Docker, the store or GitHub, or log: `history` inspects, captures and words what came of each.

use std::collections::BTreeMap;
use std::time::Duration;

use tokio::task::JoinHandle;
use tokio::time::Instant;

use super::history::Capture;

/// Each runner's watch, by runner name, from `observe`'s first look at its container until `forget`.
///
/// A runner with no entry is unwatched: `observe` has not inspected its container yet, and `observe_again` tries each
/// tick. Nothing survives a restart: `recover` adopts each runner unwatched, and its first `observe` watches it again
/// from where its job's stored log ends, as does every capture started again. A reload keeps every entry: a runner of a retired slot is watched until it is
/// gone.
#[derive(Debug, Default)]
pub(super) struct Watches(BTreeMap<String, Watch>);

/// One runner's watch.
#[derive(Debug)]
enum Watch {
    /// Its log capture runs. `broke` says how often captures broke off before it, so a break after it waits longer.
    Live {
        capture: JoinHandle<Capture>,
        broke: Option<Broke>,
    },
    /// Its capture broke off while its container may run on: `recapture` inspects it at `at`.
    Waiting { broke: Broke, at: Instant },
    /// A capture started again after a break ended without breaking: it read its job's log whole or up to `log_max`,
    /// or its output ended. Nothing is left to capture, but its job may be left to read from the worker log, which
    /// `end_breaks` says.
    Spent { broke: Broke },
    /// Nothing is left to capture: its capture read its job's log whole or up to `log_max`, or its container stopped.
    Done,
}

/// What a runner's broken captures left.
#[derive(Debug, Clone, Copy)]
struct Broke {
    /// How many broke off so far.
    breaks: u32,
}

impl Watches {
    /// Watches `runner` through `capture`, in place of any capture it had, which is stopped. What earlier captures
    /// left when they broke stays.
    pub(super) fn start(&mut self, runner: &str, capture: JoinHandle<Capture>) {
        let broke = match self.0.remove(runner) {
            Some(Watch::Live {
                capture: old,
                broke,
            }) => {
                old.abort();
                broke
            }
            Some(Watch::Waiting { broke, .. } | Watch::Spent { broke }) => Some(broke),
            Some(Watch::Done) | None => None,
        };
        self.0
            .insert(runner.to_owned(), Watch::Live { capture, broke });
    }

    /// Notes how the captures that ended since ended, as of `now`: one that broke off waits to start again for a time
    /// that grows with its breaks (`capture_backoff`); one that ended done leaves its runner done, or spent if an
    /// earlier one broke.
    pub(super) async fn settle(&mut self, now: Instant) {
        let ended: Vec<String> = self
            .0
            .iter()
            .filter(
                |(_, watch)| matches!(watch, Watch::Live { capture, .. } if capture.is_finished()),
            )
            .map(|(runner, _)| runner.clone())
            .collect();
        for runner in ended {
            let Some(Watch::Live { capture, broke }) = self.0.remove(&runner) else {
                continue;
            };
            let watch = match (capture.await, broke) {
                (Ok(Capture::Broken), broke) => {
                    let breaks = broke.map_or(0, |broke| broke.breaks) + 1;
                    Watch::Waiting {
                        broke: Broke { breaks },
                        at: now + capture_backoff(breaks),
                    }
                }
                (_, Some(broke)) => Watch::Spent { broke },
                (_, None) => Watch::Done,
            };
            self.0.insert(runner, watch);
        }
    }

    /// Takes `runner`'s live capture, to wait for its end: its runner is going. What earlier captures left when they
    /// broke stays until `end_breaks`.
    pub(super) fn take_capture(&mut self, runner: &str) -> Option<JoinHandle<Capture>> {
        if !matches!(self.0.get(runner), Some(Watch::Live { .. })) {
            return None;
        }
        let Some(Watch::Live { capture, broke }) = self.0.remove(runner) else {
            return None;
        };
        let rest = match broke {
            Some(broke) => Watch::Waiting {
                broke,
                at: Instant::now(),
            },
            None => Watch::Done,
        };
        self.0.insert(runner.to_owned(), rest);
        Some(capture)
    }

    /// `runner`'s container stopped or went: no capture of it starts again. Whether one had broken off, so its job's
    /// name may be left to read from the worker log.
    pub(super) fn end_breaks(&mut self, runner: &str) -> bool {
        let Some(watch) = self.0.get_mut(runner) else {
            return false;
        };
        match watch {
            Watch::Live { broke, .. } => broke.take().is_some(),
            Watch::Waiting { .. } | Watch::Spent { .. } => {
                *watch = Watch::Done;
                true
            }
            Watch::Done => false,
        }
    }

    /// Stops watching `runner`, which is gone.
    pub(super) fn forget(&mut self, runner: &str) {
        self.0.remove(runner);
    }

    /// Whether `observe` watches `runner`: not while Docker fails to inspect its container.
    pub(super) fn is_watched(&self, runner: &str) -> bool {
        self.0.contains_key(runner)
    }

    /// Whether `runner`'s capture is due to start again by `now`.
    pub(super) fn is_due(&self, runner: &str, now: Instant) -> bool {
        matches!(self.0.get(runner), Some(Watch::Waiting { at, .. }) if *at <= now)
    }
}

#[cfg(test)]
impl Watches {
    /// Makes `capture` the log capture of `runner` without stopping the one it had.
    pub(super) fn replace_capture(&mut self, runner: &str, capture: JoinHandle<Capture>) {
        let broke = match self.0.remove(runner) {
            Some(Watch::Live { broke, .. }) => broke,
            Some(Watch::Waiting { broke, .. } | Watch::Spent { broke }) => Some(broke),
            Some(Watch::Done) | None => None,
        };
        self.0
            .insert(runner.to_owned(), Watch::Live { capture, broke });
    }

    /// Stops every live capture, as a daemon's exit does.
    pub(super) fn abort_all(&mut self) {
        for runner in self.0.keys().cloned().collect::<Vec<_>>() {
            if let Some(capture) = self.take_capture(&runner) {
                capture.abort();
            }
        }
    }

    /// Whether every capture started has ended, though `settle` may not have noted how yet.
    pub(super) fn every_capture_ended(&self) -> bool {
        self.0.values().all(|watch| match watch {
            Watch::Live { capture, .. } => capture.is_finished(),
            Watch::Waiting { .. } | Watch::Spent { .. } | Watch::Done => true,
        })
    }

    /// The runners watched, in name order.
    pub(super) fn runners(&self) -> impl Iterator<Item = &str> {
        self.0.keys().map(String::as_str)
    }

    /// How many runners have a live capture after an earlier one broke off.
    pub(super) fn live_after_breaks(&self) -> usize {
        self.0
            .values()
            .filter(|watch| matches!(watch, Watch::Live { broke: Some(_), .. }))
            .count()
    }
}

/// How long a runner's log capture waits to start again after its `breaks`-th break: not at all the first time,
/// then 5 seconds, doubling up to 5 minutes, so an exec Docker keeps failing is not run every tick.
fn capture_backoff(breaks: u32) -> Duration {
    if breaks <= 1 {
        return Duration::ZERO;
    }
    Duration::from_secs(5 << (breaks - 2).min(6)).min(Duration::from_mins(5))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use tokio::sync::oneshot;

    use super::*;

    /// How a test's capture ends.
    #[derive(Debug, Clone, Copy)]
    enum End {
        Broken,
        Done,
        /// It panicked, or a daemon's exit aborted it: `settle` reads a `JoinError`.
        Failed,
    }

    /// A capture that runs until `end` says how it ends.
    fn capture() -> (JoinHandle<Capture>, oneshot::Sender<End>) {
        let (end, ends) = oneshot::channel();
        let task = tokio::spawn(async move {
            match ends.await {
                Ok(End::Broken) => Capture::Broken,
                Ok(End::Done) | Err(_) => Capture::Done,
                Ok(End::Failed) => std::future::pending().await,
            }
        });
        (task, end)
    }

    /// Ends a capture `capture` started and waits until its task finished.
    async fn end(watches: &Watches, runner: &str, end: oneshot::Sender<End>, how: End) {
        if matches!(how, End::Failed) {
            if let Some(Watch::Live { capture, .. }) = watches.0.get(runner) {
                capture.abort();
            }
        } else {
            end.send(how).unwrap();
        }
        while matches!(
            watches.0.get(runner),
            Some(Watch::Live { capture, .. }) if !capture.is_finished()
        ) {
            tokio::task::yield_now().await;
        }
    }

    fn due_now(watches: &Watches, runner: &str) -> bool {
        watches.is_due(runner, Instant::now())
    }

    #[tokio::test(start_paused = true)]
    async fn a_runner_is_unwatched_until_its_first_capture_and_after_it_is_forgotten() {
        let mut watches = Watches::default();
        assert!(
            !watches.is_watched("r"),
            "r is unwatched before its first look"
        );
        let (task, _end) = capture();
        watches.start("r", task);
        assert!(
            watches.is_watched("r"),
            "r is watched once its capture runs"
        );
        assert!(watches.take_capture("r").is_some(), "its capture is live");
        watches.forget("r");
        assert!(!watches.is_watched("r"), "a gone runner is unwatched");
        assert!(!watches.end_breaks("r"), "a gone runner has no break");
    }

    #[tokio::test(start_paused = true)]
    async fn a_capture_that_ends_done_leaves_its_runner_watched_and_never_due() {
        let mut watches = Watches::default();
        let (task, done) = capture();
        watches.start("r", task);
        end(&watches, "r", done, End::Done).await;
        watches.settle(Instant::now()).await;
        tokio::time::advance(Duration::from_hours(1)).await;
        watches.settle(Instant::now()).await;
        assert!(!due_now(&watches, "r"), "nothing is left to capture");
        assert!(
            watches.is_watched("r"),
            "`observe_again` must not watch it afresh"
        );
        assert!(watches.take_capture("r").is_none(), "no capture runs");
        assert!(!watches.end_breaks("r"), "no capture broke");
    }

    #[tokio::test(start_paused = true)]
    async fn a_capture_breaking_off_again_waits_longer() {
        let mut watches = Watches::default();
        let (task, first) = capture();
        watches.start("r", task);
        assert!(!due_now(&watches, "r"), "a live capture is not due");
        end(&watches, "r", first, End::Broken).await;
        assert!(
            !due_now(&watches, "r"),
            "not due before `settle` notes the break"
        );
        watches.settle(Instant::now()).await;
        assert!(due_now(&watches, "r"), "the first break is due at once");

        let (task, second) = capture();
        watches.start("r", task);
        assert_eq!(watches.live_after_breaks(), 1);
        end(&watches, "r", second, End::Broken).await;
        watches.settle(Instant::now()).await;
        assert!(!due_now(&watches, "r"), "the second break waits");
        tokio::time::advance(Duration::from_secs(5)).await;
        assert!(due_now(&watches, "r"), "due once its wait is over");

        let (task, third) = capture();
        watches.start("r", task);
        end(&watches, "r", third, End::Broken).await;
        watches.settle(Instant::now()).await;
        tokio::time::advance(Duration::from_secs(9)).await;
        assert!(!due_now(&watches, "r"), "the third break waits 10 seconds");
        tokio::time::advance(Duration::from_secs(1)).await;
        assert!(due_now(&watches, "r"), "due once its wait is over");
    }

    #[tokio::test(start_paused = true)]
    async fn a_capture_started_again_that_ends_is_not_due_but_keeps_its_breaks_for_its_record() {
        let mut watches = Watches::default();
        let (task, first) = capture();
        watches.start("r", task);
        end(&watches, "r", first, End::Broken).await;
        watches.settle(Instant::now()).await;
        let (task, again) = capture();
        watches.start("r", task);
        end(&watches, "r", again, End::Done).await;
        watches.settle(Instant::now()).await;
        tokio::time::advance(Duration::from_hours(1)).await;
        watches.settle(Instant::now()).await;
        assert!(!due_now(&watches, "r"), "nothing is left to capture");
        assert!(watches.take_capture("r").is_none(), "no capture runs");
        assert!(
            watches.is_watched("r"),
            "`observe_again` must not watch it afresh"
        );
        assert!(
            watches.end_breaks("r"),
            "its break is still there for its record"
        );
        assert!(!watches.end_breaks("r"), "a second end finds no break");
    }

    #[tokio::test(start_paused = true)]
    async fn a_capture_taken_after_a_break_leaves_the_break_for_its_record() {
        let mut watches = Watches::default();
        let (task, first) = capture();
        watches.start("r", task);
        end(&watches, "r", first, End::Broken).await;
        watches.settle(Instant::now()).await;
        let (task, _again) = capture();
        watches.start("r", task);
        assert!(
            watches.take_capture("r").is_some(),
            "its live capture is taken"
        );
        assert!(watches.take_capture("r").is_none(), "only once");
        assert!(watches.end_breaks("r"), "the break it had is left to end");

        let (task, _end) = capture();
        watches.start("s", task);
        assert!(
            watches.take_capture("s").is_some(),
            "s's live capture is taken"
        );
        assert!(!watches.end_breaks("s"), "s's capture never broke");
    }

    /// `SplitMix64`: the same seed gives the same sequence on every machine.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            z ^ (z >> 31)
        }

        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
    }

    /// A runner's watch as the engine kept it before `Watches`: a flag on its occupant, a map of captures and a map of
    /// recaptures, each with when it is due; `None` once a capture it started ended without breaking.
    #[derive(Debug, Default)]
    struct Old {
        observed: bool,
        capture: bool,
        recapture: Option<(u32, Option<Instant>)>,
    }

    /// The engine's old `recapture` bookkeeping for one capture that ended `how`, but for a capture started again that
    /// ends without breaking, which leaves its recapture never due instead of due at once.
    fn old_settle(old: &mut Old, how: End, now: Instant) {
        old.capture = false;
        if let End::Broken = how {
            let (breaks, _) = old.recapture.unwrap_or((0, None));
            old.recapture = Some((breaks + 1, Some(now + capture_backoff(breaks + 1))));
        } else if let Some((_, at)) = &mut old.recapture {
            *at = None;
        }
    }

    #[tokio::test(start_paused = true)]
    #[allow(
        clippy::too_many_lines,
        reason = "one arm per engine call, each beside its model of the old fields; split up, they read worse"
    )]
    async fn a_generated_sequence_reads_as_the_flag_and_two_maps_did_but_for_spent_captures() {
        const RUNNERS: [&str; 3] = ["a", "b", "c"];
        // How often the sequences reached what the old fields held at once: a capture beside a recapture, and a
        // release whose record found a break.
        let (mut live_after_breaks, mut recorded_breaks) = (0, 0);
        for seed in 0..64 {
            let mut rng = Rng(seed);
            let mut watches = Watches::default();
            let mut old: BTreeMap<&str, Old> =
                RUNNERS.iter().map(|r| (*r, Old::default())).collect();
            // The ends of each live capture, and how each one that ended, ended, until `settle` reads it.
            let mut ends: BTreeMap<&str, oneshot::Sender<End>> = BTreeMap::new();
            let mut ended: BTreeMap<&str, End> = BTreeMap::new();
            for step in 0..200 {
                let runner = RUNNERS[usize::try_from(rng.below(3)).unwrap()];
                let at = format!("seed {seed} step {step} runner {runner}");
                let model = old.get_mut(runner).unwrap();
                match rng.below(20) {
                    // `observe` and `observe_again`: an inspect that succeeds starts the first capture.
                    0..=2 if !model.observed => {
                        assert!(!watches.is_watched(runner), "{at}: unwatched");
                        model.observed = true;
                        model.capture = true;
                        let (task, end) = capture();
                        watches.start(runner, task);
                        ends.insert(runner, end);
                    }
                    // A capture ends: it breaks off somewhere, ends done, or fails.
                    3..=7 if ends.contains_key(runner) => {
                        let how = match rng.below(5) {
                            0 => End::Done,
                            1 => End::Failed,
                            // As often as before, when a break said where it stored up to, or did not.
                            _ => End::Broken,
                        };
                        let sender = ends.remove(runner).unwrap();
                        end(&watches, runner, sender, how).await;
                        ended.insert(runner, how);
                    }
                    // `recapture`, with an inspect of each runner due that finds it running, stopped, gone or fails.
                    8..=12 => {
                        let now = Instant::now();
                        watches.settle(now).await;
                        for (runner, how) in std::mem::take(&mut ended) {
                            old_settle(old.get_mut(runner).unwrap(), how, now);
                        }
                        for runner in RUNNERS {
                            let model = old.get_mut(runner).unwrap();
                            let at = format!("{at}, recapture of {runner}");
                            let due = model.recapture.is_some_and(|(_, due)| {
                                due.is_some_and(|due| due <= now) && !model.capture
                            });
                            assert_eq!(watches.is_due(runner, now), due, "{at}");
                            if !due {
                                continue;
                            }

                            match rng.below(4) {
                                0 | 1 => {
                                    live_after_breaks += 1;
                                    model.capture = true;
                                    let (task, end) = capture();
                                    watches.start(runner, task);
                                    ends.insert(runner, end);
                                }
                                2 => {
                                    model.recapture = None;
                                    watches.end_breaks(runner);
                                }
                                _ => {}
                            }
                        }
                    }
                    // A release: `drain` takes the capture, which is let end, then `record` and `forget`.
                    13 if model.observed => {
                        assert_eq!(
                            watches.take_capture(runner).is_some(),
                            std::mem::take(&mut model.capture),
                            "{at}: drain"
                        );
                        ends.remove(runner);
                        ended.remove(runner);
                        if rng.below(2) == 0 {
                            let broke = model.recapture.take().is_some();
                            recorded_breaks += usize::from(broke);
                            assert_eq!(watches.end_breaks(runner), broke, "{at}: record");
                            watches.forget(runner);
                            *model = Old::default();
                        }
                    }
                    // A kill or an idle removal: `end_capture`, then `forget`.
                    14 if model.observed => {
                        assert_eq!(
                            watches.take_capture(runner).is_some(),
                            model.capture,
                            "{at}: end_capture"
                        );
                        ends.remove(runner);
                        ended.remove(runner);
                        watches.forget(runner);
                        *model = Old::default();
                    }
                    // A restart: every runner is adopted unwatched.
                    15 => {
                        watches.abort_all();
                        watches = Watches::default();
                        ends.clear();
                        ended.clear();
                        for model in old.values_mut() {
                            *model = Old::default();
                        }
                    }
                    16..=18 => tokio::time::advance(Duration::from_secs(rng.below(40))).await,
                    _ => {}
                }
                for runner in RUNNERS {
                    let model = &old[runner];
                    assert_eq!(
                        matches!(watches.0.get(runner), Some(Watch::Live { .. })),
                        model.capture,
                        "{at}: live {runner}"
                    );
                    assert_eq!(
                        watches.is_watched(runner),
                        model.observed,
                        "{at}: watched {runner}"
                    );
                }
            }
        }
        assert!(
            live_after_breaks > 100,
            "{live_after_breaks} captures started again after a break"
        );
        assert!(
            recorded_breaks > 10,
            "{recorded_breaks} releases found a break"
        );
    }
}
