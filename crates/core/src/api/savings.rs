//! What this machine's job minutes would have cost on GitHub-hosted runners (ADR 0072): GitHub's Linux per-minute
//! rates, what each job bills, which jobs GitHub would not have billed, and every word clients show of it. Pure: the
//! daemon reads the history and the config and passes them in; it does not price, and clients do not reword.

use std::collections::BTreeMap;
use std::fmt::{self, Write as _};

use serde::{Deserialize, Serialize};

use super::JobState;
use super::units::{civil_from_days, days_from_civil, short_date};

/// What a minute of a job costs on this machine, in micro-dollars, subtracted from each hosted rate. GitHub
/// postponed its $0.002 a minute charge for self-hosted runners; this is the one place to set it if it comes back.
pub const SELF_HOSTED_RATE: u64 = 0;

/// The last 30 days' length, in seconds.
const THIRTY_DAYS: u64 = 30 * 86_400;

/// An amount in US dollars, as whole micro-dollars, so no float carries money.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Usd(pub u64);

impl fmt::Display for Usd {
    /// To the cent, rounded down, so a figure never says more than the jobs cost: `$7.40`, `$1,204.18`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let cents = self.0 / 10_000;
        write!(f, "${}.{:02}", thousands(cents / 100), cents % 100)
    }
}

/// A GitHub-hosted Linux runner, by the billing SKU GitHub's pricing page and usage report name it with, as a
/// profile's `priced_as` takes it. Windows, macOS and GPU runners are not here: runnerd runs Linux containers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Sku {
    /// Linux 1-core x64, a standard runner.
    #[serde(rename = "actions_linux_slim")]
    ActionsLinuxSlim,
    /// Linux 2-core x64, the standard runner `ubuntu-latest` gives a private repository.
    #[serde(rename = "actions_linux")]
    ActionsLinux,
    /// Linux 2-core arm64, a standard runner.
    #[serde(rename = "actions_linux_arm")]
    ActionsLinuxArm,
    /// Linux Advanced 2-core x64, a larger runner.
    #[serde(rename = "linux_2_core_advanced")]
    Linux2CoreAdvanced,
    /// Linux 4-core x64.
    #[serde(rename = "linux_4_core")]
    Linux4Core,
    /// Linux 8-core x64.
    #[serde(rename = "linux_8_core")]
    Linux8Core,
    /// Linux 16-core x64.
    #[serde(rename = "linux_16_core")]
    Linux16Core,
    /// Linux 32-core x64.
    #[serde(rename = "linux_32_core")]
    Linux32Core,
    /// Linux 64-core x64.
    #[serde(rename = "linux_64_core")]
    Linux64Core,
    /// Linux 96-core x64.
    #[serde(rename = "linux_96_core")]
    Linux96Core,
    /// Linux 2-core arm64, a larger runner.
    #[serde(rename = "linux_2_core_arm")]
    Linux2CoreArm,
    /// Linux 4-core arm64.
    #[serde(rename = "linux_4_core_arm")]
    Linux4CoreArm,
    /// Linux 8-core arm64.
    #[serde(rename = "linux_8_core_arm")]
    Linux8CoreArm,
    /// Linux 16-core arm64.
    #[serde(rename = "linux_16_core_arm")]
    Linux16CoreArm,
    /// Linux 32-core arm64.
    #[serde(rename = "linux_32_core_arm")]
    Linux32CoreArm,
    /// Linux 64-core arm64.
    #[serde(rename = "linux_64_core_arm")]
    Linux64CoreArm,
}

impl Sku {
    /// The 2-core standard runner of an architecture as Rust names it (`std::env::consts::ARCH`): arm64's on
    /// `aarch64`, x64's on any other.
    pub fn standard_for(arch: &str) -> Sku {
        if arch == "aarch64" {
            Sku::ActionsLinuxArm
        } else {
            Sku::ActionsLinux
        }
    }

