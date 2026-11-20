//! Which one pool goes first once memory or CPU keeps running out before its next job's runner fits (ADR 0096): how
//! long each pool's next job waited, whether a barrier has served the pool (ADR 0098), what an empty slice would hold,
//! the rule that picks a plan's one barrier, and which younger jobs may start behind it all the same (`Backfill`, ADRs
//! 0097 and 0151). `plan` and `admission::simulate` both decide by it, so a replay shows what the daemon does. It does
//! no I/O and reads no clock; the engine keeps the waits (`waits`), and it never stops or clamps what already runs.

use std::time::{Duration, SystemTime};

use super::waits::Waits;
use super::{Left, signed};
use crate::api::Waiting;

/// Once a pool's next job has waited this long for memory or CPU, no younger job starts until its runner fits: smaller
/// jobs would otherwise take everything that frees up, and the room a large one needs would never gather. Long enough
/// that a passing shortage puts up no barrier, whose cost is what stands idle behind it (ADR 0098).
pub const PRIORITY_AFTER: Duration = Duration::from_secs(600);

/// `PRIORITY_AFTER` for a pool a barrier has served, until its queue empties: its wait restarts with each job, so with
/// the first limit a pool of many large jobs could run them only one at a time (ADR 0098).
pub const SERVED_PRIORITY_AFTER: Duration = Duration::from_secs(120);

/// A job of another profile whose pool usually runs no longer than this may start behind a barrier whatever the running
/// runners' ends say, while the owner's room has not come (ADR 0151): a short job gives back what it took before waiting
/// could have gathered much, so if its run takes what the plan read it costs the owner this at most. As long as a served
/// pool's job waits before it may hold (`SERVED_PRIORITY_AFTER`), and longer than most light jobs' p95.
pub const SHORT_RUN: Duration = Duration::from_secs(120);

/// How fresh the usual run times a plan reads are.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum RunTimes {
    /// Read from the history less than a minute ago, or as a replay goes.
    #[default]
    Fresh,
    /// The history could not be read again, and an older reading stands. A pool's runs may have grown longer since, so
    /// no job passes a barrier as a short one (ADR 0151); 0097 still goes by them.
    Stale,
}

/// What a job's runner lacks that a running job gives back.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Scarce {
    /// The slice's memory budget, or what the host's memory leaves it.
    Memory,
    /// The CPU budget.
    Cpu,
}

/// What the budgets would leave once every runner ended, the mirror, seed containers and shells still there; `None`,
/// no such bound known. A job holds younger ones only for room that draining can give it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Drained {
    /// The slice's memory.
    pub memory: Option<i64>,
    /// The CPU budget, in millicores.
    pub cpu: Option<i64>,
    /// What the host's memory leaves the slice for a job the host holds: by the larger margin while a runner runs,
    /// whichever margin is in force (ADR 0102).
    pub host: Option<i64>,
    /// What it leaves for a job something else holds, by the plain margin: such a job's wait does not follow the margin,
    /// so its barrier need not keep off it.
    pub host_plain: Option<i64>,
}

impl Drained {
    /// Whether a runner of `memory` and `cpu`, of a job `held` so, fits, every known bound counted.
    fn holds(self, memory: u64, cpu: u64, held: Held) -> bool {
        let fits = |room: Option<i64>, needs: u64| {
            room.is_none_or(|room| room >= i64::try_from(needs).unwrap_or(i64::MAX))
        };
        let host = match held {
            Held::ByHost => self.host,
            Held::ByBudget => self.host_plain,
        };
        fits(self.memory, memory) && fits(host, memory) && fits(self.cpu, cpu)
    }
}

/// What holds a job back, which says by which margin a drain serves it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Held {
    /// What the host's memory leaves the slice.
    ByHost,
    /// The slice's memory budget, its high-water mark or the CPU budget.
    ByBudget,
}

/// A job a plan held for something scarce, with a free slot for its runner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Short {
    /// Its ID.
    pub job: u64,
    /// What it lacks first: memory before CPU, as `Waiting` says it.
    pub scarce: Scarce,
    /// Whether the host holds it.
    pub held: Held,
    /// What its runner takes of the memory budget.
    pub memory: u64,
    /// What its runner takes of the CPU budget, in millicores.
    pub cpu: u64,
}

