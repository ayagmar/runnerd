//! The automatic re-run of a run its runner failed (ADR 0153), against the fake GitHub and a real history: one re-run
//! per attempt however many jobs faulted, none after a restart or a person's re-run, none where a job failed in its
//! code or was cancelled, a refusal held without a storm, and GitHub never asked twice for one attempt over generated
//! sequences of faults, answers, restarts and reloads.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use tokio::time::Instant;

use super::{Fake, Reply, actions_of, cancelled, completed, running};
use crate::api::{self, ActionRefusal};
use crate::github::{ActionFailure, AttemptJob, JobActionRequest, JobDetails, JobStep, QueuedJob};
use crate::scheduler::engine::Control;
use crate::store::{LogPosition, RunnerRecord, Store};
use crate::testdir::TestDir;

/// How a job of the history ended.
#[derive(Clone, Copy)]
enum Ended {
    /// Its runner failed it: Sysbox's lost listener in its failed step.
    RunnerFault,
    /// Its own code failed it.
    Code,
    Success,
}

/// Job `id` of `repository`'s run `run`, attempt `attempt`, ran here and ended as `ended`; its record is final.
async fn ran(store: &Store, repository: &str, id: u64, run: u64, attempt: u32, ended: Ended) {
    let runner = format!("r{id}");
    store.queued(
        repository,
        &QueuedJob {
            id,
            run_id: run,
            name: format!("job {id}"),
            labels: vec!["self-hosted".to_owned()],
            // Now, so a restart's look back over the last hours finds it.
            created_at: crate::time::now_ms(),
            run_attempt: attempt,
            ..QueuedJob::default()
        },
        crate::time::now_ms(),
    );
    store.runner_started(RunnerRecord {
        purpose: None,
        admission: None,
        name: runner.clone(),
        repository: repository.to_owned(),
        profile: "app".to_owned(),
        slot: 1,
        container: format!("{runner}-container"),
        started_at: 1_000_100,
        memory_cap: None,
        cpu_cap: None,
    });
    store.claimed(id, run, &runner, 1_000_200);
    let printed = match ended {
        Ended::RunnerFault => "2026-11-09T00:16:02.8Z mount /proc: function not implemented",
        Ended::Code | Ended::Success => "2026-11-09T00:16:02.8Z expected 2, got 3",
    };
    let log = [
        "2026-11-09T00:16:00.1Z ##[group]Run ./gradlew check",
        "2026-11-09T00:16:00.1Z ##[endgroup]",
        printed,
        "2026-11-09T00:16:10.8Z ##[error]Process completed with exit code 1.",
    ];
    store
        .log(
            id,
            log.iter().map(|line| (*line).to_owned()).collect(),
            LogPosition { page: 1, offset: 0 },
            u64::MAX,
        )
        .await;
    let conclusion = match ended {
        Ended::RunnerFault | Ended::Code => "failure",
        Ended::Success => "success",
    };
    store.details(JobDetails {
        id,
        run_id: run,
        run_attempt: attempt,
        name: format!("job {id}"),
        workflow_name: None,
        head_branch: None,
        html_url: None,
        labels: Vec::new(),
        created_at: None,
        status: "completed".to_owned(),
        conclusion: Some(conclusion.to_owned()),
        started_at: None,
        completed_at: None,
        runner_name: Some(runner.clone()),
        steps: [
            ("Set up job", "success"),
            ("Run ./gradlew check", conclusion),
        ]
        .iter()
        .zip(1..)
        .map(|((name, conclusion), number)| JobStep {
            number,
            name: (*name).to_owned(),
            status: "completed".to_owned(),
            conclusion: Some((*conclusion).to_owned()),
            started_at: None,
            completed_at: None,
        })
        .collect(),
    });
    store.runner_removed(&runner, 1_000_300);
}

