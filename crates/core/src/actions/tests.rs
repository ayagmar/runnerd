//! The job actions against a fake GitHub and a real history in a temp directory: nothing is sent without a
//! confirmation this runnerd issued, and that once; the run's attempt and the config of the moment are checked once
//! the token is in hand; each of GitHub's refusals is worded with its class and status and changes nothing here; an
//! ambiguous request is never sent again; concurrent requests for one run send once.

#![allow(clippy::unwrap_used, reason = "tests")]

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use futures_util::future::BoxFuture;
use tokio::sync::Semaphore;

use super::{ActionError, ActionsGithub, Asked, JobActions, Limits};
use crate::api::{self, ActionOutcome, ActionPreview, ActionRefusal, JobAction};
use crate::github::{ActionFailure, JobActionRequest, Permit, QueuedJob, RunNow};
use crate::scheduler::engine::{Control, Shutdown};
use crate::store::Store;
use crate::testdir::TestDir;

/// How the fake answers a job action.
#[derive(Clone, Copy)]
enum Reply {
    Status(u16),
    Fail(ActionFailure),
    /// Never answers.
    Hang,
}

/// A GitHub whose runs are at attempt 2 and running until a test says otherwise.
struct Fake {
    reply: Mutex<Reply>,
    /// What a read of a run says.
    run: Mutex<Result<RunNow, ActionFailure>>,
    sent: Mutex<Vec<(String, JobActionRequest)>>,
    /// The sends that came to the gate.
    entered: AtomicUsize,
    /// Held, a send waits before its permit, as for its token.
    gate: Semaphore,
    /// Runs during the next read of the run, as something that lands while GitHub answers it.
    during_read: Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

impl Fake {
    fn new(reply: Reply) -> Arc<Fake> {
        Arc::new(Fake {
            reply: Mutex::new(reply),
            run: Mutex::new(Ok(running(2))),
            sent: Mutex::new(Vec::new()),
            entered: AtomicUsize::new(0),
            gate: Semaphore::new(Semaphore::MAX_PERMITS),
            during_read: Mutex::new(None),
        })
    }

    fn sent(&self) -> Vec<(String, JobActionRequest)> {
        self.sent.lock().unwrap().clone()
    }

    fn set_run(&self, run: Result<RunNow, ActionFailure>) {
        *self.run.lock().unwrap() = run;
    }

    /// Holds every send at its token until `release`.
    fn hold(&self) {
        self.gate.forget_permits(Semaphore::MAX_PERMITS);
    }

    fn release(&self) {
        self.gate.add_permits(Semaphore::MAX_PERMITS);
    }

    /// Waits until `count` sends came to the gate.
    async fn entered(&self, count: usize) {
        while self.entered.load(Ordering::SeqCst) < count {
            tokio::task::yield_now().await;
        }
    }
}

impl ActionsGithub for Fake {
    fn run<'call>(
        &'call self,
        _repository: &'call str,
        _run: u64,
    ) -> BoxFuture<'call, Result<RunNow, ActionFailure>> {
        let during = self.during_read.lock().unwrap().take();
        if let Some(during) = during {
            during();
        }
        let run = *self.run.lock().unwrap();
        Box::pin(async move { run })
    }

    fn send<'call>(
        &'call self,
        repository: &'call str,
        request: JobActionRequest,
        permit: &'call Permit<'call>,
    ) -> BoxFuture<'call, Result<u16, ActionFailure>> {
        Box::pin(async move {
            self.entered.fetch_add(1, Ordering::SeqCst);
            drop(self.gate.acquire().await.unwrap());
            if !permit().await {
                return Err(ActionFailure::Withdrawn);
            }
            self.sent
                .lock()
                .unwrap()
                .push((repository.to_owned(), request));
            let reply = *self.reply.lock().unwrap();
            match reply {
                Reply::Status(status) if (200..300).contains(&status) => Ok(status),
                Reply::Status(status) => Err(ActionFailure::Status { status }),
                Reply::Fail(failure) => Err(failure),
                Reply::Hang => std::future::pending().await,
            }
        })
    }
}

struct Setup {
    actions: JobActions,
    github: Arc<Fake>,
    control: Arc<Control>,
    store: Store,
    _dir: TestDir,
}

/// Job 11 of run 500, attempt 2 of `o/app`, queued; jobs 12 (attempt 1) and 13 of the same run, and job 20 of another,
/// are in the history too. Job 30 of `o/old`, a repository the config does not have, and job 40 with no run.
async fn setup(reply: Reply) -> Setup {
    setup_with(reply, Duration::from_secs(60)).await
}