/// The one job a plan lets no younger job start before, whichever of memory and CPU it lacks: it drains towards both.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Barrier {
    /// The job's ID.
    pub job: u64,
    /// What it lacks now.
    pub scarce: Scarce,
    /// What its runner takes of the memory budget.
    pub memory: u64,
    /// What its runner takes of the CPU budget, in millicores.
    pub cpu: u64,
    /// When its wait reached its limit: from then on, what starts counts against the room it is owed.
    pub since: SystemTime,
}

impl Barrier {
    /// The barrier `short`, whose pool `waits`, puts up: the first of a plan's jobs that waited long enough and that
    /// `drained` has room for. `None` from a demand draining could not serve, as a runner past the budget or a host
    /// short because of programs outside the slice: younger jobs that fit go on. `drained` is `None` while a safety
    /// interlock starts nothing, whose zeros say nothing of the room there is: the wait keeps its age meanwhile.
    pub fn of(
        short: Short,
        waits: &Waits,
        drained: Option<Drained>,
        now: SystemTime,
    ) -> Option<Barrier> {
        let drained = drained?;
        let since = waits.matured(short.job, now)?;
        let holds = drained.holds(short.memory, short.cpu, short.held);
        holds.then_some(Barrier {
            job: short.job,
            scarce: short.scarce,
            memory: short.memory,
            cpu: short.cpu,
            since,
        })
    }

    /// Why a younger job waits behind it.
    pub fn reason(self) -> Waiting {
        match self.scarce {
            Scarce::Memory => Waiting::BehindMemory { job: self.job },
            Scarce::Cpu => Waiting::BehindCpu { job: self.job },
        }
    }
}

/// What a runner may take at most: its container's limits, its profile's caps when it started. A reload changes a
/// profile's caps for the runners it starts next, not for those that run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Caps {
    /// Memory.
    pub memory: u64,
    /// CPU, in millicores.
    pub cpu: u64,
}

/// A runner as a backfill reads it, one in a slot or one the plan already started.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Runner<'plan> {
    /// Its profile.
    pub(crate) profile: &'plan str,
    /// When it started.
    pub(crate) started_at: SystemTime,
    /// How long its job usually keeps it; `None`, it ends now as far as a backfill knows: it runs no job yet, or one
    /// with no estimate. That only makes the room come sooner, and less starts behind the barrier.
    pub(crate) usual: Option<Duration>,
    /// What it takes of the memory budget now.
    pub(crate) memory: u64,
    /// What it takes of the CPU budget now, in millicores.
    pub(crate) cpu: u64,
    /// What it could take: the caps it started with.
    pub(crate) caps: Caps,
    /// Whether it started behind a barrier (ADR 0131): a warm runner, one started for a job older than the owner and
    /// one a restart adopted did not.
    pub(crate) passed: bool,
}

impl Runner<'_> {
    /// How long from `now` it usually ends: at once if that time has passed, or if it started in the future, as after
    /// a clock went back: the room coming sooner is the side that starts less behind a barrier.
    fn left_to_run(&self, now: SystemTime) -> Duration {
        let Ok(ran) = now.duration_since(self.started_at) else {
            return Duration::ZERO;
        };
        self.usual
            .map_or(Duration::ZERO, |usual| usual.saturating_sub(ran))
    }
}

/// A job older than a barrier's owner that the plan left waiting. It goes before the owner, so room it loses to a
/// younger job comes back to the owner late.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Older<'plan> {
    /// Its pool's profile.
    pub(crate) profile: &'plan str,
    /// Whether that profile has a free slot, or will have one with no runner ending, as when it is preparing.
    pub(crate) free_slot: bool,
    /// What its runner takes of the memory budget.
    pub(crate) memory: u64,
    /// What its runner takes of the CPU budget, in millicores.
    pub(crate) cpu: u64,
}

/// What a plan knows when a barrier goes up.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Around<'plan> {
    /// The budgets left by the plain host margin, counted by it (`Budget::plain`): which jobs pass then does not
    /// depend on the band they move (ADR 0151). Under the larger margin the room comes later, so this lets less pass.
    pub(crate) left: Left,
    /// How fresh the usual run times are.
    pub(crate) run_times: RunTimes,
    /// How much more memory the pooled count could leave with no runner ending (`Budget::credit_room`).
    pub(crate) credit_room: u64,
    /// How much more the host's limit could leave with no runner ending (`Budget::host_credit_room`).
    pub(crate) host_credit_room: u64,
    /// What a drain leaves.
    pub(crate) drained: Drained,
    /// The runners in slots, and those the plan started so far.
    pub(crate) runners: &'plan [Runner<'plan>],
    /// The jobs older than the owner it left waiting.
    pub(crate) older: &'plan [Older<'plan>],
}