    /// Its per-minute rate on GitHub's pricing page, read 2026-10-26.
    pub fn rate(self) -> Usd {
        Usd(match self {
            Sku::ActionsLinuxSlim => 2_000,
            Sku::ActionsLinux | Sku::Linux2CoreAdvanced => 6_000,
            Sku::ActionsLinuxArm | Sku::Linux2CoreArm => 5_000,
            Sku::Linux4Core => 12_000,
            Sku::Linux8Core => 22_000,
            Sku::Linux16Core => 42_000,
            Sku::Linux32Core => 82_000,
            Sku::Linux64Core => 162_000,
            Sku::Linux96Core => 252_000,
            Sku::Linux4CoreArm => 8_000,
            Sku::Linux8CoreArm => 14_000,
            Sku::Linux16CoreArm => 26_000,
            Sku::Linux32CoreArm => 50_000,
            Sku::Linux64CoreArm => 98_000,
        })
    }

    /// Whether it is a standard runner: free in public repositories and for Dependabot and Pages. Larger runners
    /// are billed everywhere.
    pub fn standard(self) -> bool {
        matches!(
            self,
            Sku::ActionsLinuxSlim | Sku::ActionsLinux | Sku::ActionsLinuxArm
        )
    }

    /// GitHub's name for it, as `priced_as` takes it: `actions_linux`.
    pub fn key(self) -> &'static str {
        match self {
            Sku::ActionsLinuxSlim => "actions_linux_slim",
            Sku::ActionsLinux => "actions_linux",
            Sku::ActionsLinuxArm => "actions_linux_arm",
            Sku::Linux2CoreAdvanced => "linux_2_core_advanced",
            Sku::Linux4Core => "linux_4_core",
            Sku::Linux8Core => "linux_8_core",
            Sku::Linux16Core => "linux_16_core",
            Sku::Linux32Core => "linux_32_core",
            Sku::Linux64Core => "linux_64_core",
            Sku::Linux96Core => "linux_96_core",
            Sku::Linux2CoreArm => "linux_2_core_arm",
            Sku::Linux4CoreArm => "linux_4_core_arm",
            Sku::Linux8CoreArm => "linux_8_core_arm",
            Sku::Linux16CoreArm => "linux_16_core_arm",
            Sku::Linux32CoreArm => "linux_32_core_arm",
            Sku::Linux64CoreArm => "linux_64_core_arm",
        }
    }

    /// The runner in words: `2-core Linux`, `8-core Linux arm64`.
    pub fn runner(self) -> &'static str {
        match self {
            Sku::ActionsLinuxSlim => "1-core Linux",
            Sku::ActionsLinux => "2-core Linux",
            Sku::ActionsLinuxArm => "2-core Linux arm64",
            Sku::Linux2CoreAdvanced => "Advanced 2-core Linux",
            Sku::Linux4Core => "4-core Linux",
            Sku::Linux8Core => "8-core Linux",
            Sku::Linux16Core => "16-core Linux",
            Sku::Linux32Core => "32-core Linux",
            Sku::Linux64Core => "64-core Linux",
            Sku::Linux96Core => "96-core Linux",
            Sku::Linux2CoreArm => "2-core Linux arm64 (larger runner)",
            Sku::Linux4CoreArm => "4-core Linux arm64",
            Sku::Linux8CoreArm => "8-core Linux arm64",
            Sku::Linux16CoreArm => "16-core Linux arm64",
            Sku::Linux32CoreArm => "32-core Linux arm64",
            Sku::Linux64CoreArm => "64-core Linux arm64",
        }
    }
}

impl fmt::Display for Sku {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.key())
    }
}

/// A repository's visibility as GitHub gives it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Visibility {
    /// Standard runners are free.
    Public,
    /// Billed.
    Private,
    /// An enterprise's: billed as private.
    Internal,
}

/// The runner each profile's jobs are priced at, from the config running now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pricing {
    /// Each profile's `priced_as`, by profile; a profile without one is at `default_sku`.
    pub profiles: BTreeMap<String, Sku>,
    /// The runner of a profile that names none, or is gone from the config.
    pub default_sku: Sku,
}

impl Pricing {
    /// The runner a job of `profile` is priced at.
    pub fn of(&self, profile: Option<&str>) -> Sku {
        profile
            .and_then(|profile| self.profiles.get(profile))
            .copied()
            .unwrap_or(self.default_sku)
    }
}

