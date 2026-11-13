//! Cancelling a job's run and re-running a job on GitHub (ADR 0134): the routes' query, the preview a request gets
//! before anything is sent, GitHub's answer and every sentence clients show of them. Plain data and wording;
//! `runnerd_core::actions` decides, issues the confirmations and sends. Nothing here holds GitHub's own text: a
//! refusal is its class and HTTP status, and every name a preview shows is cleaned of control characters and escapes
//! before it reaches a client.

use serde::{Deserialize, Serialize};

use super::{JobSummary, Tone, duration};

/// The doctor check that says whether the token can cancel and re-run jobs. Advisory: the attention list leaves it
/// out whatever its status, since only GitHub's answer to a real request says for sure.
pub const JOB_ACTIONS_CHECK: &str = "job actions";

/// The most other jobs of the run a confirmation names.
pub const SIBLINGS_MAX: usize = 50;

/// The longest job name a sentence quotes, in characters.
const NAME_MAX: usize = 80;

/// How long a preview's confirmation can be sent back, in seconds.
pub const CONFIRM_SECS: u64 = 300;

/// What an owner asks GitHub to do with a job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum JobAction {
    /// Cancel the job's workflow run: GitHub cancels runs, not single jobs.
    Cancel,
    /// Re-run the job, and with it the jobs that depend on it.
    Rerun,
    /// Re-run the failed jobs of the job's run, and the jobs that depend on them.
    RerunFailed,
    /// An action a newer daemon knows and this client does not.
    #[serde(other)]
    Unsupported,
}

impl JobAction {
    /// The actions a client offers on `job` as it stands: cancel while it is queued or runs, a re-run once it is
    /// final, and once it failed, a re-run of its run's failed jobs too. GitHub decides the rest; a client offers
    /// none while `Status::job_actions` is not on ([`JobActions::offered`]).
    pub fn for_job(job: &JobSummary) -> &'static [JobAction] {
        if !job.state.is_final() {
            &[JobAction::Cancel]
        } else if job.outcome().kind == super::OutcomeKind::Failed {
            &[JobAction::Rerun, JobAction::RerunFailed]
        } else {
            &[JobAction::Rerun]
        }
    }

    /// Its name in a log line.
    pub fn slug(self) -> &'static str {
        match self {
            JobAction::Cancel => "cancel",
            JobAction::Rerun => "rerun",
            JobAction::RerunFailed => "rerun-failed",
            JobAction::Unsupported => "unsupported",
        }
    }

    /// What a confirming button or prompt says: `Cancel run`, `Re-run job`, `Re-run failed jobs`.
    pub fn confirm_label(self) -> &'static str {
        match self {
            JobAction::Cancel => "Cancel run",
            JobAction::Rerun => "Re-run job",
            JobAction::RerunFailed => "Re-run failed jobs",
            JobAction::Unsupported => "Confirm",
        }
    }
}

/// Whether a daemon cancels and re-runs jobs, as `GET /v1/status` says.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum JobActions {
    /// It does.
    On,
    /// `[github] job_actions = false`: both routes answer 403.
    Off,
    /// A daemon without the routes, before 0.2, or a state this client does not know: never read as on.
    #[default]
    #[serde(other)]
    Unsupported,
}

impl JobActions {
    /// From `[github] job_actions`.
    pub fn configured(on: bool) -> JobActions {
        if on { JobActions::On } else { JobActions::Off }
    }

    /// Whether a client may offer cancel and re-run.
    pub fn offered(self) -> bool {
        self == JobActions::On
    }

    /// Why a client offers neither, `None` while it does.
    pub fn why_not(self) -> Option<&'static str> {
        match self {
            JobActions::On => None,
            JobActions::Off => Some(actions_off()),
            JobActions::Unsupported => {
                Some("this runnerd cannot cancel or re-run jobs: it is older than this client")
            }
        }
    }
}

/// What both routes answer, 403, while `[github] job_actions` is off.
pub fn actions_off() -> &'static str {
    "runnerd does not cancel or re-run jobs: [github] job_actions is false in its config"
}

/// The 403 for a job of a repository the config no longer has.
pub fn actions_not_configured(repository: &str) -> String {
    format!(
        "{repository} is not a configured repository; runnerd cancels and re-runs only the jobs of configured ones"
    )
}