fn job(id: u64, conclusion: &str) -> AttemptJob {
    AttemptJob {
        id,
        status: "completed".to_owned(),
        conclusion: Some(conclusion.to_owned()),
    }
}

struct Rig {
    github: Arc<Fake>,
    control: Arc<Control>,
    store: Store,
    _dir: TestDir,
}

/// A history in a temp directory, `o/app` configured with its runner faults re-run (its config opts in), and a GitHub
/// that accepts.
fn rig() -> Rig {
    let dir = TestDir::new("rerun");
    let store = Store::open(&dir.0.join("history.db")).unwrap();
    let (control, _commands) = Control::new(["o/app".to_owned()], [], store.clone());
    control.set_action_policy(api_allowed(&["o/app"], &["o/app"]));
    Rig {
        github: Fake::new(Reply::Status(201)),
        control: Arc::new(control),
        store,
        _dir: dir,
    }
}

impl Rig {
    /// The job actions of a runnerd on this history, as a start or a restart has them.
    fn actions(&self) -> super::JobActions {
        actions_of(
            &self.github,
            &self.store,
            &self.control,
            Duration::from_secs(60),
        )
    }

    fn reruns_sent(&self) -> usize {
        self.github
            .sent()
            .iter()
            .filter(|(_, request)| matches!(request, JobActionRequest::RerunFailed { .. }))
            .count()
    }

    fn reads(&self) -> usize {
        self.github.reads.load(Ordering::SeqCst)
    }
}

/// A started runnerd's first pass over the faults its history has.
async fn start(rig: &Rig) -> super::JobActions {
    let actions = rig.actions();
    actions.read_faults(Instant::now()).await;
    actions.pass(Instant::now()).await;
    actions
}