/// A younger job a plan would start, had it no barrier to wait behind.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Candidate<'plan> {
    /// Its pool's profile.
    pub(crate) profile: &'plan str,
    /// How many of that profile's slots are free.
    pub(crate) free_slots: usize,
    /// The largest usual run time of its pool's queued jobs, since GitHub may give the runner any of them; `None` if
    /// any has no estimate (`admission::Predictor::longest_usual`).
    pub(crate) usual: Option<Duration>,
    /// Its profile's caps.
    pub(crate) caps: Caps,
}

/// The room behind a barrier, as ADRs 0097, 0131 and 0151 bound it. If each run takes what the plan read, a job that
/// passes by its run time (0097) has ended before the owner or any job older than it could start, so nothing they see
/// changes and an owner that keeps the barrier starts no later than with no backfill. A short job of another profile
/// (0151) passes while the owner's room has not come, even with no time known for it, if it ends before any older job a
/// drain serves could start, and costs the owner at most what it has left to run once the room comes: nothing more
/// passes as a short job once only what passed keeps the owner out. Both read the host by its plain margin, so which
/// jobs pass does not depend on the band they move. The plan reads p95s, so a run that ends early can bring the room while a job started here still
/// runs, and that job can cost the owner what it has left. Whatever the run times do, what passed a barrier since this
/// one matured cannot take, even at its caps, what an empty slice has for the owner and the older jobs, nor the last
/// free slot of its profile, so the owner fits once the older jobs and what ran before have ended.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Backfill<'plan> {
    /// The owner's profile.
    profile: &'plan str,
    /// How long from now until the running runners' usual ends let the owner or an older job start; `None` where no
    /// such time is ahead: the room may come at any moment, or no runner's end brings it.
    until: Option<Duration>,
    /// How long a short job may usually run to pass (ADR 0151): at most `SHORT_RUN`, and no longer than until an older
    /// job a drain serves could start, as under 0097. `None` while the owner's room is there but for what passed a
    /// barrier, while one of those older jobs could start now, or while the run times are stale.
    short: Option<Duration>,
    /// What a drain leaves past the owner and the caps of what passed a barrier since this one matured.
    spare: Left,
}

/// The runners by how long each usually has left, soonest first.
struct Ending<'plan>(Vec<(Duration, Runner<'plan>)>);

impl Ending<'_> {
    /// How long until their ends let a runner of `memory` and `cpu` fit `left`; `None` if none does.
    fn room_in(&self, left: Left, memory: u64, cpu: u64) -> Option<Duration> {
        if left.fits(memory, cpu) {
            return Some(Duration::ZERO);
        }
        let mut room = left;
        self.0.iter().find_map(|(left_to_run, runner)| {
            room.give(runner.memory, runner.cpu);
            room.fits(memory, cpu).then_some(*left_to_run)
        })
    }

    /// How long until `older` could start, the budgets being `left`; `None` if no end lets it. Older jobs beside it may
    /// well take that room first, so it may start later, never sooner.
    fn start_of(&self, older: &Older<'_>, left: Left) -> Option<Duration> {
        let room = self.room_in(left, older.memory, older.cpu)?;
        let in_profile = |(_, runner): &&(Duration, Runner<'_>)| runner.profile == older.profile;
        let slot = if older.free_slot {
            Duration::ZERO
        } else {
            self.0.iter().find(in_profile)?.0
        };
        Some(room.max(slot))
    }
}