/// The 409 while another action on the same run is being sent to GitHub, or one with the same confirmation.
pub fn actions_in_flight(other: JobAction, run: u64) -> String {
    let what = match other {
        JobAction::Cancel => "a cancel",
        JobAction::Rerun => "a re-run of a job",
        JobAction::RerunFailed => "a re-run of the failed jobs",
        JobAction::Unsupported => "another action",
    };
    format!("{what} of run {run} is being sent to GitHub; ask again once it answers")
}

/// The 422 for a job whose run the history did not record.
pub fn actions_no_run(job: u64) -> String {
    format!("the history has no workflow run for job {job}, so runnerd cannot ask GitHub about it")
}

/// `POST /v1/jobs/{id}/cancel` and `POST /v1/jobs/{id}/rerun` query parameters.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionQuery {
    /// Send it. Without it, or without a `confirm` this daemon issued, the answer is a 409 [`ActionPreview`] and
    /// nothing is sent to GitHub.
    #[serde(default)]
    pub force: bool,
    /// The preview's `confirm`: an opaque token this daemon issued with it, for [`CONFIRM_SECS`], that sends what it
    /// showed (the action, repository, run, attempt and job, under the config it was shown with) once.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confirm: Option<String>,
    /// Re-run the run's failed jobs rather than this job; only for `rerun`.
    #[serde(default)]
    pub failed: bool,
}

/// What an action acts on: the job as the history records it, and the run's attempt as GitHub said it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActionTarget {
    /// What is asked.
    pub action: JobAction,
    /// The job it was asked from.
    pub job: u64,
    /// The job's name in its workflow, as GitHub named it.
    pub name: String,
    /// `owner/name`.
    pub repository: String,
    /// The job's workflow run.
    pub run: u64,
    /// The run's current attempt, as GitHub said it; `None` when it could not be read.
    pub attempt: Option<u32>,
}

impl ActionTarget {
    /// `run 4182`, or `run 4182 (attempt 3)` once GitHub said which attempt is current.
    fn run_words(&self) -> String {
        match self.attempt {
            Some(attempt) => format!("run {} (attempt {attempt})", self.run),
            None => format!("run {}", self.run),
        }
    }

    /// The job's name for a sentence: without control characters or escapes, at most `NAME_MAX` characters, `job
    /// <id>` while unknown.
    fn name(&self) -> String {
        let plain = super::jobs::plain(&self.name);
        let plain = plain.trim();
        if plain.is_empty() {
            return format!("job {}", self.job);
        }
        if plain.chars().count() > NAME_MAX {
            let cut: String = plain.chars().take(NAME_MAX - 1).collect();
            return format!("{cut}…");
        }
        plain.to_owned()
    }

    /// What is asked, in the words of a confirmation's title.
    fn title(&self) -> String {
        let (run, repository) = (self.run_words(), &self.repository);
        match self.action {
            JobAction::Cancel => format!("Cancel {run} of {repository}?"),
            JobAction::Rerun => format!("Re-run {} of {repository}?", self.name()),
            JobAction::RerunFailed => {
                format!("Re-run the failed jobs of {run} of {repository}?")
            }
            JobAction::Unsupported => format!("Act on {run} of {repository}?"),
        }
    }

    /// What GitHub does with it, which is more than one job.
    fn scope(&self) -> String {
        let run = self.run;
        match self.action {
            JobAction::Cancel => "GitHub cancels the whole run, not one job: every job of it still queued or running \
                                  stops, here and on any other runner."
                .to_owned(),
            JobAction::Rerun => format!(
                "GitHub re-runs this job and the jobs that depend on it, in a new attempt of run {run}."
            ),
            JobAction::RerunFailed => format!(
                "GitHub re-runs the failed jobs of run {run} and the jobs that depend on them, in a new attempt."
            ),
            JobAction::Unsupported => String::new(),
        }
    }
}