/// As [`setup`], with confirmations that last `lasts`.
async fn setup_with(reply: Reply, lasts: Duration) -> Setup {
    let dir = TestDir::new("actions");
    let store = Store::open(&dir.0.join("history.db")).unwrap();
    let queue = |repository: &str, id: u64, run_id: u64, attempt: u32| {
        store.queued(
            repository,
            &QueuedJob {
                id,
                run_id,
                name: format!("build {id}"),
                labels: vec!["self-hosted".to_owned()],
                created_at: 1_000_000,
                run_attempt: attempt,
                workflow: Some("CI".to_owned()),
                ..QueuedJob::default()
            },
            1_000_000,
        );
    };
    queue("o/app", 11, 500, 2);
    queue("o/app", 12, 500, 1);
    queue("o/app", 13, 500, 2);
    queue("o/app", 20, 600, 1);
    queue("o/old", 30, 700, 1);
    queue("o/app", 40, 0, 1);
    store.flush().await;
    let (control, _commands) = Control::new(["o/app".to_owned()], [], store.clone());
    let control = Arc::new(control);
    let github = Fake::new(reply);
    let actions = actions_of(&github, &store, &control, lasts);
    Setup {
        actions,
        github,
        control,
        store,
        _dir: dir,
    }
}

/// A runnerd's job actions on `store`, as one more daemon, or the same one after a restart, has them.
fn actions_of(
    github: &Arc<Fake>,
    store: &Store,
    control: &Arc<Control>,
    lasts: Duration,
) -> JobActions {
    JobActions::new(
        github.clone(),
        store.clone(),
        control.clone(),
        Limits {
            deadline: Duration::from_millis(300),
            confirmation: lasts,
        },
    )
}

fn preview(asked: Asked) -> Box<ActionPreview> {
    match asked {
        Asked::Preview(preview) => preview,
        Asked::Done(result) => panic!("sent: {result:?}"),
    }
}

fn done(asked: Asked) -> api::ActionResult {
    match asked {
        Asked::Done(result) => result,
        Asked::Preview(preview) => panic!("previewed: {preview:?}"),
    }
}

/// The confirmation a preview of `action` of job 11 issues.
async fn confirm(setup: &Setup, action: JobAction) -> String {
    preview(setup.actions.ask(11, action, None).await.unwrap())
        .confirm
        .unwrap()
}

async fn send(setup: &Setup, action: JobAction) -> api::ActionResult {
    let token = confirm(setup, action).await;
    done(setup.actions.ask(11, action, Some(&token)).await.unwrap())
}

/// Job 11 as the history has it.
async fn job_11(store: &Store) -> api::JobSummary {
    store.job(11).await.unwrap().unwrap().summary
}

fn running(attempt: u32) -> RunNow {
    RunNow {
        attempt,
        completed: false,
    }
}

fn completed(attempt: u32) -> RunNow {
    RunNow {
        attempt,
        completed: true,
    }
}

#[tokio::test]
async fn a_request_without_a_confirmation_this_runnerd_issued_is_previewed_and_sends_nothing() {
    let setup = setup(Reply::Status(202)).await;
    let first = preview(
        setup
            .actions
            .ask(11, JobAction::Cancel, None)
            .await
            .unwrap(),
    );
    assert_eq!(first.title, "Cancel run 500 (attempt 2) of o/app?");
    assert_eq!(first.attempt, Some(2));
    assert!(
        first.scope.contains("cancels the whole run, not one job"),
        "the preview names the run's scope: {}",
        first.scope
    );
    let siblings: Vec<u64> = first.siblings.iter().map(|job| job.id).collect();
    assert_eq!(
        siblings,
        [13, 12],
        "the run's other jobs, newest attempt first"
    );
    assert_eq!(
        first.siblings_note,
        "runnerd knows 2 other jobs of this run; GitHub may have more, on other runners."
    );
    assert_eq!(first.confirm_label, "Cancel run");
    let token = first.confirm.unwrap();
    assert_eq!(token.len(), 32, "128 random bits: {token}");
    assert!(
        !token.contains("o/app") && !token.contains("500"),
        "opaque: {token}"
    );
    let second = confirm(&setup, JobAction::Cancel).await;
    assert_ne!(token, second, "each preview issues its own");
    // Made up, of the form the run's details would give, or another action's: a new preview, nothing sent.
    let rerun = confirm(&setup, JobAction::Rerun).await;
    for (forged, why) in [
        ("cancel:o/app:500:11", "not one this runnerd holds"),
        (
            "0123456789abcdef0123456789abcdef",
            "not one this runnerd holds",
        ),
        ("", "not one this runnerd holds"),
        (rerun.as_str(), "for another action or job"),
    ] {
        let again = preview(
            setup
                .actions
                .ask(11, JobAction::Cancel, Some(forged))
                .await
                .unwrap(),
        );
        assert!(
            again.notes.first().is_some_and(|note| note.contains(why)),
            "`{forged}`: {:?}",
            again.notes
        );
    }
    assert_eq!(setup.github.sent(), Vec::new(), "nothing went to GitHub");
}