#[tokio::test]
async fn a_run_whose_runner_failed_two_jobs_is_rerun_once_and_not_again_after_a_restart() {
    let rig = rig();
    rig.github.set_run(Ok(completed(1)));
    *rig.github.jobs.lock().unwrap() = Ok(vec![
        job(1, "failure"),
        job(2, "failure"),
        job(3, "success"),
    ]);
    let actions = rig.actions();
    let following = tokio::spawn(actions.clone().rerun_faults());
    // Two faults of one attempt, and a job that passed, each final while the daemon follows the history.
    ran(&rig.store, "o/app", 1, 500, 1, Ended::RunnerFault).await;
    ran(&rig.store, "o/app", 2, 500, 1, Ended::RunnerFault).await;
    ran(&rig.store, "o/app", 3, 500, 1, Ended::Success).await;
    rig.store.flush().await;
    tokio::time::timeout(Duration::from_secs(5), async {
        while rig.reruns_sent() == 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the run is re-run");
    // GitHub moved the run to its next attempt, as an accepted re-run does.
    rig.github.set_run(Ok(running(2)));
    actions
        .pass(Instant::now() + Duration::from_secs(600))
        .await;
    following.abort();
    assert_eq!(
        rig.github.sent(),
        vec![(
            "o/app".to_owned(),
            JobActionRequest::RerunFailed { run: 500 }
        )],
        "one re-run for the attempt's two faults"
    );
    rig.store.flush().await;
    let summary = |id| {
        let store = rig.store.clone();
        async move { store.job(id).await.unwrap().unwrap().summary }
    };
    assert_eq!(
        summary(1).await.rerun_line().as_deref(),
        Some("runnerd re-ran the run's failed jobs as attempt 2, since the runner failed this one")
    );
    assert!(
        summary(2).await.rerun_at.is_some(),
        "job 2 is noted re-run too"
    );
    assert_eq!(summary(3).await.rerun_at, None, "job 3 passed");

    // A restart reads nothing back to re-run.
    start(&rig).await;
    assert_eq!(rig.reruns_sent(), 1, "a restart re-runs nothing again");
}

#[tokio::test]
async fn a_run_a_person_reran_or_with_a_job_failed_otherwise_is_left_alone() {
    // A person re-ran it before runnerd could, or runnerd's re-run landed and the daemon stopped before noting it:
    // GitHub's attempt says so.
    let rig = rig();
    ran(&rig.store, "o/app", 1, 500, 1, Ended::RunnerFault).await;
    rig.store.flush().await;
    rig.github.set_run(Ok(completed(2)));
    start(&rig).await;
    assert_eq!(rig.reruns_sent(), 0, "a later attempt exists");

    // A job of the attempt failed in its code: a re-run would run it again for nothing.
    let rig = self::rig();
    ran(&rig.store, "o/app", 1, 500, 1, Ended::RunnerFault).await;
    ran(&rig.store, "o/app", 4, 500, 1, Ended::Code).await;
    rig.store.flush().await;
    rig.github.set_run(Ok(completed(1)));
    *rig.github.jobs.lock().unwrap() = Ok(vec![job(1, "failure"), job(4, "failure")]);
    start(&rig).await;
    assert_eq!(rig.reruns_sent(), 0, "job 4 failed in its code");

    // A job cancelled, or failed on a runner of another machine, which the history does not have.
    *rig.github.jobs.lock().unwrap() = Ok(vec![job(1, "failure"), job(99, "cancelled")]);
    start(&rig).await;
    assert_eq!(rig.reruns_sent(), 0, "job 99 was cancelled");

    // The repository's re-runs are off: GitHub is not even read.
    let rig = self::rig();
    ran(&rig.store, "o/app", 1, 500, 1, Ended::RunnerFault).await;
    rig.store.flush().await;
    rig.github.set_run(Ok(completed(1)));
    *rig.github.jobs.lock().unwrap() = Ok(vec![job(1, "failure")]);
    rig.control.set_action_policy(api_allowed(&["o/app"], &[]));
    start(&rig).await;
    assert_eq!(
        (rig.reruns_sent(), rig.reads()),
        (0, 0),
        "rerun_runner_faults is off"
    );
}

#[tokio::test]
async fn a_run_still_running_is_read_again_a_minute_on_and_rerun_once_it_completed() {
    let rig = rig();
    ran(&rig.store, "o/app", 1, 500, 1, Ended::RunnerFault).await;
    rig.store.flush().await;
    rig.github.set_run(Ok(running(1)));
    *rig.github.jobs.lock().unwrap() = Ok(vec![job(1, "failure"), job(2, "success")]);
    let actions = start(&rig).await;
    assert_eq!(
        (rig.reruns_sent(), rig.reads()),
        (0, 1),
        "GitHub refuses a re-run of a run in progress"
    );
    let now = Instant::now();
    actions.pass(now + Duration::from_secs(30)).await;
    assert_eq!(rig.reads(), 1, "not read again within the minute");
    rig.github.set_run(Ok(completed(1)));
    actions.pass(now + Duration::from_secs(61)).await;
    assert_eq!(rig.reruns_sent(), 1, "re-run once it completed");
}

#[tokio::test]
async fn a_token_without_actions_write_holds_reruns_an_hour_without_a_storm() {
    let rig = rig();
    *rig.github.reply.lock().unwrap() = Reply::Status(403);
    ran(&rig.store, "o/app", 1, 500, 1, Ended::RunnerFault).await;
    ran(&rig.store, "o/app", 2, 600, 1, Ended::RunnerFault).await;
    rig.store.flush().await;
    rig.github.set_run(Ok(completed(1)));
    *rig.github.jobs.lock().unwrap() = Ok(vec![job(1, "failure"), job(2, "failure")]);
    let actions = start(&rig).await;
    assert_eq!(rig.reruns_sent(), 1, "one refused request, not one per run");
    let refused = actions
        .rerun_refused("o/app")
        .expect("the refusal is shown");
    assert_eq!(
        (refused.refusal, refused.status),
        (ActionRefusal::Forbidden, Some(403))
    );
    // Within the hour nothing is sent or read; once the token has the permission, the hour's end re-runs both.
    let reads = rig.reads();
    actions.pass(Instant::now() + Duration::from_mins(30)).await;
    assert_eq!(
        (rig.reruns_sent(), rig.reads()),
        (1, reads),
        "held for the hour"
    );
    *rig.github.reply.lock().unwrap() = Reply::Status(201);
    actions.pass(Instant::now() + Duration::from_mins(61)).await;
    assert_eq!(rig.reruns_sent(), 3, "both runs once the hold ended");
    assert!(actions.rerun_refused("o/app").is_none(), "the hold is over");
}

/// `setting` on, with the repositories `configured`, those of `reruns` re-running their runner faults.
fn api_allowed(configured: &[&str], reruns: &[&str]) -> super::super::policy::Allowed {
    let set = |names: &[&str]| {
        names
            .iter()
            .map(|name| (*name).to_owned())
            .collect::<BTreeSet<_>>()
    };
    super::super::policy::Allowed {
        setting: api::JobActions::On,
        repositories: set(configured),
        reruns: set(reruns),
    }
}

/// A small deterministic generator.
struct Rng(u64);

impl Rng {
    fn below(&mut self, n: u64) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 % n
    }
}

/// Over generated sequences of faults, GitHub's answers, restarts, reloads and time, GitHub is never asked to re-run
/// one attempt of a run twice, never asked while the run is past the fault's attempt, never asked while the
/// repository's re-runs are off or one of the attempt's jobs failed in its code, and never asked from the third
/// attempt on. A request GitHub acted on but answered with a server error or not at all counts as asked: what
/// follows must read the attempt and not ask again.
#[tokio::test]
#[allow(
    clippy::too_many_lines,
    reason = "one arm per kind of step, which split up would hide the model"
)]
async fn github_is_never_asked_twice_for_one_attempt_over_generated_sequences() {
    // How many re-runs GitHub acted on, and of those how many it answered without saying so: the sequences must reach
    // both, or they prove nothing.
    let (mut acted_on, mut unsaid) = (0, 0);
    for seed in 1..=100_u64 {
        let rig = rig();
        let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
        let mut actions = rig.actions();
        let mut attempt = 1_u32;
        let mut completed_run = false;
        let mut code_failed = false;
        let mut on = true;
        // The attempts GitHub acted on a re-run of, and those a runner failed a job in.
        let mut acted: BTreeSet<u32> = BTreeSet::new();
        let mut faulted: BTreeSet<u32> = BTreeSet::new();
        let mut next_job = 1_u64;
        let mut at = Instant::now();
        for _ in 0..40 {
            match rng.below(12) {
                // The runner fails a job of the current attempt.
                0 | 1 => {
                    ran(
                        &rig.store,
                        "o/app",
                        next_job,
                        500,
                        attempt,
                        Ended::RunnerFault,
                    )
                    .await;
                    rig.store.flush().await;
                    actions.read_faults(at).await;
                    faulted.insert(attempt);
                    next_job += 1;
                }
                // Now and then, a job of the current attempt fails in its code.
                2 if rng.below(3) == 0 => {
                    ran(&rig.store, "o/app", next_job, 500, attempt, Ended::Code).await;
                    rig.store.flush().await;
                    code_failed = true;
                    next_job += 1;
                }
                // The run completes, or a person re-runs it.
                3 | 4 => {
                    if rng.below(4) == 0 {
                        attempt += 1;
                        completed_run = false;
                        code_failed = false;
                    } else {
                        completed_run = true;
                    }
                }
                // GitHub's next answer to a send.
                5 => {
                    *rig.github.reply.lock().unwrap() = match rng.below(5) {
                        0 => Reply::Status(403),
                        1 => Reply::Status(502),
                        2 => Reply::Fail(ActionFailure::TimedOut),
                        3 => Reply::Status(409),
                        _ => Reply::Status(201),
                    };
                }
                // A restart: what was in memory is gone.
                6 => {
                    actions = rig.actions();
                    actions.read_faults(at).await;
                }
                // A reload turns the repository's re-runs off or on.
                7 => {
                    on = !on;
                    let reruns: &[&str] = if on { &["o/app"] } else { &[] };
                    rig.control
                        .set_action_policy(api_allowed(&["o/app"], reruns));
                }
                // Time passes and due checks run.
                _ => {
                    at += Duration::from_secs(rng.below(2400));
                    let jobs = rig.store.run_jobs("o/app", 500, 0, 500).await.unwrap().jobs;
                    *rig.github.by_attempt.lock().unwrap() = jobs
                        .iter()
                        .map(|summary| {
                            let conclusion = summary.conclusion.as_deref().unwrap_or("failure");
                            (summary.run_attempt, job(summary.id, conclusion))
                        })
                        .collect();
                    rig.github.set_run(Ok(if completed_run {
                        completed(attempt)
                    } else {
                        running(attempt)
                    }));
                    let before = rig.github.sent().len();
                    actions.pass(at).await;
                    let sent = rig.github.sent().len() - before;
                    assert!(sent <= 1, "seed {seed}: one send per pass for one run");
                    if sent == 1 {
                        assert!(on, "seed {seed}: sent with re-runs off");
                        assert!(
                            completed_run,
                            "seed {seed}: sent while the run was in progress"
                        );
                        assert!(
                            !code_failed,
                            "seed {seed}: sent with a job failed in its code"
                        );
                        assert!(attempt <= 2, "seed {seed}: sent from attempt {attempt}");
                        assert!(
                            faulted.contains(&attempt),
                            "seed {seed}: attempt {attempt} had no fault"
                        );
                        // GitHub acts on every request that reached it, whatever it answered, but a refusal.
                        let reply = *rig.github.reply.lock().unwrap();
                        if !matches!(reply, Reply::Status(403 | 409)) {
                            assert!(
                                acted.insert(attempt),
                                "seed {seed}: attempt {attempt} re-run twice"
                            );
                            acted_on += 1;
                            if !matches!(reply, Reply::Status(201)) {
                                unsaid += 1;
                            }
                            attempt += 1;
                            completed_run = false;
                            code_failed = false;
                        }
                    }
                }
            }
        }
    }
    assert!(
        acted_on >= 10 && unsaid >= 2,
        "acted on {acted_on}, {unsaid} unsaid"
    );
}

