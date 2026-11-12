//! What needs the owner's attention now, as one list every client draws (ADR 0132): queued jobs runnerd cannot start
//! with its config, a job held long for the host's memory, the doctor's failures and warnings, warning insights,
//! GitHub refusing or failing the listings, and the rate limit. [`attention_items`] decides and words every item from
//! what the daemon already serves; the daemon's attention worker calls it and owns the [`Attention`] snapshot. Plain
//! data and pure functions: no I/O, and nothing inferred from history or occupancy where no evidence says it holds now.

use serde::{Deserialize, Deserializer, Serialize};

use super::{
    CheckStatus, Doctor, DoctorState, Insight, InsightKind, Insights, QueuedJob, Severity, Status,
    Tone, View, Waiting,
};

/// The most items an [`Attention`] lists; the rest are counted in `omitted`.
pub const ATTENTION_MAX: usize = 50;

/// `GET /v1/attention`: what needs attention now, most urgent first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attention {
    /// The daemon process that keeps the snapshot: new at each start, never the same as an earlier one's. Revisions
    /// count within one instance; a client compares them only when the instances are the same.
    pub instance: String,
    /// From 1 at the daemon's start, one up whenever the content changes, also when one item replaces another.
    /// `attention.changed` on `/v1/events` carries the new one. Two snapshots of one `instance` and `revision` say the
    /// same.
    pub revision: u64,
    /// By tone, then known evidence times before unknown, newest first, then `key`; at most [`ATTENTION_MAX`].
    pub items: Vec<AttentionItem>,
    /// Items beyond [`ATTENTION_MAX`], left out.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub omitted: u64,
}

#[expect(
    clippy::trivially_copy_pass_by_ref,
    reason = "serde's skip_serializing_if passes a reference"
)]
fn is_zero(count: &u64) -> bool {
    *count == 0
}

/// One thing that needs attention.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttentionItem {
    /// The same while the condition holds, such as `cannot-start:paused:halyard/ledger`: what a client keys it by.
    pub key: String,
    /// What source it comes from.
    pub kind: AttentionKind,
    /// How urgent; a tone this client does not know reads as [`Tone::Neutral`].
    #[serde(deserialize_with = "tone_or_neutral")]
    pub tone: Tone,
    /// What holds, in one line.
    pub title: String,
    /// What it is about.
    pub subject: Subject,
    /// When the evidence for it began or was taken, in seconds since the Unix epoch; absent when unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since: Option<u64>,
    /// What clears it; absent when there is nothing to name, or the fix is one this client cannot read.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "lenient"
    )]
    pub fix: Option<Fix>,
}

/// Where an attention item comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AttentionKind {
    /// Queued jobs runnerd cannot start with the config it runs with: their repository is paused, or no pool serves
    /// their labels ([`Waiting::cannot_start`]).
    CannotStart,
    /// A job has waited 30 minutes or more for the host's memory, and one waits for it now.
    HostMemory,
    /// A doctor check failed or warns, in a run for the config the daemon runs with.
    Doctor,
    /// The doctor's checks are not current: never run yet, failed, or stale.
    DoctorUnavailable,
    /// A warning insight.
    Insight,
    /// A repository's queued jobs cannot be listed.
    GithubListing,
    /// GitHub refuses runnerd's token, or there is none: no repository is listed.
    GithubAuth,
    /// GitHub's rate limit is used up: no runner starts until it resets.
    RateLimited,
    /// A kind from a newer daemon than this client knows: draw it by its title.
    #[serde(other)]
    Unknown,
}

/// What an attention item is about, for a client to link.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "kind")]
pub enum Subject {
    /// A job.
    Job {
        /// GitHub's job ID, the history's too.
        id: u64,
        /// `owner/name`.
        repository: String,
        /// The job's name.
        name: String,
    },
    /// A repository.
    Repository {
        /// `owner/name`.
        name: String,
    },
    /// One of the doctor's checks.
    Check {
        /// The check's name, such as `slice`.
        name: String,
    },
    /// An insight.
    Insight {
        /// Its key in `GET /v1/insights`.
        key: String,
    },
    /// The host as a whole.
    Host,
    /// GitHub, for every repository.
    Github,
    /// A subject from a newer daemon than this client knows.
    #[serde(other)]
    Unknown,
}