#[tokio::test]
async fn a_confirmation_is_sent_once_and_a_replay_after_it_landed_gets_a_new_preview() {
    let setup = setup(Reply::Status(202)).await;
    let token = confirm(&setup, JobAction::Cancel).await;
    let result = done(
        setup
            .actions
            .ask(11, JobAction::Cancel, Some(&token))
            .await
            .unwrap(),
    );
    assert_eq!(result.outcome, ActionOutcome::Accepted);
    let replay = preview(
        setup
            .actions
            .ask(11, JobAction::Cancel, Some(&token))
            .await
            .unwrap(),
    );
    assert!(
        replay.notes[0].starts_with("That confirmation was sent already"),
        "{:?}",
        replay.notes
    );
    assert_eq!(setup.github.sent().len(), 1, "a replay sends nothing");
    // A new preview's confirmation sends anew: no exactly-once promise past its own.
    let again = done(
        setup
            .actions
            .ask(11, JobAction::Cancel, replay.confirm.as_deref())
            .await
            .unwrap(),
    );
    assert_eq!(again.outcome, ActionOutcome::Accepted);
    assert_eq!(setup.github.sent().len(), 2);
}

#[tokio::test]
async fn a_confirmation_from_another_runnerd_from_before_a_restart_or_run_out_is_unknown() {
    let setup = setup_with(Reply::Status(202), Duration::from_millis(200)).await;
    let token = confirm(&setup, JobAction::Cancel).await;
    // Another daemon on the same history, or this one after a restart: it holds none of the first one's.
    let other = actions_of(
        &setup.github,
        &setup.store,
        &setup.control,
        Duration::from_secs(60),
    );
    let elsewhere = preview(
        other
            .ask(11, JobAction::Cancel, Some(&token))
            .await
            .unwrap(),
    );
    assert!(
        elsewhere.notes[0].starts_with("That confirmation is not one this runnerd holds"),
        "{:?}",
        elsewhere.notes
    );
    tokio::time::sleep(Duration::from_millis(250)).await;
    let late = preview(
        setup
            .actions
            .ask(11, JobAction::Cancel, Some(&token))
            .await
            .unwrap(),
    );
    assert!(
        late.notes[0].starts_with("That confirmation is not one this runnerd holds"),
        "run out: {:?}",
        late.notes
    );
    assert_eq!(setup.github.sent(), Vec::new(), "nothing went to GitHub");
}

#[tokio::test]
async fn a_repository_removed_and_added_back_voids_its_confirmations() {
    let setup = setup(Reply::Status(202)).await;
    let token = confirm(&setup, JobAction::Cancel).await;
    let on = api::JobActions::On;
    setup.control.set_action_policy(on, BTreeSet::new());
    assert_eq!(
        setup
            .actions
            .ask(11, JobAction::Cancel, Some(&token))
            .await
            .unwrap_err(),
        ActionError::Forbidden(api::actions_not_configured("o/app"))
    );
    setup
        .control
        .set_action_policy(on, BTreeSet::from(["o/app".to_owned()]));
    let again = preview(
        setup
            .actions
            .ask(11, JobAction::Cancel, Some(&token))
            .await
            .unwrap(),
    );
    assert!(
        again.notes[0].starts_with("runnerd's config changed"),
        "{:?}",
        again.notes
    );
    // A reload that changes neither keeps the confirmation.
    let kept = again.confirm.unwrap();
    setup
        .control
        .set_action_policy(on, BTreeSet::from(["o/app".to_owned()]));
    let result = done(
        setup
            .actions
            .ask(11, JobAction::Cancel, Some(&kept))
            .await
            .unwrap(),
    );
    assert_eq!(result.outcome, ActionOutcome::Accepted);
    assert_eq!(setup.github.sent().len(), 1);
}

#[tokio::test]
async fn job_actions_turned_off_refuse_a_confirmation_before_github() {
    let setup = setup(Reply::Status(202)).await;
    let token = confirm(&setup, JobAction::Cancel).await;
    setup
        .control
        .set_action_policy(api::JobActions::Off, BTreeSet::from(["o/app".to_owned()]));
    assert_eq!(
        setup
            .actions
            .ask(11, JobAction::Cancel, Some(&token))
            .await
            .unwrap_err(),
        ActionError::Forbidden(api::actions_off().to_owned())
    );
    assert_eq!(setup.github.sent(), Vec::new());
}