/// Faults in runs 500, 600 and 700 of `o/app`, attempt 1, jobs 1 to 3.
async fn three_runs(rig: &Rig) {
    for (job, run) in [(1, 500), (2, 600), (3, 700)] {
        ran(&rig.store, "o/app", job, run, 1, Ended::RunnerFault).await;
    }
    rig.store.flush().await;
}

#[tokio::test]
async fn a_send_github_answers_without_saying_is_never_sent_again_nor_credited() {
    // GitHub did not act: the run stays at its attempt, and nothing tells runnerd the request will not land later.
    let rig = rig();
    ran(&rig.store, "o/app", 1, 500, 1, Ended::RunnerFault).await;
    rig.store.flush().await;
    rig.github.set_run(Ok(completed(1)));
    *rig.github.jobs.lock().unwrap() = Ok(vec![job(1, "failure")]);
    *rig.github.reply.lock().unwrap() = Reply::Status(502);
    let actions = start(&rig).await;
    for hours in [1, 3, 5] {
        actions
            .pass(Instant::now() + Duration::from_hours(hours))
            .await;
    }
    assert_eq!(
        rig.reruns_sent(),
        1,
        "an unstated outcome is not sent again"
    );

    // GitHub acted and answered with a timeout: the run moved on, but a person may have moved it.
    let rig = self::rig();
    ran(&rig.store, "o/app", 1, 500, 1, Ended::RunnerFault).await;
    rig.store.flush().await;
    rig.github.set_run(Ok(completed(1)));
    *rig.github.jobs.lock().unwrap() = Ok(vec![job(1, "failure")]);
    *rig.github.reply.lock().unwrap() = Reply::Fail(ActionFailure::TimedOut);
    rig.github.acts.store(true, Ordering::SeqCst);
    start(&rig).await;
    rig.store.flush().await;
    let summary = rig.store.job(1).await.unwrap().unwrap().summary;
    assert_eq!(
        summary.rerun_at, None,
        "only a re-run GitHub accepted is noted as runnerd's"
    );
}