/// What clears an attention item. A client reads a fix of a kind or view it does not know as none
/// ([`AttentionItem::fix`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "kind")]
pub enum Fix {
    /// A command to run, such as `runnerctl resume halyard/ledger`.
    Command {
        /// The command line.
        command: String,
    },
    /// A key of the config to change, such as `repositories.labels`.
    Config {
        /// The key, dotted.
        key: String,
    },
    /// A view where the subject is shown with what to do.
    Open {
        /// The view.
        view: View,
    },
    /// What to do, in words, such as a doctor check's fix.
    Text {
        /// What to do.
        text: String,
    },
}

impl Fix {
    /// What to do, as every client says it: the command, `Change repositories.labels in the config`, `Open Settings`,
    /// or the text.
    pub fn line(&self) -> String {
        match self {
            Fix::Command { command } => command.clone(),
            Fix::Config { key } => format!("Change {key} in the config"),
            Fix::Open { view } => format!("Open {}", view.title()),
            Fix::Text { text } => text.clone(),
        }
    }
}

impl Attention {
    /// The snapshot of `items`, as [`attention_items`] orders them, at `revision` of `instance`: the first
    /// [`ATTENTION_MAX`], and how many more.
    pub fn new(instance: String, revision: u64, mut items: Vec<AttentionItem>) -> Attention {
        let omitted = items.len().saturating_sub(ATTENTION_MAX);
        items.truncate(ATTENTION_MAX);
        Attention {
            instance,
            revision,
            items,
            omitted: omitted as u64,
        }
    }

    /// Whether `other` says the same, whatever its instance and revision.
    pub fn same_content(&self, other: &Attention) -> bool {
        self.items == other.items && self.omitted == other.omitted
    }

    /// What a client that shows the first `shown` items says of the rest: `and 3 more`; `None` when it shows all.
    pub fn more(&self, shown: usize) -> Option<String> {
        let rest = self.items.len().saturating_sub(shown) as u64 + self.omitted;
        (rest > 0).then(|| format!("and {rest} more"))
    }
}

/// Everything that needs attention now, most urgent first: from `status`, the doctor's last run as `doctor` says it,
/// and `insights` when they could be read, at `now` (seconds since the Unix epoch). Each item stands on evidence that
/// holds now; a source without it is left out, and doctor checks that are not current are one item saying so.
pub fn attention_items(
    status: &Status,
    doctor: &Doctor,
    insights: Option<&Insights>,
    now: u64,
) -> Vec<AttentionItem> {
    let mut items = cannot_start(status);
    items.extend(host_memory(status, insights));
    items.extend(doctor_items(doctor, now));
    items.extend(
        insights
            .into_iter()
            .flat_map(|insights| &insights.insights)
            .filter_map(insight_item),
    );
    items.extend(github(status, now));
    items.sort_by(|a, b| {
        rank(a.tone)
            .cmp(&rank(b.tone))
            .then_with(|| b.since.is_some().cmp(&a.since.is_some()))
            .then_with(|| b.since.cmp(&a.since))
            .then_with(|| a.key.cmp(&b.key))
    });
    items
}

/// The most urgent tone first.
fn rank(tone: Tone) -> u8 {
    match tone {
        Tone::Bad => 0,
        Tone::Warn => 1,
        Tone::Neutral => 2,
        Tone::Quiet => 3,
        Tone::Good => 4,
    }
}