#[tokio::test]
async fn a_reload_while_the_send_waits_for_its_token_withdraws_it() {
    let setup = setup(Reply::Status(202)).await;
    let token = confirm(&setup, JobAction::Cancel).await;
    setup.github.hold();
    let asked = {
        let actions = setup.actions.clone();
        tokio::spawn(async move { actions.ask(11, JobAction::Cancel, Some(&token)).await })
    };
    setup.github.entered(1).await;
    // A reload that turns job actions off lands while the token is read.
    setup
        .control
        .set_action_policy(api::JobActions::Off, BTreeSet::from(["o/app".to_owned()]));
    setup.github.release();
    let result = done(asked.await.unwrap().unwrap());
    assert_eq!(result.outcome, ActionOutcome::Refused);
    assert_eq!(result.refusal, Some(ActionRefusal::Withdrawn));
    assert_eq!(
        result.message,
        "runnerd did not cancel run 500 (attempt 2): its config changed before the request went out (job actions \
         turned off, or the repositories changed). Nothing was sent."
    );
    assert_eq!(setup.github.sent(), Vec::new(), "nothing went to GitHub");
}

#[tokio::test]
async fn a_reload_while_the_run_is_read_again_before_the_send_withdraws_it() {
    let setup = setup(Reply::Status(202)).await;
    let token = confirm(&setup, JobAction::Cancel).await;
    // A reload that adds a repository, which leaves o/app's jobs open to actions, lands while the run is read
    // right before the send: the confirmation was shown under the config before it.
    let control = setup.control.clone();
    *setup.github.during_read.lock().unwrap() = Some(Box::new(move || {
        control.set_action_policy(
            api::JobActions::On,
            BTreeSet::from(["o/app".to_owned(), "o/new".to_owned()]),
        );
    }));
    let result = done(
        setup
            .actions
            .ask(11, JobAction::Cancel, Some(&token))
            .await
            .unwrap(),
    );
    assert_eq!(result.refusal, Some(ActionRefusal::Withdrawn));
    assert_eq!(setup.github.sent(), Vec::new(), "nothing went to GitHub");
}

#[tokio::test]
async fn a_new_attempt_between_preview_and_confirmation_gets_a_new_preview() {
    let setup = setup(Reply::Status(202)).await;
    let token = confirm(&setup, JobAction::Cancel).await;
    setup.github.set_run(Ok(running(3)));
    let fresh = preview(
        setup
            .actions
            .ask(11, JobAction::Cancel, Some(&token))
            .await
            .unwrap(),
    );
    assert_eq!(fresh.attempt, Some(3));
    assert_eq!(fresh.title, "Cancel run 500 (attempt 3) of o/app?");
    assert!(
        fresh.notes[0].starts_with("The run moved to another attempt"),
        "{:?}",
        fresh.notes
    );
    assert!(
        fresh.notes.iter().any(|note| note
            == "This job ran in attempt 2; the cancel stops attempt 3, the run's current one."),
        "{:?}",
        fresh.notes
    );
    assert_eq!(setup.github.sent(), Vec::new(), "nothing went to GitHub");
    let result = done(
        setup
            .actions
            .ask(11, JobAction::Cancel, fresh.confirm.as_deref())
            .await
            .unwrap(),
    );
    assert_eq!(result.outcome, ActionOutcome::Accepted);
    assert_eq!(setup.github.sent().len(), 1);
}

#[tokio::test]
async fn a_new_attempt_while_the_send_waits_for_its_token_gets_a_new_preview() {
    let setup = setup(Reply::Status(202)).await;
    let token = confirm(&setup, JobAction::Cancel).await;
    setup.github.hold();
    let asked = {
        let actions = setup.actions.clone();
        tokio::spawn(async move { actions.ask(11, JobAction::Cancel, Some(&token)).await })
    };
    setup.github.entered(1).await;
    setup.github.set_run(Ok(running(3)));
    setup.github.release();
    let fresh = preview(asked.await.unwrap().unwrap());
    assert_eq!(fresh.attempt, Some(3));
    assert!(
        fresh.notes[0].starts_with("The run moved to another attempt"),
        "{:?}",
        fresh.notes
    );
    assert_eq!(setup.github.sent(), Vec::new(), "nothing went to GitHub");
}