/// A job of the history as pricing reads it: one a runner of this machine took, completed or lost. Times are
/// milliseconds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PricedRun {
    /// `owner/name`.
    pub repository: String,
    /// The workflow's name.
    pub workflow: Option<String>,
    /// The workflow's `runs-on` labels.
    pub labels: Vec<String>,
    /// `Completed` or `Lost`; others are not counted.
    pub state: JobState,
    /// The profile its runner ran in; `None` without a runner row.
    pub profile: Option<String>,
    /// When it was queued.
    pub queued_at: u64,
    /// When it started.
    pub started_at: Option<u64>,
    /// When it ended.
    pub finished_at: Option<u64>,
}

impl PricedRun {
    /// The time it is dated by: its end, or without one its start, else its queue time. The store selects by the
    /// same.
    pub fn at(&self) -> u64 {
        self.finished_at
            .or(self.started_at)
            .unwrap_or(self.queued_at)
    }

    /// Whether its workflow is named as the ones GitHub generates for Dependabot updates and Pages builds, which
    /// it runs free on a standard runner. A name is not proof, as any workflow may take it, so such a job is not
    /// priced, and not called free either.
    fn named_like_service(&self) -> bool {
        self.workflow.as_deref().is_some_and(|workflow| {
            let workflow = workflow.to_ascii_lowercase();
            workflow == "dependabot updates"
                || workflow == "pages build and deployment"
                || workflow.starts_with("dynamic/dependabot/")
                || workflow.starts_with("dynamic/pages/")
        })
    }
}

/// What pricing reads of the history.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PricingHistory {
    /// The completed and lost jobs dated from [`reads_since`] on.
    pub runs: Vec<PricedRun>,
    /// Each repository's visibility as GitHub last gave it, by repository.
    pub visibility: BTreeMap<String, Visibility>,
    /// Since when, in milliseconds, the history holds every final job: when it started, or the cutoff of its last
    /// prune of job records.
    pub kept_since: u64,
}

/// From when, in milliseconds, [`Savings::of`] reads the history at `now` (ms): the earlier of the month's start and
/// 30 days before.
pub fn reads_since(now: u64) -> u64 {
    month_start(now / 1000)
        .min((now / 1000).saturating_sub(THIRTY_DAYS))
        .saturating_mul(1000)
}

/// The start of the UTC calendar month of `at`, in seconds.
fn month_start(at: u64) -> u64 {
    let (year, month, _) = civil_from_days(at / 86_400);
    days_from_civil(year, month, 1) * 86_400
}

/// `GET /v1/savings`: what this machine's job minutes would have cost on GitHub-hosted runners.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Savings {
    /// When it was read, in seconds.
    pub until: u64,
    /// The UTC calendar month so far.
    pub month: SavedWindow,
    /// The last 30 days.
    pub last_30_days: SavedWindow,
}

/// Which window a [`SavedWindow`] covers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Period {
    /// The UTC calendar month so far: GitHub resets usage at the start of each month.
    Month,
    /// The 30 days to now.
    Last30Days,
}

/// The jobs of one window and what they would have cost.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedWindow {
    /// Which window.
    pub period: Period,
    /// Its start, in seconds: the window's, or when the history starts holding every job if that is later.
    pub since: u64,
    /// Whether the history starts after the window's start, so `since` is when it does.
    pub cut: bool,
    /// The jobs priced.
    pub jobs: u64,
    /// Their minutes, each job's rounded up to a whole minute.
    pub minutes: u64,
    /// What they would have cost, before any minutes the account's plan includes.
    pub cost: Usd,
    /// The same by runner priced at, by SKU.
    pub runners: Vec<PricedRunner>,
    /// Jobs GitHub would have run free: on a standard runner, of a public repository.
    pub free: u64,
    /// Jobs on a standard runner whose workflow is named as GitHub's Dependabot or Pages workflow, which GitHub runs
    /// free: not priced, as the name does not prove it.
    pub services: u64,
    /// Jobs of repositories whose visibility GitHub has not given: not priced.
    pub unknown: u64,
    /// Lost jobs: not priced, as their end is when runnerd saw their runner gone.
    pub lost: u64,
    /// Completed jobs without a start or an end: not priced.
    pub untimed: u64,
}

