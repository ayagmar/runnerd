//! What a slot is doing and whether CI keeps up, as every client names and counts it. Plain derivations of
//! [`Status`] and [`Activity`]: a client never infers a slot's state from whether it found the job's details.

use super::{Activity, ProfileStatus, RunnerStatus, SlotStatus, Status, Tone, duration};

/// What a slot is doing, from `Status::slot_state`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlotState {
    /// No runner, and nothing to do.
    Idle,
    /// No runner: slot 1's layers become the profile's snapshot.
    Reseeding,
    /// A runner started for a queued job and waits for GitHub to hand it one.
    Starting {
        /// `owner/name` the runner is registered on.
        repository: String,
    },
    /// A warm runner: no queued job waits for it; it takes the next one GitHub hands out.
    Warm {
        /// `owner/name` the runner is registered on.
        repository: String,
    },
    /// Its runner runs a job.
    Running {
        /// `owner/name` the runner is registered on.
        repository: String,
    },
}

impl SlotState {
    /// What a slot holding `runner` is doing: running its job, warm for its repository, or starting.
    pub fn of_runner(runner: &RunnerStatus) -> SlotState {
        let repository = runner.repository.clone();
        if runner.busy {
            SlotState::Running { repository }
        } else if runner.warm {
            SlotState::Warm { repository }
        } else {
            SlotState::Starting { repository }
        }
    }

    /// One word for it, as a table column says it: `idle`, `reseeding`, `starting`, `warm` or `running`.
    pub fn word(&self) -> &'static str {
        match self {
            SlotState::Idle => "idle",
            SlotState::Reseeding => "reseeding",
            SlotState::Starting { .. } => "starting",
            SlotState::Warm { .. } => "warm",
            SlotState::Running { .. } => "running",
        }
    }

    /// What it does, as a sentence's tail: `idle`, `reseeding the snapshot`, `a runner for o/app waiting for its
    /// job`, `a warm runner for o/app, waiting for a job` or `running a job of o/app`.
    pub fn line(&self) -> String {
        match self {
            SlotState::Idle => "idle".to_owned(),
            SlotState::Reseeding => "reseeding the snapshot".to_owned(),
            SlotState::Starting { repository } => {
                format!("a runner for {repository} waiting for its job")
            }
            SlotState::Warm { repository } => {
                format!("a warm runner for {repository}, waiting for a job")
            }
            SlotState::Running { repository } => format!("running a job of {repository}"),
        }
    }

    /// Whether a runner runs a job in it.
    pub fn is_running(&self) -> bool {
        matches!(self, SlotState::Running { .. })
    }

    /// What it means at a glance: a running slot is a plain fact, every other one background.
    pub fn tone(&self) -> Tone {
        match self {
            SlotState::Running { .. } => Tone::Neutral,
            _ => Tone::Quiet,
        }
    }
}

impl Status {
    /// What `slot` of `profile` is doing: its runner's state as the daemon reports it, or for a free slot, whether its
    /// profile reseeds into it.
    pub fn slot_state(profile: &ProfileStatus, slot: &SlotStatus) -> SlotState {
        match &slot.runner {
            Some(runner) => SlotState::of_runner(runner),
            None if profile.reseeding && slot.slot == 1 => SlotState::Reseeding,
            None => SlotState::Idle,
        }
    }

    /// Whether CI keeps up now: slots running, starting and warm, and the queue with its oldest job's wait at `now`.
    pub fn health(&self, now: u64) -> Health {
        let mut health = Health {
            slots: 0,
            running: 0,
            starting: 0,
            warm: 0,
            waiting: self.queued.len(),
            oldest_wait: self
                .queued
                .iter()
                .filter(|job| job.queued_at > 0)
                .map(|job| now.saturating_sub(job.queued_at))
                .max(),
        };
        for profile in &self.profiles {
            for slot in &profile.slots {
                health.slots += 1;
                match Status::slot_state(profile, slot) {
                    SlotState::Running { .. } => health.running += 1,
                    SlotState::Starting { .. } => health.starting += 1,
                    SlotState::Warm { .. } => health.warm += 1,
                    SlotState::Idle | SlotState::Reseeding => {}
                }
            }
        }
        health
    }
}