#[tokio::test]
async fn a_restart_while_a_send_is_unanswered_never_sends_it_again() {
    // The request reached GitHub, which has not answered, nor acted yet, when the daemon stops.
    let rig = rig();
    ran(&rig.store, "o/app", 1, 500, 1, Ended::RunnerFault).await;
    rig.store.flush().await;
    rig.github.set_run(Ok(completed(1)));
    *rig.github.jobs.lock().unwrap() = Ok(vec![job(1, "failure")]);
    *rig.github.reply.lock().unwrap() = Reply::Hang;
    let crashed = rig.actions();
    let pass = tokio::spawn(async move {
        crashed.read_faults(Instant::now()).await;
        crashed.pass(Instant::now()).await;
    });
    while rig.reruns_sent() == 0 {
        tokio::task::yield_now().await;
    }
    pass.abort();
    let _ = pass.await;
    rig.store.flush().await;

    // A restart, and a check after a later fault of the same attempt.
    *rig.github.reply.lock().unwrap() = Reply::Status(201);
    let actions = start(&rig).await;
    ran(&rig.store, "o/app", 2, 500, 1, Ended::RunnerFault).await;
    rig.store.flush().await;
    actions.read_faults(Instant::now()).await;
    actions.pass(Instant::now() + Duration::from_hours(1)).await;
    assert_eq!(
        rig.reruns_sent(),
        1,
        "the claimed attempt is never sent again"
    );

    // One that GitHub answered with a 502 is claimed as well.
    let rig = self::rig();
    ran(&rig.store, "o/app", 1, 500, 1, Ended::RunnerFault).await;
    rig.store.flush().await;
    rig.github.set_run(Ok(completed(1)));
    *rig.github.jobs.lock().unwrap() = Ok(vec![job(1, "failure")]);
    *rig.github.reply.lock().unwrap() = Reply::Status(502);
    start(&rig).await;
    rig.store.flush().await;
    *rig.github.reply.lock().unwrap() = Reply::Status(201);
    start(&rig).await;
    assert_eq!(rig.reruns_sent(), 1, "a restart after a 502 sends nothing");
}