/// A window's jobs priced at one runner.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PricedRunner {
    /// The runner.
    pub sku: Sku,
    /// Its jobs.
    pub jobs: u64,
    /// Their minutes.
    pub minutes: u64,
    /// What they would have cost.
    pub cost: Usd,
}

/// What GitHub would have done with one job.
enum Priced {
    /// Billed this many minutes at this runner.
    Billed {
        sku: Sku,
        minutes: u64,
    },
    Free,
    Service,
    Unknown,
    Lost,
    Untimed,
}

/// How many minutes GitHub bills a job that ran from `started` to `finished` (ms): each partial minute rounded up,
/// and at least one.
pub fn billed_minutes(started: u64, finished: u64) -> u64 {
    finished.saturating_sub(started).div_ceil(60_000).max(1)
}

fn price(run: &PricedRun, history: &PricingHistory, pricing: &Pricing) -> Option<Priced> {
    match run.state {
        JobState::Completed => {}
        JobState::Lost => return Some(Priced::Lost),
        JobState::Queued | JobState::Running | JobState::Elsewhere => return None,
    }
    let (Some(started), Some(finished)) = (run.started_at, run.finished_at) else {
        return Some(Priced::Untimed);
    };
    let sku = pricing.of(run.profile.as_deref());
    if sku.standard() && run.named_like_service() {
        return Some(Priced::Service);
    }
    Some(match history.visibility.get(&run.repository) {
        None => Priced::Unknown,
        Some(Visibility::Public) if sku.standard() => Priced::Free,
        Some(_) => Priced::Billed {
            sku,
            minutes: billed_minutes(started, finished),
        },
    })
}

impl Savings {
    /// What the jobs of `history` would have cost at `pricing`, at `now` (ms).
    pub fn of(history: &PricingHistory, pricing: &Pricing, now: u64) -> Savings {
        let until = now / 1000;
        Savings {
            until,
            month: SavedWindow::of(Period::Month, month_start(until), history, pricing, now),
            last_30_days: SavedWindow::of(
                Period::Last30Days,
                until.saturating_sub(THIRTY_DAYS),
                history,
                pricing,
                now,
            ),
        }
    }
    /// The windows to show, the month first. The last 30 days are left out when they start where the month does,
    /// as when the history is younger than both: they hold the same jobs.
    pub fn windows(&self) -> Vec<&SavedWindow> {
        if self.last_30_days.since == self.month.since {
            vec![&self.month]
        } else {
            vec![&self.month, &self.last_30_days]
        }
    }
}

impl SavedWindow {
    /// The window from `start` (seconds) to `now` (ms).
    fn of(
        period: Period,
        start: u64,
        history: &PricingHistory,
        pricing: &Pricing,
        now: u64,
    ) -> SavedWindow {
        let kept_since = history.kept_since / 1000;
        let cut = kept_since > start;
        let mut window = SavedWindow {
            period,
            since: start.max(kept_since),
            cut,
            jobs: 0,
            minutes: 0,
            cost: Usd(0),
            runners: Vec::new(),
            free: 0,
            services: 0,
            unknown: 0,
            lost: 0,
            untimed: 0,
        };
        let mut runners: BTreeMap<Sku, PricedRunner> = BTreeMap::new();
        let from = start.saturating_mul(1000).max(history.kept_since);
        for run in history
            .runs
            .iter()
            .filter(|run| (from..=now).contains(&run.at()))
        {
            match price(run, history, pricing) {
                None => {}
                Some(Priced::Billed { sku, minutes }) => {
                    let cost =
                        minutes.saturating_mul(sku.rate().0.saturating_sub(SELF_HOSTED_RATE));
                    let runner = runners.entry(sku).or_insert(PricedRunner {
                        sku,
                        jobs: 0,
                        minutes: 0,
                        cost: Usd(0),
                    });
                    runner.jobs += 1;
                    runner.minutes += minutes;
                    runner.cost.0 += cost;
                    window.jobs += 1;
                    window.minutes += minutes;
                    window.cost.0 += cost;
                }
                Some(Priced::Free) => window.free += 1,
                Some(Priced::Service) => window.services += 1,
                Some(Priced::Unknown) => window.unknown += 1,
                Some(Priced::Lost) => window.lost += 1,
                Some(Priced::Untimed) => window.untimed += 1,
            }
        }
        window.runners = runners.into_values().collect();
        window
    }