/// Whether CI keeps up now, from `Status::health`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Health {
    /// Every configured slot.
    pub slots: usize,
    /// Slots whose runner runs a job.
    pub running: usize,
    /// Slots whose runner waits for the queued job it started for.
    pub starting: usize,
    /// Slots whose warm runner waits for jobs not queued yet.
    pub warm: usize,
    /// Queued jobs no runner holds yet.
    pub waiting: usize,
    /// How long the oldest of them has waited, in seconds; `None` when none says when it was queued.
    pub oldest_wait: Option<u64>,
}

impl Health {
    /// What the queue means: any job waiting is worth a look.
    pub fn waiting_tone(&self) -> Tone {
        if self.waiting > 0 {
            Tone::Warn
        } else {
            Tone::Neutral
        }
    }
}

/// One of the overview's four health figures, worded once for every client: the web dashboard lays them out as
/// tiles, the TUI on one line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Figure {
    /// What it counts: `Running`, `Waiting`, `Wait p95, 24 h` or `Failed, 24 h`.
    pub label: &'static str,
    /// The figure itself.
    pub value: String,
    /// What it is out of, such as `of 6 slots`.
    pub of: Option<String>,
    /// What qualifies it, most telling first.
    pub notes: Vec<String>,
    /// What it means.
    pub tone: Tone,
}

impl Health {
    /// The four figures that say whether CI keeps up: slots running, jobs waiting and the oldest's wait, and from
    /// the last day's `activity`, the wait of the jobs that started and the failures. The last two are left out
    /// until the day is read.
    pub fn figures(&self, activity: Option<&Activity>) -> Vec<Figure> {
        let mut running = Vec::new();
        if self.starting > 0 {
            let s = if self.starting == 1 { "" } else { "s" };
            running.push(format!("{} runner{s} starting", self.starting));
        }
        if self.warm > 0 {
            running.push(format!("{} warm, waiting for jobs", self.warm));
        }
        let waiting = match self.oldest_wait {
            Some(oldest) => vec![format!("oldest {}", duration(oldest))],
            None => Vec::new(),
        };
        let mut figures = vec![
            Figure {
                label: "Running",
                value: self.running.to_string(),
                of: Some(format!("of {} slots", self.slots)),
                notes: running,
                tone: Tone::Neutral,
            },
            Figure {
                label: "Waiting",
                value: self.waiting.to_string(),
                of: None,
                notes: waiting,
                tone: self.waiting_tone(),
            },
        ];
        if let Some(day) = activity {
            let started = day.started.len();
            figures.push(Figure {
                label: "Wait p95, 24 h",
                value: day.wait_p95().map_or_else(|| "–".to_owned(), duration),
                of: None,
                notes: vec![format!(
                    "{started} job{} started",
                    if started == 1 { "" } else { "s" }
                )],
                tone: Tone::Neutral,
            });
            let runner =
                (day.runner_failed > 0).then(|| format!("{} by their runner", day.runner_failed));
            figures.push(Figure {
                label: "Failed, 24 h",
                value: day.failed.to_string(),
                of: Some(format!("of {} finished", day.finished)),
                notes: runner.into_iter().collect(),
                tone: day.failed_tone(),
            });
        }
        figures
    }
}

impl Activity {
    /// What the day's failures mean: bad only when a runner failed, since a job's own failing steps are its
    /// repository's.
    pub fn failed_tone(&self) -> Tone {
        if self.runner_failed > 0 {
            Tone::Bad
        } else {
            Tone::Neutral
        }
    }
}

#[cfg(test)]
mod tests;