#[tokio::test]
async fn a_refused_send_releases_its_claim_for_a_later_check() {
    let rig = rig();
    ran(&rig.store, "o/app", 1, 500, 1, Ended::RunnerFault).await;
    rig.store.flush().await;
    rig.github.set_run(Ok(completed(1)));
    *rig.github.jobs.lock().unwrap() = Ok(vec![job(1, "failure")]);
    *rig.github.reply.lock().unwrap() = Reply::Status(409);
    start(&rig).await;
    rig.store.flush().await;
    *rig.github.reply.lock().unwrap() = Reply::Status(201);
    start(&rig).await;
    assert_eq!(
        rig.reruns_sent(),
        2,
        "GitHub refused the first, so a restart may send it"
    );
}

#[tokio::test]
async fn a_read_after_an_unanswered_send_is_rate_limited_and_refused_as_any_other() {
    let rig = rig();
    three_runs(&rig).await;
    rig.github.set_run(Ok(completed(1)));
    *rig.github.jobs.lock().unwrap() = Ok(vec![job(1, "failure")]);
    *rig.github.reply.lock().unwrap() = Reply::Status(502);
    *rig.github.run_after_send.lock().unwrap() = Some(Err(ActionFailure::Status { status: 429 }));
    start(&rig).await;
    // The run's read, its jobs, the permit's read, and the read after the send.
    assert_eq!(rig.reads(), 4, "the other runs wait out the 429");
    assert_eq!(rig.reruns_sent(), 1);

    let rig = self::rig();
    ran(&rig.store, "o/app", 1, 500, 1, Ended::RunnerFault).await;
    rig.store.flush().await;
    rig.github.set_run(Ok(completed(1)));
    *rig.github.jobs.lock().unwrap() = Ok(vec![job(1, "failure")]);
    *rig.github.reply.lock().unwrap() = Reply::Status(502);
    *rig.github.run_after_send.lock().unwrap() = Some(Err(ActionFailure::Status { status: 403 }));
    let actions = start(&rig).await;
    assert!(
        actions.rerun_refused("o/app").is_some(),
        "a refused read after the send holds the repository"
    );
}