impl<'plan> Backfill<'plan> {
    /// What the `barrier` of a job of `profile` leaves room for, as things stand `around` it. `None` where nothing may
    /// start behind it: no time is known before which the owner's room comes, and it or an older job's room is there but
    /// for what passed a barrier.
    pub(crate) fn behind(
        barrier: Barrier,
        profile: &'plan str,
        around: Around<'_>,
        now: SystemTime,
    ) -> Option<Backfill<'plan>> {
        let Around {
            mut left,
            run_times,
            credit_room,
            host_credit_room,
            drained,
            runners,
            older,
        } = around;
        // Room that is there now but for what passed: the owner takes it once that ends, so a short job must not take it
        // first. A credit that may come is not there yet; the next plan reads it. An older job's room needs no such
        // check: a short job ends before any of them could start.
        let mut freed = left;
        for runner in runners.iter().filter(|runner| runner.passed) {
            freed.give(runner.memory, runner.cpu);
        }
        let waiting_for_passes = freed.fits(barrier.memory, barrier.cpu);
        // The room may come sooner than ends bring it: either credit can grow at any moment, and an end gives back at
        // most what its runner counts. So the time found is the earliest the owner could start. A runner this plan
        // started has no job yet, so it counts as ending now and gives back its whole take at once: what it may later
        // add to the host's credit is its headroom, never more than that take, so `host_credit_room` need not count it.
        left.memory = left
            .memory
            .map(|memory| memory.saturating_add(signed(credit_room)));
        left.host = left
            .host
            .map(|host| host.saturating_add(signed(host_credit_room)));
        let mut ending: Vec<(Duration, Runner<'_>)> = runners
            .iter()
            .map(|runner| (runner.left_to_run(now), *runner))
            .collect();
        ending.sort_by_key(|(left_to_run, _)| *left_to_run);
        let ending = Ending(ending);
        let until = ending
            .room_in(left, barrier.memory, barrier.cpu)
            .map(|owner| {
                let starts = older.iter().filter_map(|job| ending.start_of(job, left));
                starts.fold(owner, Duration::min)
            })
            .filter(|until| !until.is_zero());
        let mut spare = Left {
            memory: drained.memory,
            cpu: drained.cpu,
            host: drained.host,
        };
        // An older job that matures takes the barrier, so its room is kept as the owner's is. One no drain serves never
        // will, and must not stop everything behind the barrier.
        let served = older.iter().filter(|job| spare.fits(job.memory, job.cpu));
        // A short job that delays an older one delays what that one gives back, and the next older one's start after
        // it: so it ends before any of them could start, as 0097 asks. One whose start no end brings may start at any
        // moment. Its start is read by the plain margin, so any older job a drain by the plain margin serves counts,
        // though only those the drain in force serves keep room past the owner.
        let plain_drain = Left {
            host: drained.host_plain.or(drained.host),
            ..spare
        };
        let short = older
            .iter()
            .filter(|job| plain_drain.fits(job.memory, job.cpu))
            .map(|job| ending.start_of(job, left).unwrap_or(Duration::ZERO))
            .fold(SHORT_RUN, Duration::min);
        let short = (run_times == RunTimes::Fresh && !waiting_for_passes && !short.is_zero())
            .then_some(short);
        if until.is_none() && short.is_none() {
            return None;
        }
        let memory = served.clone().map(|job| job.memory).max().unwrap_or(0);
        let cpu = served.map(|job| job.cpu).max().unwrap_or(0);
        spare.take(barrier.memory.max(memory), barrier.cpu.max(cpu));
        // A runner started for an older job, or before T0, is one the owner waits out under a strict barrier too. One
        // that passed counts at its caps, as it may hang and grow to them, whether or not GitHub gave it a job: an idle
        // one holds its room until `IDLE_TIMEOUT`.
        for runner in runners
            .iter()
            .filter(|runner| runner.passed && runner.started_at >= barrier.since)
        {
            spare.take(runner.caps.memory, runner.caps.cpu);
        }
        Some(Backfill {
            profile,
            until,
            short,
            spare,
        })
    }

    /// Whether `candidate` may start behind the barrier once it fits the budgets left and has a free slot: it usually
    /// ends before the owner's room comes (ADR 0097), or it is a short job of another profile while that room has not
    /// come (ADR 0151); either way its caps leave the owner its room. A job that may pass but does not fit waits for
    /// what it lacks, not for the barrier.
    pub(crate) fn passes(&self, candidate: Candidate<'_>) -> bool {
        let Some(usual) = candidate.usual else {
            return false;
        };
        let other_profile = candidate.profile != self.profile;
        let in_time = self.until.is_some_and(|until| usual <= until)
            && (other_profile || candidate.free_slots > 1);
        let short = other_profile && self.short.is_some_and(|most| usual <= most);
        (in_time || short) && self.spare.fits(candidate.caps.memory, candidate.caps.cpu)
    }

    /// Counts the caps of `candidate`, which `passes` let start: they need not move the time found, as a runner that
    /// usually ends by then gives back what it took.
    pub(crate) fn admit(&mut self, candidate: Candidate<'_>) {
        self.spare.take(candidate.caps.memory, candidate.caps.cpu);
    }
}