/// Queued jobs runnerd cannot start with its config, one item per repository and reason.
fn cannot_start(status: &Status) -> Vec<AttentionItem> {
    let mut groups: Vec<(String, Vec<&QueuedJob>)> = Vec::new();
    for job in &status.queued {
        let Some(waiting) = job
            .waiting
            .as_ref()
            .filter(|waiting| waiting.cannot_start())
        else {
            continue;
        };
        let key = match waiting {
            Waiting::NoPool { labels } => format!(
                "cannot-start:no-pool:{}:{}",
                job.repository,
                labels.join(",")
            ),
            _ => format!("cannot-start:paused:{}", job.repository),
        };
        match groups.iter_mut().find(|(group, _)| *group == key) {
            Some((_, jobs)) => jobs.push(job),
            None => groups.push((key, vec![job])),
        }
    }
    groups
        .into_iter()
        .filter_map(|(key, jobs)| {
            let first = *jobs.first()?;
            let repository = &first.repository;
            let (what, subject) = match jobs.as_slice() {
                [job] => (
                    format!("{} of {repository}", job.name),
                    Subject::Job {
                        id: job.id,
                        repository: repository.clone(),
                        name: job.name.clone(),
                    },
                ),
                _ => (
                    format!("{} queued jobs of {repository}", jobs.len()),
                    Subject::Repository {
                        name: repository.clone(),
                    },
                ),
            };
            let (why, fix) = match first.waiting.as_ref()? {
                Waiting::NoPool { labels } if labels.is_empty() => (
                    "it asks for no labels, so no profile serves it".to_owned(),
                    Fix::Config {
                        key: "repositories.labels".to_owned(),
                    },
                ),
                Waiting::NoPool { labels } => (
                    format!(
                        "no profile of {repository} serves the {} {}",
                        if labels.len() == 1 { "label" } else { "labels" },
                        labels.join(", ")
                    ),
                    Fix::Config {
                        key: "repositories.labels".to_owned(),
                    },
                ),
                _ => (
                    format!("{repository} is paused"),
                    Fix::Command {
                        command: format!("runnerctl resume {repository}"),
                    },
                ),
            };
            Some(AttentionItem {
                key,
                kind: AttentionKind::CannotStart,
                tone: Tone::Warn,
                title: format!("runnerd cannot start {what} with the current configuration: {why}"),
                subject,
                since: jobs
                    .iter()
                    .map(|job| job.queued_at)
                    .filter(|at| *at > 0)
                    .min(),
                fix: Some(fix),
            })
        })
        .collect()
}

/// The host-held insight, while a queued job waits for the host's memory in `status` too: one item, which the
/// insights leave out. It is about the host, which programs outside CI hold, not about one job: the insight names
/// none, and the jobs waiting may have changed since it was read. Its evidence time is unknown for the same reason;
/// the daemon reads the insights again when the jobs waiting for the host change.
fn host_memory(status: &Status, insights: Option<&Insights>) -> Option<AttentionItem> {
    let held = insights?
        .insights
        .iter()
        .find(|insight| matches!(insight.kind, InsightKind::HostHeld { .. }))?;
    status
        .queued
        .iter()
        .any(|job| matches!(job.waiting, Some(Waiting::HostMemory { .. })))
        .then(|| AttentionItem {
            key: "host-memory".to_owned(),
            kind: AttentionKind::HostMemory,
            tone: Tone::Warn,
            title: held.sentence.clone(),
            subject: Subject::Host,
            since: None,
            fix: Some(Fix::Open {
                view: View::Overview,
            }),
        })
}

/// The doctor's failures and warnings from a current run, or one item saying its checks are not current.
fn doctor_items(doctor: &Doctor, now: u64) -> Vec<AttentionItem> {
    let at = match &doctor.state {
        // `runnerd doctor`'s own run, or one a daemon ran for the request: current.
        None => None,
        Some(DoctorState::Ready { at }) => Some(*at),
        Some(
            state @ (DoctorState::NotRun
            | DoctorState::Stale { .. }
            | DoctorState::Failed { .. }
            | DoctorState::Unknown),
        ) => {
            let (tone, since) = match state {
                DoctorState::Failed { at, .. } => (Tone::Warn, Some(*at)),
                DoctorState::Stale { at } => (Tone::Quiet, Some(*at)),
                _ => (Tone::Quiet, None),
            };
            return state
                .line(now)
                .map(|title| AttentionItem {
                    key: "doctor-unavailable".to_owned(),
                    kind: AttentionKind::DoctorUnavailable,
                    tone,
                    title,
                    subject: Subject::Host,
                    since,
                    fix: Some(Fix::Open {
                        view: View::Settings,
                    }),
                })
                .into_iter()
                .collect();
        }
    };
    let shown: Vec<_> = doctor
        .checks
        .iter()
        .filter(|check| matches!(check.status, CheckStatus::Fail | CheckStatus::Warn))
        // Advisory whatever it says: only GitHub's answer to a real action is evidence (ADR 0134).
        .filter(|check| check.name != super::JOB_ACTIONS_CHECK)
        .collect();
    let mut keys: Vec<String> = Vec::new();
    shown
        .iter()
        .map(|check| {
            // Checks of one name, such as each repository's visibility, are told apart by what they found, so a key
            // stays with its check when another of the name comes or goes.
            let repeated = shown
                .iter()
                .filter(|other| other.name == check.name)
                .count()
                > 1;
            let mut key = if repeated {
                format!("doctor:{}:{}", check.name, check.detail)
            } else {
                format!("doctor:{}", check.name)
            };
            let twins = keys.iter().filter(|seen| **seen == key).count();
            keys.push(key.clone());
            if twins > 0 {
                key = format!("{key}#{}", twins + 1);
            }
            AttentionItem {
                key,
                kind: AttentionKind::Doctor,
                tone: check.status.tone(),
                title: format!("{}: {}", check.name, check.detail),
                subject: Subject::Check {
                    name: check.name.clone(),
                },
                since: at,
                fix: Some(match &check.fix {
                    Some(text) => Fix::Text { text: text.clone() },
                    None => Fix::Open {
                        view: View::Settings,
                    },
                }),
            }
        })
        .collect()
}