#[tokio::test]
async fn a_run_cancelled_as_a_whole_is_not_rerun_whatever_its_jobs_say() {
    let rig = rig();
    ran(&rig.store, "o/app", 1, 500, 1, Ended::RunnerFault).await;
    rig.store.flush().await;
    rig.github.set_run(Ok(cancelled(1)));
    *rig.github.jobs.lock().unwrap() = Ok(vec![job(1, "failure"), job(2, "success")]);
    start(&rig).await;
    assert_eq!(rig.reruns_sent(), 0, "the run was cancelled");
}

#[tokio::test]
async fn a_refused_read_holds_the_repository_like_a_refused_send() {
    let rig = rig();
    three_runs(&rig).await;
    rig.github
        .set_run(Err(ActionFailure::Status { status: 403 }));
    let actions = start(&rig).await;
    assert_eq!(rig.reads(), 1, "one refused read, not one per run");
    let refused = actions
        .rerun_refused("o/app")
        .expect("the refusal is shown");
    assert_eq!(refused.status, Some(403));
    actions.pass(Instant::now() + Duration::from_mins(30)).await;
    assert_eq!(rig.reads(), 1, "held for the hour");
}

#[tokio::test]
async fn a_rate_limit_without_a_reset_stops_every_runs_reads_for_a_minute() {
    let rig = rig();
    three_runs(&rig).await;
    rig.github
        .set_run(Err(ActionFailure::Status { status: 429 }));
    let actions = start(&rig).await;
    assert_eq!(rig.reads(), 1, "the other runs wait out the 429");
    actions.pass(Instant::now() + Duration::from_secs(30)).await;
    assert_eq!(rig.reads(), 1, "within the minute");
}

#[tokio::test]
async fn a_slow_pass_schedules_each_runs_next_read_from_when_it_was_read() {
    let rig = rig();
    three_runs(&rig).await;
    rig.github.set_run(Ok(running(1)));
    *rig.github.delay.lock().unwrap() = Duration::from_millis(50);
    let began = Instant::now();
    let actions = rig.actions();
    actions.read_faults(began).await;
    actions.pass(began).await;
    assert_eq!(rig.reads(), 3);
    actions.pass(began + Duration::from_millis(60_020)).await;
    assert_eq!(
        rig.reads(),
        3,
        "each was read 50 ms or more after the pass began"
    );
}

#[tokio::test]
async fn another_fault_does_not_cut_a_failed_checks_backoff_short() {
    let rig = rig();
    ran(&rig.store, "o/app", 1, 500, 1, Ended::RunnerFault).await;
    rig.store.flush().await;
    rig.github
        .set_run(Err(ActionFailure::Status { status: 502 }));
    let actions = start(&rig).await;
    assert_eq!(rig.reads(), 1);
    // A second fault of the attempt, or a replay of the first after missed events.
    ran(&rig.store, "o/app", 2, 500, 1, Ended::RunnerFault).await;
    rig.store.flush().await;
    let now = Instant::now();
    actions.read_faults(now).await;
    actions.pass(now + Duration::from_secs(1)).await;
    assert_eq!(rig.reads(), 1, "the backoff holds");
}