/// The 409 a request without `force`, or with a `confirm` for something else, gets: what the action would do, said
/// before anything is sent to GitHub. Has the `message` of an [`ErrorResponse`](super::ErrorResponse), so a client
/// that reads only that still gets it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionPreview {
    /// The title and the scope in one line.
    pub message: String,
    /// What would be asked.
    pub action: JobAction,
    /// The job it is asked from.
    pub job: u64,
    /// `owner/name`.
    pub repository: String,
    /// The job's workflow run.
    pub run: u64,
    /// The run's current attempt as GitHub said it, which a confirmation acts on; `None` when it could not be read,
    /// and then there is no `confirm`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempt: Option<u32>,
    /// `Cancel run 4182 (attempt 3) of halyard/ledger?`
    pub title: String,
    /// What GitHub does, which is more than one job.
    pub scope: String,
    /// What else holds now: why an earlier confirmation was not taken, the run could not be read, it completed, the
    /// job is of an earlier attempt, the repository is paused. Often none.
    #[serde(default)]
    pub notes: Vec<String>,
    /// Other jobs of the run the history has, newest attempt first, at most [`SIBLINGS_MAX`], their names, workflows
    /// and branches cleaned of control characters and escapes. GitHub may have more: jobs on other runners, and jobs
    /// not queued yet.
    #[serde(default)]
    pub siblings: Vec<JobSummary>,
    /// What `siblings` covers, never "all of them".
    pub siblings_note: String,
    /// What the confirming button or prompt says.
    pub confirm_label: String,
    /// Send it back as `confirm` with `force=true` to act, within [`CONFIRM_SECS`] and once. `None` when nothing
    /// can be sent: the run could not be read, so what a confirmation would act on is unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confirm: Option<String>,
}

/// The run as GitHub said it when the preview was made.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunSeen {
    /// Read: its current attempt, and whether it completed.
    Now {
        /// The current attempt.
        attempt: u32,
        /// Whether it completed.
        completed: bool,
    },
    /// Not read: GitHub answered `status`, or did not answer.
    Unread {
        /// The HTTP status, when GitHub answered.
        status: Option<u16>,
    },
}

/// Why a request that came with a confirmation got a new preview instead, and nothing was sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Superseded {
    /// This runnerd holds no such confirmation: it ran out, runnerd restarted since, or another runnerd issued it.
    Unknown,
    /// It was sent already.
    Used,
    /// It was issued for another action or job.
    OtherRequest,
    /// The config's job actions or repositories changed since it was issued.
    ConfigChanged,
    /// The run moved to another attempt since it was issued.
    AttemptChanged,
    /// The run could not be read again right before the send.
    Unverified,
}

/// The facts of an [`ActionPreview`]: what the route found for the job.
#[derive(Debug, Clone)]
pub struct PreviewFacts {
    /// What is asked of which job; its `attempt` is `run`'s, when read.
    pub target: ActionTarget,
    /// The attempt of the run the job ran in, as the history says.
    pub job_attempt: u32,
    /// The run as GitHub said it.
    pub run: RunSeen,
    /// Why the request's confirmation was not taken, if it came with one.
    pub superseded: Option<Superseded>,
    /// The confirmation issued with this preview; `None` when the run was not read.
    pub confirm: Option<String>,
    /// Whether the repository is paused.
    pub paused: bool,
    /// The other jobs of the run the history has, newest attempt first, at most [`SIBLINGS_MAX`].
    pub siblings: Vec<JobSummary>,
    /// Whether the history has more of them than `siblings`.
    pub more: bool,
}

impl ActionPreview {
    /// The preview core words for `facts`.
    pub fn new(facts: PreviewFacts) -> ActionPreview {
        let PreviewFacts {
            mut target,
            job_attempt,
            run,
            superseded,
            confirm,
            paused,
            siblings,
            more,
        } = facts;
        let mut notes = Vec::new();
        if let Some(superseded) = superseded {
            notes.push(superseded_note(superseded));
        }
        let rerun = matches!(target.action, JobAction::Rerun | JobAction::RerunFailed);
        match run {
            RunSeen::Now { attempt, completed } => {
                target.attempt = Some(attempt);
                if completed && target.action == JobAction::Cancel {
                    notes.push(format!(
                        "Run {} has completed: GitHub has nothing left to cancel.",
                        target.run
                    ));
                }
                if !completed && rerun {
                    notes.push(format!(
                        "Run {} has not completed; GitHub refuses to re-run a run that has not.",
                        target.run
                    ));
                }
                if job_attempt < attempt {
                    notes.push(match target.action {
                        JobAction::Cancel => format!(
                            "This job ran in attempt {job_attempt}; the cancel stops attempt {attempt}, the run's \
                             current one."
                        ),
                        _ => format!("This job ran in attempt {job_attempt}; the run is at attempt {attempt} now."),
                    });
                }
            }
            RunSeen::Unread { status } => {
                target.attempt = None;
                let answered = status.map_or_else(
                    || "it did not answer".to_owned(),
                    |status| format!("it answered {status}"),
                );
                notes.push(format!(
                    "runnerd could not read run {} from GitHub ({answered}), so it cannot say which attempt this \
                     would act on, and offers no confirmation. Ask again later.",
                    target.run
                ));
            }
        }
        if paused && rerun {
            notes.push(format!(
                "{} is paused: runnerd starts none of the new attempt's jobs until it is resumed; other runners \
                 may take them.",
                target.repository
            ));
        }
        let siblings_note = match (siblings.len(), more) {
            (0, _) => {
                "runnerd knows no other job of this run; GitHub may have some, on other runners."
                    .to_owned()
            }
            (1, false) => {
                "runnerd knows 1 other job of this run; GitHub may have more, on other runners."
                    .to_owned()
            }
            (count, false) => format!(
                "runnerd knows {count} other jobs of this run; GitHub may have more, on other runners."
            ),
            (count, true) => format!(
                "runnerd knows more than {count} other jobs of this run, the first {count} shown; GitHub may have \
                 more still."
            ),
        };
        let title = target.title();
        let scope = target.scope();
        ActionPreview {
            message: format!("{title} {scope}"),
            action: target.action,
            job: target.job,
            repository: target.repository.clone(),
            run: target.run,
            attempt: target.attempt,
            title,
            scope,
            notes,
            siblings: siblings.into_iter().map(cleaned).collect(),
            siblings_note,
            confirm_label: target.action.confirm_label().to_owned(),
            confirm,
        }
    }
}