#[tokio::test]
async fn a_run_that_cannot_be_read_gets_a_preview_with_no_confirmation() {
    let setup = setup(Reply::Status(202)).await;
    setup
        .github
        .set_run(Err(ActionFailure::Status { status: 404 }));
    let unread = preview(
        setup
            .actions
            .ask(11, JobAction::Cancel, None)
            .await
            .unwrap(),
    );
    assert_eq!(
        unread.confirm, None,
        "nothing can be sent on a target not read"
    );
    assert_eq!(unread.attempt, None);
    assert_eq!(unread.title, "Cancel run 500 of o/app?");
    assert_eq!(
        unread.notes,
        [
            "runnerd could not read run 500 from GitHub (it answered 404), so it cannot say which attempt this would \
          act on, and offers no confirmation. Ask again later."
        ]
    );
    // Read at the preview, not right before the send: a new preview, still with nothing to confirm.
    setup.github.set_run(Ok(running(2)));
    let token = confirm(&setup, JobAction::Cancel).await;
    setup.github.set_run(Err(ActionFailure::TimedOut));
    let unverified = preview(
        setup
            .actions
            .ask(11, JobAction::Cancel, Some(&token))
            .await
            .unwrap(),
    );
    assert!(
        unverified.notes[0]
            .starts_with("runnerd could not read the run again right before sending"),
        "{:?}",
        unverified.notes
    );
    assert!(
        unverified.notes[1].contains("(it did not answer)"),
        "{:?}",
        unverified.notes
    );
    assert_eq!(unverified.confirm, None);
    assert_eq!(setup.github.sent(), Vec::new(), "nothing went to GitHub");
}

#[tokio::test]
async fn re_runs_name_the_jobs_that_depend_on_them_and_go_to_their_own_endpoints() {
    let setup = setup(Reply::Status(201)).await;
    setup.github.set_run(Ok(completed(2)));
    let shown = preview(setup.actions.ask(11, JobAction::Rerun, None).await.unwrap());
    assert_eq!(shown.title, "Re-run build 11 of o/app?");
    assert_eq!(
        shown.scope,
        "GitHub re-runs this job and the jobs that depend on it, in a new attempt of run 500."
    );
    assert_eq!(
        shown.notes,
        Vec::<String>::new(),
        "a completed run, this job of its attempt"
    );
    let rerun = send(&setup, JobAction::Rerun).await;
    assert_eq!(rerun.outcome, ActionOutcome::Accepted);
    assert_eq!(rerun.status, Some(201));
    let failed = send(&setup, JobAction::RerunFailed).await;
    assert!(
        failed
            .message
            .contains("failed jobs of run 500 (attempt 2) and the jobs that depend on them"),
        "{}",
        failed.message
    );
    assert_eq!(
        setup.github.sent(),
        [
            ("o/app".to_owned(), JobActionRequest::RerunJob { job: 11 }),
            (
                "o/app".to_owned(),
                JobActionRequest::RerunFailed { run: 500 }
            ),
        ]
    );
}

#[tokio::test]
async fn each_refusal_is_worded_by_its_class_and_status_and_changes_nothing_here() {
    let until = SystemTime::UNIX_EPOCH + Duration::from_secs(4_000_000_000);
    let cases = [
        (
            Reply::Status(403),
            ActionRefusal::Forbidden,
            Some(403),
            "may lack Actions: write",
        ),
        (
            Reply::Status(404),
            ActionRefusal::NotFound,
            Some(404),
            "does not show the run",
        ),
        (
            Reply::Status(410),
            ActionRefusal::NotFound,
            Some(410),
            "does not show the run",
        ),
        (
            Reply::Status(401),
            ActionRefusal::Unauthorized,
            Some(401),
            "refused the token",
        ),
        (
            Reply::Status(422),
            ActionRefusal::Unprocessable,
            Some(422),
            "cannot do that",
        ),
        (
            Reply::Status(400),
            ActionRefusal::Failed,
            Some(400),
            "it failed",
        ),
        (
            Reply::Fail(ActionFailure::RateLimited {
                status: Some(429),
                until,
            }),
            ActionRefusal::RateLimited,
            Some(429),
            "GitHub refused to re-run build 11 (429): its rate limit is used up",
        ),
        (
            Reply::Fail(ActionFailure::RateLimited {
                status: None,
                until,
            }),
            ActionRefusal::RateLimited,
            None,
            "GitHub's rate limit is used up, so runnerd did not re-run build 11",
        ),
        (
            Reply::Fail(ActionFailure::NoToken),
            ActionRefusal::NoToken,
            None,
            "no GitHub token",
        ),
        (
            Reply::Fail(ActionFailure::Unreachable),
            ActionRefusal::Unreachable,
            None,
            "could not reach GitHub",
        ),
    ];
    for (reply, refusal, status, words) in cases {
        let setup = setup(reply).await;
        let before = job_11(&setup.store).await;
        let result = send(&setup, JobAction::Rerun).await;
        assert_eq!(result.outcome, ActionOutcome::Refused, "{}", result.message);
        assert_eq!(result.refusal, Some(refusal));
        assert_eq!(result.status, status);
        assert!(result.message.contains(words), "{}", result.message);
        setup.store.flush().await;
        assert_eq!(
            job_11(&setup.store).await,
            before,
            "the job's record stays as it was"
        );
        assert_eq!(setup.github.sent().len(), 1, "a refusal is not sent again");
    }
}

