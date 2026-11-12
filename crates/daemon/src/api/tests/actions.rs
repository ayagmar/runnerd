//! Cancelling and re-running a job through the socket (ADR 0134): a preview and the confirmation it issues before
//! anything is sent, a confirmed request's answer, the config of the moment, and the shared client's reading of each
//! answer, with a fake GitHub.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use futures_util::future::BoxFuture;
use reqwest::StatusCode;
use runnerd_core::actions::ActionsGithub;
use runnerd_core::api::{
    ActionOutcome, ActionPreview, ActionQuery, ActionResult, ErrorResponse, JobAction, JobActions,
};
use runnerd_core::client::{Client, JobActionAnswer};
use runnerd_core::github::{ActionFailure, JobActionRequest, Permit, QueuedJob, RunNow};

use super::{Daemon, body, start};
use crate::testdir::Dir;

/// A GitHub whose runs are at attempt 1, that answers every action with `status` and keeps what it was sent.
pub(super) struct FakeGithub {
    status: Mutex<u16>,
    sent: Mutex<Vec<JobActionRequest>>,
}

impl FakeGithub {
    pub(super) fn new() -> Arc<FakeGithub> {
        Arc::new(FakeGithub {
            status: Mutex::new(202),
            sent: Mutex::new(Vec::new()),
        })
    }

    fn sent(&self) -> Vec<JobActionRequest> {
        self.sent.lock().unwrap().clone()
    }
}

impl ActionsGithub for FakeGithub {
    fn run<'call>(
        &'call self,
        _repository: &'call str,
        _run: u64,
    ) -> BoxFuture<'call, Result<RunNow, ActionFailure>> {
        Box::pin(async {
            Ok(RunNow {
                attempt: 1,
                completed: false,
            })
        })
    }

    fn send<'call>(
        &'call self,
        _repository: &'call str,
        request: JobActionRequest,
        permit: &'call Permit<'call>,
    ) -> BoxFuture<'call, Result<u16, ActionFailure>> {
        Box::pin(async move {
            if !permit().await {
                return Err(ActionFailure::Withdrawn);
            }
            self.sent.lock().unwrap().push(request);
            let status = *self.status.lock().unwrap();
            match status {
                200..300 => Ok(status),
                _ => Err(ActionFailure::Status { status }),
            }
        })
    }
}

/// A daemon whose history has job 5 of run 77 of `o/app`, and job 6 of the same run.
async fn serving() -> (Daemon, Dir) {
    let dir = Dir::new();
    let daemon = start(&dir.0, None).await;
    for id in [5, 6] {
        daemon.store.queued(
            "o/app",
            &QueuedJob {
                id,
                run_id: 77,
                name: format!("test {id}"),
                run_attempt: 1,
                ..QueuedJob::default()
            },
            1_000,
        );
    }
    daemon.store.flush().await;
    (daemon, dir)
}

fn client(dir: &Dir) -> Client {
    Client::new(dir.0.join("runnerd.sock"))
}

#[tokio::test]
async fn a_cancel_is_previewed_with_409_and_sent_only_with_the_confirmation_it_issued() {
    let (daemon, _dir) = serving().await;
    let response = daemon
        .client
        .post("http://runnerd/v1/jobs/5/cancel")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CONFLICT);
    let bytes = body(response).await;
    let preview: ActionPreview = serde_json::from_slice(&bytes).unwrap();
    let error: ErrorResponse = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        error.message, preview.message,
        "an ErrorResponse reader gets the message"
    );
    assert_eq!(preview.siblings.len(), 1);
    assert_eq!(preview.attempt, Some(1));
    let token = preview.confirm.unwrap();
    // `force` alone, or with a confirmation of the form a job's details would make, is no confirmation.
    for query in ["force=true", "force=true&confirm=cancel%3Ao%2Fapp%3A77%3A5"] {
        let forced = daemon
            .client
            .post(format!("http://runnerd/v1/jobs/5/cancel?{query}"))
            .send()
            .await
            .unwrap();
        assert_eq!(forced.status(), StatusCode::CONFLICT, "{query}");
    }
    assert!(daemon.github.sent().is_empty(), "nothing went to GitHub");
    let response = daemon
        .client
        .post(format!(
            "http://runnerd/v1/jobs/5/cancel?force=true&confirm={token}"
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let result: ActionResult = serde_json::from_slice(&body(response).await).unwrap();
    assert_eq!(result.outcome, ActionOutcome::Accepted);
    assert_eq!(
        daemon.github.sent(),
        [JobActionRequest::CancelRun { run: 77 }]
    );
}

#[tokio::test]
async fn the_shared_client_reads_a_preview_a_result_and_each_refusal() {
    let (daemon, dir) = serving().await;
    let client = client(&dir);
    let rerun = ActionQuery {
        failed: true,
        ..ActionQuery::default()
    };
    let JobActionAnswer::Confirm(preview) = client
        .job_action(5, JobAction::Rerun, &rerun)
        .await
        .unwrap()
    else {
        panic!("no preview");
    };
    assert_eq!(preview.action, JobAction::RerunFailed);
    let confirmed = ActionQuery {
        force: true,
        confirm: preview.confirm.clone(),
        failed: true,
    };
    *daemon.github.status.lock().unwrap() = 403;
    let JobActionAnswer::Done(result) = client
        .job_action(5, JobAction::Rerun, &confirmed)
        .await
        .unwrap()
    else {
        panic!("not sent");
    };
    assert_eq!(result.outcome, ActionOutcome::Refused);
    assert_eq!(
        daemon.github.sent(),
        [JobActionRequest::RerunFailed { run: 77 }]
    );
    // Off in the config of the moment: 403, worded.
    let configured = BTreeSet::from(["o/app".to_owned()]);
    daemon
        .control
        .set_action_policy(JobActions::Off, configured.clone());
    let off = client
        .job_action(5, JobAction::Cancel, &ActionQuery::default())
        .await
        .unwrap_err();
    assert!(
        matches!(&off, runnerd_core::client::ClientError::Api { status, message, .. }
            if *status == StatusCode::FORBIDDEN && message.contains("job_actions is false")),
        "{off}"
    );
    daemon.control.set_action_policy(JobActions::On, configured);
    let unknown = client
        .job_action(99, JobAction::Cancel, &ActionQuery::default())
        .await
        .unwrap_err();
    assert!(
        matches!(&unknown, runnerd_core::client::ClientError::Api { status, .. } if *status == StatusCode::NOT_FOUND),
        "{unknown}"
    );
    assert!(
        !unknown.is_unsupported(),
        "a job the daemon lacks is not a route it lacks"
    );
    let cancel_failed = daemon
        .client
        .post("http://runnerd/v1/jobs/5/cancel?failed=true")
        .send()
        .await
        .unwrap();
    assert_eq!(cancel_failed.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(daemon.github.sent().len(), 1);
}