/// Why a confirmation was not taken, in words.
fn superseded_note(superseded: Superseded) -> String {
    let minutes = CONFIRM_SECS / 60;
    match superseded {
        Superseded::Unknown => format!(
            "That confirmation is not one this runnerd holds: it lasts {minutes} minutes, and a restart forgets it. \
             Nothing was sent; this is a new preview."
        ),
        Superseded::Used => {
            "That confirmation was sent already, and is sent once. Nothing more was sent; this is a new preview."
                .to_owned()
        }
        Superseded::OtherRequest => "That confirmation is for another action or job. Nothing was sent; this is a \
                                     new preview."
            .to_owned(),
        Superseded::ConfigChanged => "runnerd's config changed what it may do since that confirmation. Nothing was \
                                      sent; this is a new preview."
            .to_owned(),
        Superseded::AttemptChanged => "The run moved to another attempt since that confirmation. Nothing was sent; \
                                       this is a new preview of the attempt it is at now."
            .to_owned(),
        Superseded::Unverified => "runnerd could not read the run again right before sending, so it sent nothing; \
                                   this is a new preview."
            .to_owned(),
    }
}

/// A sibling as a preview shows it: every text GitHub named cleaned of control characters and escapes. The history
/// keeps the original.
fn cleaned(job: JobSummary) -> JobSummary {
    let clean = |text: &String| super::jobs::plain(text);
    JobSummary {
        name: clean(&job.name),
        repository: clean(&job.repository),
        workflow: job.workflow.as_ref().map(clean),
        branch: job.branch.as_ref().map(clean),
        conclusion: job.conclusion.as_ref().map(clean),
        url: job.url.as_ref().map(clean),
        labels: job.labels.iter().map(clean).collect(),
        runner: job.runner.as_ref().map(|runner| super::JobRunner {
            name: clean(&runner.name),
            ..runner.clone()
        }),
        ..job
    }
}

/// How GitHub took an action.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ActionOutcome {
    /// GitHub accepted it. It answers with an acknowledgement: a cancel takes effect as the runners report it, a
    /// re-run's new jobs show up as they queue.
    Accepted,
    /// The run had finished already: GitHub answered 409 and a read of the run said it completed.
    AlreadyFinished,
    /// GitHub, or runnerd before sending, refused it: nothing was done.
    Refused,
    /// runnerd cannot tell whether GitHub did it: the request may have reached GitHub and its answer not come back.
    /// Also how this client reads an outcome only a newer daemon knows.
    #[serde(other)]
    Unknown,
}