#[tokio::test]
async fn a_cancels_409_is_already_finished_only_when_the_run_says_it_completed() {
    let setup = setup(Reply::Status(409)).await;
    let token = confirm(&setup, JobAction::Cancel).await;
    // The run completes between the preview and the confirmation; GitHub answers 409 to the cancel.
    setup.github.set_run(Ok(completed(2)));
    let before = job_11(&setup.store).await;
    let finished = done(
        setup
            .actions
            .ask(11, JobAction::Cancel, Some(&token))
            .await
            .unwrap(),
    );
    assert_eq!(finished.outcome, ActionOutcome::AlreadyFinished);
    assert_eq!(finished.status, Some(409));
    assert_eq!(
        finished.message,
        "Run 500 of o/app had completed already: nothing was cancelled."
    );
    assert!(finished.succeeded(), "nothing was left to do");
    assert_eq!(job_11(&setup.store).await, before);
    // A 409 for a run GitHub still shows running is a conflict, not a run that finished.
    let setup = self::setup(Reply::Status(409)).await;
    let conflict = send(&setup, JobAction::Cancel).await;
    assert_eq!(conflict.outcome, ActionOutcome::Refused);
    assert_eq!(conflict.refusal, Some(ActionRefusal::Conflict));
    assert!(!conflict.succeeded(), "a conflict is no success");
    // A re-run's 409 is a refusal.
    let setup = self::setup(Reply::Status(409)).await;
    let rerun = send(&setup, JobAction::Rerun).await;
    assert_eq!(rerun.refusal, Some(ActionRefusal::Conflict));
}

#[tokio::test]
async fn every_server_error_and_a_broken_answer_is_unknown_and_never_sent_again() {
    for (reply, status, words) in [
        (
            Reply::Status(500),
            Some(500),
            "GitHub answered 500, a server error",
        ),
        (
            Reply::Status(501),
            Some(501),
            "GitHub answered 501, a server error",
        ),
        (
            Reply::Status(502),
            Some(502),
            "GitHub answered 502, a server error",
        ),
        (
            Reply::Status(503),
            Some(503),
            "GitHub answered 503, a server error",
        ),
        (
            Reply::Status(504),
            Some(504),
            "GitHub answered 504, a server error",
        ),
        (
            Reply::Fail(ActionFailure::Broken),
            None,
            "the connection broke",
        ),
        (
            Reply::Fail(ActionFailure::TimedOut),
            None,
            "did not answer in time",
        ),
        (Reply::Hang, None, "did not answer in time"),
    ] {
        let setup = setup(reply).await;
        let result = send(&setup, JobAction::Rerun).await;
        assert_eq!(result.outcome, ActionOutcome::Unknown, "{}", result.message);
        assert_eq!(result.status, status);
        assert!(result.message.contains(words), "{}", result.message);
        assert!(
            result
                .message
                .contains("Check the run on GitHub before asking again"),
            "{}",
            result.message
        );
        assert!(
            !result.message.contains("Nothing was done"),
            "{}",
            result.message
        );
        assert_eq!(
            setup.github.sent().len(),
            1,
            "an ambiguous request is sent once"
        );
        assert_eq!(setup.actions.in_flight(), 0, "the flight ended");
    }
}

#[tokio::test]
async fn simultaneous_confirmations_for_one_run_send_once_and_another_action_waits_its_turn() {
    let setup = setup(Reply::Status(202)).await;
    let token = confirm(&setup, JobAction::Cancel).await;
    let other_tab = confirm(&setup, JobAction::Cancel).await;
    let rerun = confirm(&setup, JobAction::Rerun).await;
    setup.github.hold();
    let clicks: Vec<_> = [&token, &token, &other_tab]
        .into_iter()
        .map(|token| {
            let (actions, token) = (setup.actions.clone(), token.clone());
            tokio::spawn(async move { actions.ask(11, JobAction::Cancel, Some(&token)).await })
        })
        .collect();
    setup.github.entered(1).await;
    while setup.actions.waiting() < 3 {
        tokio::task::yield_now().await;
    }
    let other = setup.actions.ask(11, JobAction::Rerun, Some(&rerun)).await;
    assert_eq!(
        other.unwrap_err(),
        ActionError::Busy(
            "a cancel of run 500 is being sent to GitHub; ask again once it answers".to_owned()
        )
    );
    setup.github.release();
    for click in clicks {
        let result = done(click.await.unwrap().unwrap());
        assert_eq!(result.outcome, ActionOutcome::Accepted);
    }
    assert_eq!(setup.github.sent().len(), 1, "three clicks, one request");
    assert_eq!(setup.actions.in_flight(), 0);
    // The re-run's confirmation was not taken while it waited its turn.
    let again = done(
        setup
            .actions
            .ask(11, JobAction::Rerun, Some(&rerun))
            .await
            .unwrap(),
    );
    assert_eq!(again.outcome, ActionOutcome::Accepted);
    assert_eq!(setup.github.sent().len(), 2);
}