/// A warning insight that says what holds now: not the host-held one, which [`host_memory`] stands for, nor a past
/// day's low memory.
fn insight_item(insight: &Insight) -> Option<AttentionItem> {
    let current = !matches!(
        insight.kind,
        InsightKind::HostHeld { .. } | InsightKind::HostLow { .. } | InsightKind::Unknown
    );
    (insight.severity == Severity::Warning && current).then(|| AttentionItem {
        key: format!("insight:{}", insight.key),
        kind: AttentionKind::Insight,
        tone: Tone::Warn,
        title: insight.sentence.clone(),
        subject: Subject::Insight {
            key: insight.key.clone(),
        },
        since: insight.runs.first().map(|run| run.at),
        fix: Some(Fix::Open {
            view: View::Insights,
        }),
    })
}

/// The listings GitHub fails or refuses, and its rate limit.
fn github(status: &Status, now: u64) -> Vec<AttentionItem> {
    let failing: Vec<_> = status
        .repositories
        .iter()
        .filter_map(|repository| Some((repository.name.as_str(), repository.listing.as_ref()?)))
        .collect();
    let token = failing
        .iter()
        .filter(|(_, failure)| failure.problem.is_token())
        .min_by_key(|(_, failure)| failure.since);
    let mut items: Vec<AttentionItem> = token
        .map(|(_, failure)| AttentionItem {
            key: "github-auth".to_owned(),
            kind: AttentionKind::GithubAuth,
            tone: Tone::Bad,
            title: match failure.problem {
                super::ListingProblem::NoToken => {
                    "runnerd has no GitHub token, so it lists no repository's queued jobs"
                        .to_owned()
                }
                _ => {
                    "GitHub refuses runnerd's token (401), so it lists no repository's queued jobs"
                        .to_owned()
                }
            },
            subject: Subject::Github,
            since: Some(failure.since),
            fix: Some(Fix::Open {
                view: View::Settings,
            }),
        })
        .into_iter()
        .collect();
    items.extend(
        failing
            .iter()
            .filter(|(_, failure)| !failure.problem.is_token())
            .map(|(name, failure)| AttentionItem {
                key: format!("github-listing:{name}"),
                kind: AttentionKind::GithubListing,
                tone: Tone::Warn,
                title: failure.line(name),
                subject: Subject::Repository {
                    name: (*name).to_owned(),
                },
                since: Some(failure.since),
                fix: matches!(failure.problem, super::ListingProblem::Refused { .. }).then_some(
                    Fix::Open {
                        view: View::Settings,
                    },
                ),
            }),
    );
    // Gone once its reset time passed, whether or not the engine published since.
    let limited = status.github_limited_until.is_some_and(|until| until > now);
    items.extend(
        status
            .github_line()
            .filter(|_| limited)
            .map(|line| AttentionItem {
                key: "rate-limited".to_owned(),
                kind: AttentionKind::RateLimited,
                tone: Tone::Warn,
                title: line.to_owned(),
                subject: Subject::Github,
                since: None,
                fix: None,
            }),
    );
    items
}

/// A value of a type this client may not know all of, such as a [`Fix`] naming a view it lacks: `None` then, so the
/// rest of the document still reads.
fn lenient<'de, D: Deserializer<'de>, T: Deserialize<'de>>(
    deserializer: D,
) -> Result<Option<T>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Read<T> {
        Known(T),
        Other(serde::de::IgnoredAny),
    }
    Ok(match Option::<Read<T>>::deserialize(deserializer)? {
        Some(Read::Known(value)) => Some(value),
        Some(Read::Other(_)) | None => None,
    })
}

fn tone_or_neutral<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Tone, D::Error> {
    Ok(lenient(deserializer)?.unwrap_or(Tone::Neutral))
}

// The wire format is JSON, which only the host and client features read.
#[cfg(all(test, any(feature = "host", feature = "client")))]
mod tests;