/// What class of refusal an [`ActionOutcome::Refused`] was. Never GitHub's own words.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ActionRefusal {
    /// runnerd could read no GitHub token: nothing was sent.
    NoToken,
    /// runnerd could not reach GitHub: nothing was sent.
    Unreachable,
    /// GitHub refused the token: 401.
    Unauthorized,
    /// 403: the token lacks the permission, or GitHub does not allow the action for the run now.
    Forbidden,
    /// 404 or 410: GitHub does not show the run or the job to this token, or it is gone.
    NotFound,
    /// 409: the run's state does not allow it, for a cancel while the run had not completed.
    Conflict,
    /// 422: GitHub cannot do it for this run.
    Unprocessable,
    /// GitHub's rate limit is used up: nothing was sent, or GitHub refused for it.
    RateLimited,
    /// Another error status below 500, such as a 400.
    Failed,
    /// runnerd's config changed between the confirmation and the request (job actions turned off, the repositories
    /// changed): runnerd sent nothing.
    Withdrawn,
    /// A class only a newer daemon knows.
    #[serde(other)]
    Other,
}

/// Why runnerd cannot tell what GitHub did. Not on the wire: the result's `message` words it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnknownCause {
    /// No answer within the request's deadline.
    TimedOut,
    /// The connection broke after the request may have been sent.
    Broken,
    /// GitHub, or a gateway in front of it, answered with a server error (5xx): it may have done it.
    ServerError,
    /// runnerd began to stop while GitHub was asked.
    Stopping,
}

/// What GitHub answered an action, as runnerd classifies it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionAnswer {
    /// A 2xx.
    Accepted {
        /// Its HTTP status.
        status: u16,
    },
    /// A cancel's 409, with a read of the run that said it completed.
    AlreadyFinished {
        /// Its HTTP status.
        status: u16,
    },
    /// Refused, with nothing done.
    Refused {
        /// Its class.
        refusal: ActionRefusal,
        /// The HTTP status, when GitHub answered.
        status: Option<u16>,
        /// For a rate limit, when it resets, in seconds since the Unix epoch.
        until: Option<u64>,
    },
    /// Whether GitHub did it is unknown.
    Unknown {
        /// Why.
        cause: UnknownCause,
        /// The HTTP status, when one came.
        status: Option<u16>,
    },
}

/// `POST /v1/jobs/{id}/cancel` and `POST /v1/jobs/{id}/rerun` once GitHub was asked: how it took it. The job's record
/// is not changed: GitHub's listings bring the run's new state and a re-run's new jobs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionResult {
    /// What was asked.
    pub action: JobAction,
    /// The job it was asked from.
    pub job: u64,
    /// `owner/name`.
    pub repository: String,
    /// The job's workflow run.
    pub run: u64,
    /// How GitHub took it.
    pub outcome: ActionOutcome,
    /// GitHub's HTTP status, when it answered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    /// For a refusal, its class.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refusal: Option<ActionRefusal>,
    /// For a rate limit, when it resets, in seconds since the Unix epoch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub until: Option<u64>,
    /// What happened, in words.
    pub message: String,
}

impl ActionResult {
    /// The result core words for `answer` to `target`, at `now`.
    pub fn new(target: &ActionTarget, answer: ActionAnswer, now: u64) -> ActionResult {
        let (outcome, status, refusal, until) = match answer {
            ActionAnswer::Accepted { status } => {
                (ActionOutcome::Accepted, Some(status), None, None)
            }
            ActionAnswer::AlreadyFinished { status } => {
                (ActionOutcome::AlreadyFinished, Some(status), None, None)
            }
            ActionAnswer::Refused {
                refusal,
                status,
                until,
            } => (ActionOutcome::Refused, status, Some(refusal), until),
            ActionAnswer::Unknown { status, .. } => (ActionOutcome::Unknown, status, None, None),
        };
        let message = match answer {
            ActionAnswer::Accepted { .. } => accepted(target),
            ActionAnswer::AlreadyFinished { .. } => format!(
                "Run {} of {} had completed already: nothing was cancelled.",
                target.run, target.repository
            ),
            ActionAnswer::Refused {
                refusal,
                status,
                until,
            } => refused(target, refusal, status, until, now),
            ActionAnswer::Unknown { cause, status } => unknown(target, cause, status),
        };
        ActionResult {
            action: target.action,
            job: target.job,
            repository: target.repository.clone(),
            run: target.run,
            outcome,
            status,
            refusal,
            until,
            message,
        }
    }

    /// What it means for the reader.
    pub fn tone(&self) -> Tone {
        match self.outcome {
            ActionOutcome::Accepted => Tone::Good,
            ActionOutcome::AlreadyFinished => Tone::Neutral,
            ActionOutcome::Refused => Tone::Bad,
            ActionOutcome::Unknown => Tone::Warn,
        }
    }