#[tokio::test]
async fn a_requester_that_goes_away_leaves_the_request_to_finish() {
    let setup = setup(Reply::Status(202)).await;
    let token = confirm(&setup, JobAction::Cancel).await;
    setup.github.hold();
    let asked = {
        let actions = setup.actions.clone();
        tokio::spawn(async move { actions.ask(11, JobAction::Cancel, Some(&token)).await })
    };
    setup.github.entered(1).await;
    asked.abort();
    assert_eq!(
        setup.actions.in_flight(),
        1,
        "the request is still in flight"
    );
    setup.github.release();
    while setup.actions.in_flight() > 0 {
        tokio::task::yield_now().await;
    }
    assert_eq!(setup.github.sent().len(), 1);
}

#[tokio::test]
async fn a_stop_ends_what_is_in_flight_as_unknown_and_refuses_what_comes_after() {
    let setup = setup(Reply::Hang).await;
    let token = confirm(&setup, JobAction::Cancel).await;
    let asked = {
        let actions = setup.actions.clone();
        tokio::spawn(async move { actions.ask(11, JobAction::Cancel, Some(&token)).await })
    };
    while setup.github.sent().is_empty() {
        tokio::task::yield_now().await;
    }
    setup.control.shutdown(Shutdown::Graceful);
    let result = done(asked.await.unwrap().unwrap());
    assert_eq!(result.outcome, ActionOutcome::Unknown);
    assert!(
        result.message.contains("began to stop"),
        "{}",
        result.message
    );
    assert_eq!(setup.actions.in_flight(), 0);
    assert_eq!(
        setup
            .actions
            .ask(11, JobAction::Cancel, None)
            .await
            .unwrap_err(),
        ActionError::Stopped
    );
}

#[tokio::test]
async fn a_removed_repository_a_job_with_no_run_and_an_unknown_job_are_refused_before_github() {
    let setup = setup(Reply::Status(202)).await;
    assert!(matches!(
        setup.actions.ask(30, JobAction::Cancel, None).await,
        Err(ActionError::Forbidden(_))
    ));
    assert_eq!(
        setup
            .actions
            .ask(40, JobAction::Rerun, None)
            .await
            .unwrap_err(),
        ActionError::Invalid(api::actions_no_run(40))
    );
    assert!(matches!(
        setup.actions.ask(99, JobAction::Cancel, None).await,
        Err(ActionError::NotFound(_))
    ));
    assert!(matches!(
        setup.actions.ask(11, JobAction::Unsupported, None).await,
        Err(ActionError::Invalid(_))
    ));
    assert_eq!(setup.github.sent(), Vec::new(), "nothing went to GitHub");
}

#[tokio::test]
async fn a_job_the_history_learns_of_after_the_preview_keeps_its_confirmation() {
    let setup = setup(Reply::Status(202)).await;
    let token = confirm(&setup, JobAction::Cancel).await;
    setup.store.queued(
        "o/app",
        &QueuedJob {
            id: 14,
            run_id: 500,
            name: "late".to_owned(),
            run_attempt: 2,
            ..QueuedJob::default()
        },
        1_000_000,
    );
    setup.store.flush().await;
    let result = done(
        setup
            .actions
            .ask(11, JobAction::Cancel, Some(&token))
            .await
            .unwrap(),
    );
    assert_eq!(
        result.outcome,
        ActionOutcome::Accepted,
        "the confirmation binds the run and its attempt, not the jobs known of it"
    );
}

#[tokio::test]
async fn a_paused_repository_a_running_run_and_an_older_attempts_job_are_said_in_the_preview() {
    let setup = setup(Reply::Status(201)).await;
    setup
        .control
        .set_paused(
            &crate::scheduler::engine::Target::Repository("o/app".to_owned()),
            true,
        )
        .await
        .unwrap();
    let Asked::Preview(shown) = setup.actions.ask(12, JobAction::Rerun, None).await.unwrap() else {
        panic!("no preview");
    };
    assert_eq!(
        shown.notes,
        [
            "Run 500 has not completed; GitHub refuses to re-run a run that has not.",
            "This job ran in attempt 1; the run is at attempt 2 now.",
            "o/app is paused: runnerd starts none of the new attempt's jobs until it is resumed; other runners may \
             take them.",
        ]
    );
    let lone = preview(
        setup
            .actions
            .ask(20, JobAction::Cancel, None)
            .await
            .unwrap(),
    );
    assert_eq!(
        lone.siblings_note,
        "runnerd knows no other job of this run; GitHub may have some, on other runners."
    );
    setup.github.set_run(Ok(completed(1)));
    let done_run = preview(
        setup
            .actions
            .ask(20, JobAction::Cancel, None)
            .await
            .unwrap(),
    );
    assert_eq!(
        done_run.notes,
        ["Run 600 has completed: GitHub has nothing left to cancel."]
    );
}