#[tokio::test]
async fn an_owners_rerun_in_flight_is_not_noted_as_automatic() {
    let rig = rig();
    ran(&rig.store, "o/app", 1, 500, 1, Ended::RunnerFault).await;
    rig.store.flush().await;
    rig.github.set_run(Ok(completed(1)));
    *rig.github.jobs.lock().unwrap() = Ok(vec![job(1, "failure")]);
    let actions = rig.actions();
    let super::Asked::Preview(preview) = actions
        .ask(1, api::JobAction::RerunFailed, None)
        .await
        .unwrap()
    else {
        panic!("a preview first");
    };
    let confirm = preview.confirm.clone().unwrap();
    rig.github.hold();
    let owner = actions.clone();
    let asked = tokio::spawn(async move {
        owner
            .ask(1, api::JobAction::RerunFailed, Some(&confirm))
            .await
    });
    rig.github.entered(1).await;
    let automatic = actions.clone();
    let pass = tokio::spawn(async move {
        automatic.read_faults(Instant::now()).await;
        automatic.pass(Instant::now()).await;
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    rig.github.release();
    asked.await.unwrap().unwrap();
    pass.await.unwrap();
    rig.store.flush().await;
    let summary = rig.store.job(1).await.unwrap().unwrap().summary;
    assert_eq!(summary.rerun_at, None, "the owner re-ran it, not runnerd");
}

#[tokio::test]
async fn a_reload_before_the_permit_withdraws_an_automatic_send() {
    let rig = rig();
    ran(&rig.store, "o/app", 1, 500, 1, Ended::RunnerFault).await;
    rig.store.flush().await;
    rig.github.set_run(Ok(completed(1)));
    *rig.github.jobs.lock().unwrap() = Ok(vec![job(1, "failure")]);
    rig.github.hold();
    let actions = rig.actions();
    let automatic = actions.clone();
    let pass = tokio::spawn(async move {
        automatic.read_faults(Instant::now()).await;
        automatic.pass(Instant::now()).await;
    });
    rig.github.entered(1).await;
    rig.control.set_action_policy(api_allowed(&["o/app"], &[]));
    tokio::time::sleep(Duration::from_millis(20)).await;
    rig.github.release();
    pass.await.unwrap();
    assert_eq!(rig.reruns_sent(), 0, "withdrawn by the reload");
}

#[tokio::test]
async fn a_reload_after_github_took_the_send_leaves_it_unanswered_and_never_sent_again() {
    let rig = rig();
    ran(&rig.store, "o/app", 1, 500, 1, Ended::RunnerFault).await;
    rig.store.flush().await;
    rig.github.set_run(Ok(completed(1)));
    *rig.github.jobs.lock().unwrap() = Ok(vec![job(1, "failure")]);
    *rig.github.reply.lock().unwrap() = Reply::Hang;
    let actions = rig.actions();
    let automatic = actions.clone();
    let pass = tokio::spawn(async move {
        automatic.read_faults(Instant::now()).await;
        automatic.pass(Instant::now()).await;
    });
    while rig.reruns_sent() == 0 {
        tokio::task::yield_now().await;
    }
    // A reload that changes something else: `o/app`'s faults are still re-run.
    rig.control
        .set_action_policy(api_allowed(&["o/app", "o/web"], &["o/app"]));
    tokio::time::timeout(Duration::from_secs(5), pass)
        .await
        .expect("the reload ends the wait")
        .unwrap();
    *rig.github.reply.lock().unwrap() = Reply::Status(201);
    actions.pass(Instant::now() + Duration::from_hours(1)).await;
    assert_eq!(
        rig.reruns_sent(),
        1,
        "a send GitHub may act on is never sent again"
    );
}