    /// The jobs of the window not in its figure, free or not priced.
    pub fn not_priced(&self) -> u64 {
        self.free + self.services + self.unknown + self.lost + self.untimed
    }

    /// The figure with what it is before, for compact places, which show it whole or not at all: "This month: 1,234
    /// job-minutes here, about $7.40 at GitHub's 2-core Linux rate before your plan's free minutes (3 jobs not
    /// priced)". What it rests on, today's profiles and repository visibility, is the last of `details`.
    pub fn headline(&self) -> String {
        let when = if self.cut {
            format!("Since {}", short_date(self.since))
        } else {
            match self.period {
                Period::Month => "This month".to_owned(),
                Period::Last30Days => "Last 30 days".to_owned(),
            }
        };
        if self.jobs == 0 && self.not_priced() == 0 {
            return format!("{when}: no job-minutes here yet");
        }
        let rate = match self.runners.as_slice() {
            [one] => format!("at GitHub's {} rate", one.sku.runner()),
            [] => "at GitHub's rates".to_owned(),
            _ => "at GitHub's rates for each profile's runner".to_owned(),
        };
        let mut line = format!(
            "{when}: {} here, about {} {rate} before your plan's free minutes",
            count(self.minutes, "job-minute", "job-minutes"),
            self.cost,
        );
        if self.not_priced() > 0 {
            let _ = write!(
                line,
                " ({} not priced)",
                count(self.not_priced(), "job", "jobs")
            );
        }
        line
    }

    /// What the headline sums and leaves out, a line each: the runners priced at, the jobs not priced and why, and
    /// what the figure rests on.
    pub fn details(&self) -> Vec<String> {
        let mut lines: Vec<String> = self
            .runners
            .iter()
            .map(|runner| {
                format!(
                    "{} of {} at {} ({}, {} a minute): {}",
                    count(runner.minutes, "minute", "minutes"),
                    count(runner.jobs, "job", "jobs"),
                    runner.sku.key(),
                    runner.sku.runner(),
                    rate(runner.sku.rate()),
                    runner.cost,
                )
            })
            .collect();
        for (jobs, why) in [
            (
                self.free,
                "free on GitHub: on a standard runner, of a public repository",
            ),
            (
                self.services,
                "not priced: their workflow is named as GitHub's Dependabot or Pages workflow, free on a standard \
                 runner",
            ),
            (
                self.unknown,
                "not priced: GitHub has not said whether their repository is public",
            ),
            (
                self.lost,
                "not priced: lost, so how long they ran is not known",
            ),
            (self.untimed, "not priced: no start or end recorded"),
        ] {
            if jobs > 0 {
                lines.push(format!("{} {why}", count(jobs, "job", "jobs")));
            }
        }
        if self.cut {
            lines.push(format!(
                "The history keeps jobs since {}",
                short_date(self.since)
            ));
        }
        lines.push(
            "An estimate from this machine's run times, at each profile's priced_as and each repository's \
             visibility as they are now; on GitHub's runners the jobs would run longer or shorter"
                .to_owned(),
        );
        lines
    }
}

/// A per-minute rate to the tenth of a cent: `$0.006`.
fn rate(rate: Usd) -> String {
    let thousandths = rate.0 / 1000;
    format!(
        "${}.{:03}",
        thousands(thousandths / 1000),
        thousandths % 1000
    )
}

/// `1 job`, `1,234 jobs`.
fn count(number: u64, one: &str, many: &str) -> String {
    format!(
        "{} {}",
        thousands(number),
        if number == 1 { one } else { many }
    )
}

/// A whole number with a comma every three digits: `1,234,567`.
fn thousands(number: u64) -> String {
    let digits = number.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

#[cfg(test)]
mod tests;