#[tokio::test]
async fn every_name_a_preview_shows_is_cleaned_and_the_history_keeps_the_original() {
    let setup = setup(Reply::Status(201)).await;
    let hostile = "evil\u{1b}]52;c;aGk=\u{7}\r\u{1b}[2J\u{9b}31m name";
    setup.store.queued(
        "o/app",
        &QueuedJob {
            id: 15,
            run_id: 500,
            name: hostile.to_owned(),
            run_attempt: 2,
            workflow: Some(format!("CI {hostile}")),
            branch: Some(format!("main{hostile}")),
            ..QueuedJob::default()
        },
        1_000_000,
    );
    setup.store.flush().await;
    let clean = |text: &str| {
        assert!(
            !text.chars().any(|c| c.is_control() && c != '\t'),
            "no control character or escape in {text:?}"
        );
    };
    // The job itself, and as another job's sibling.
    let own = preview(setup.actions.ask(15, JobAction::Rerun, None).await.unwrap());
    clean(&own.title);
    clean(&own.message);
    let sibling = preview(
        setup
            .actions
            .ask(11, JobAction::Cancel, None)
            .await
            .unwrap(),
    );
    let shown = sibling.siblings.iter().find(|job| job.id == 15).unwrap();
    for text in [
        shown.name.as_str(),
        shown.workflow.as_deref().unwrap(),
        shown.branch.as_deref().unwrap(),
        &shown.title().to_string(),
    ] {
        clean(text);
    }
    assert!(shown.name.ends_with(" name"), "{:?}", shown.name);
    let kept = setup.store.job(15).await.unwrap().unwrap().summary;
    assert_eq!(kept.name, hostile, "the history keeps what GitHub sent");
}

#[tokio::test]
async fn a_run_with_more_jobs_than_a_preview_lists_says_it_has_more() {
    let setup = setup(Reply::Status(202)).await;
    let preview_of = |count: u64| {
        let setup = &setup;
        async move {
            // Job 20's run gets other jobs until it has `count` of them.
            for id in 1000..1000 + count {
                setup.store.queued(
                    "o/app",
                    &QueuedJob {
                        id,
                        run_id: 600,
                        name: format!("shard {id}"),
                        run_attempt: 1,
                        ..QueuedJob::default()
                    },
                    1_000_000,
                );
            }
            setup.store.flush().await;
            preview(
                setup
                    .actions
                    .ask(20, JobAction::Cancel, None)
                    .await
                    .unwrap(),
            )
        }
    };
    let max = u64::try_from(api::SIBLINGS_MAX).unwrap();
    let full = preview_of(max).await;
    assert_eq!(full.siblings.len(), api::SIBLINGS_MAX);
    assert_eq!(
        full.siblings_note,
        "runnerd knows 50 other jobs of this run; GitHub may have more, on other runners."
    );
    let over = preview_of(max + 1).await;
    assert_eq!(
        over.siblings.len(),
        api::SIBLINGS_MAX,
        "the list stops at the most"
    );
    assert_eq!(
        over.siblings_note,
        "runnerd knows more than 50 other jobs of this run, the first 50 shown; GitHub may have more still."
    );
}

#[test]
fn a_policy_is_read_whole_while_reloads_replace_it() {
    use super::policy::Policies;
    let a = BTreeSet::from(["o/a".to_owned()]);
    let a_and_b = BTreeSet::from(["o/a".to_owned(), "o/b".to_owned()]);
    let policies = Arc::new(Policies::new(api::JobActions::On, a.clone()));
    let first = policies.now().revision();
    std::thread::scope(|scope| {
        let reloads = {
            let policies = policies.clone();
            let (a, a_and_b) = (a.clone(), a_and_b.clone());
            scope.spawn(move || {
                // On without o/b, then off with it: neither lets o/b's jobs be acted on.
                for _ in 0..2000 {
                    policies.replace(api::JobActions::On, a.clone());
                    policies.replace(api::JobActions::Off, a_and_b.clone());
                }
            })
        };
        for _ in 0..4 {
            let policies = policies.clone();
            scope.spawn(move || {
                for _ in 0..2000 {
                    assert!(
                        policies.now().allows("o/b").is_err(),
                        "no snapshot is half one reload, half the other"
                    );
                }
            });
        }
        reloads.join().unwrap();
    });
    assert!(
        policies.now().revision() > first,
        "each change moves the revision"
    );
    let settled = policies.now().revision();
    policies.replace(api::JobActions::Off, a_and_b);
    assert_eq!(
        policies.now().revision(),
        settled,
        "a reload that changes neither keeps it"
    );
}