    /// Whether GitHub took it, or there was nothing to do: what a command's exit status says.
    pub fn succeeded(&self) -> bool {
        matches!(
            self.outcome,
            ActionOutcome::Accepted | ActionOutcome::AlreadyFinished
        )
    }
}

fn accepted(target: &ActionTarget) -> String {
    let (run, repository) = (target.run_words(), &target.repository);
    match target.action {
        JobAction::Cancel => format!(
            "GitHub is cancelling {run} of {repository}; its jobs end as their runners report it."
        ),
        JobAction::Rerun => format!(
            "GitHub re-runs {} and the jobs that depend on it in a new attempt of {run}; they show here as \
             they queue.",
            target.name()
        ),
        JobAction::RerunFailed => format!(
            "GitHub re-runs the failed jobs of {run} and the jobs that depend on them in a new attempt; they \
             show here as they queue."
        ),
        JobAction::Unsupported => format!("GitHub accepted the action on {run}."),
    }
}

/// What was refused, before why.
fn asked(target: &ActionTarget) -> String {
    match target.action {
        JobAction::Cancel => format!("cancel {}", target.run_words()),
        JobAction::Rerun => format!("re-run {}", target.name()),
        JobAction::RerunFailed => format!("re-run the failed jobs of {}", target.run_words()),
        JobAction::Unsupported => format!("act on {}", target.run_words()),
    }
}

fn refused(
    target: &ActionTarget,
    refusal: ActionRefusal,
    status: Option<u16>,
    until: Option<u64>,
    now: u64,
) -> String {
    let asked = asked(target);
    let repository = &target.repository;
    let rerun = matches!(target.action, JobAction::Rerun | JobAction::RerunFailed);
    let why = match refusal {
        ActionRefusal::NoToken => {
            return format!("runnerd could read no GitHub token, so it did not {asked}.");
        }
        ActionRefusal::Unreachable => {
            return format!("runnerd could not reach GitHub, so it did not {asked}.");
        }
        ActionRefusal::RateLimited => {
            let wait = until.map_or_else(String::new, |until| {
                format!(
                    "; try again in {}",
                    duration(until.saturating_sub(now).max(1))
                )
            });
            return match status {
                Some(status) => format!(
                    "GitHub refused to {asked} ({status}): its rate limit is used up{wait}. Nothing was done."
                ),
                None => {
                    format!("GitHub's rate limit is used up, so runnerd did not {asked}{wait}.")
                }
            };
        }
        ActionRefusal::Withdrawn => {
            return format!(
                "runnerd did not {asked}: its config changed before the request went out (job actions turned off, \
                 or the repositories changed). Nothing was sent."
            );
        }
        ActionRefusal::Unauthorized => "it refused the token".to_owned(),
        ActionRefusal::Forbidden if rerun => format!(
            "the token may lack Actions: write (or the repo scope) on {repository}, or the run has not finished"
        ),
        ActionRefusal::Forbidden => {
            format!("the token may lack Actions: write (or the repo scope) on {repository}")
        }
        ActionRefusal::NotFound => {
            "it does not show the run to this token, or the run is gone".to_owned()
        }
        ActionRefusal::Conflict => "the run's state does not allow it now".to_owned(),
        ActionRefusal::Unprocessable => "it cannot do that for this run".to_owned(),
        ActionRefusal::Failed | ActionRefusal::Other => "it failed".to_owned(),
    };
    let status = status.map_or_else(String::new, |status| format!(" ({status})"));
    format!("GitHub refused to {asked}{status}: {why}. Nothing was done.")
}

fn unknown(target: &ActionTarget, cause: UnknownCause, status: Option<u16>) -> String {
    let why = match cause {
        UnknownCause::TimedOut => "GitHub did not answer in time".to_owned(),
        UnknownCause::Broken => "the connection broke after the request went out".to_owned(),
        UnknownCause::ServerError => match status {
            Some(status) => format!(
                "GitHub answered {status}, a server error, which does not say whether it acted"
            ),
            None => "GitHub answered with a server error, which does not say whether it acted"
                .to_owned(),
        },
        UnknownCause::Stopping => "runnerd began to stop while GitHub was asked".to_owned(),
    };
    let asked = asked(target);
    format!(
        "runnerd cannot tell whether GitHub did {asked}: {why}. Check the run on GitHub before asking again."
    )
}

#[cfg(all(test, any(feature = "host", feature = "client")))]
mod tests;
