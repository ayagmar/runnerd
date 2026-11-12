//! How a client reads the job actions' answers, a newer daemon's included, and a daemon that has none.

#![allow(clippy::unwrap_used, reason = "tests")]

use super::{
    ActionAnswer, ActionOutcome, ActionPreview, ActionRefusal, ActionResult, ActionTarget,
    JobAction, JobActions,
};
use crate::api::Status;

/// A result and a preview as a newer daemon might send them: an action, outcome and refusal this client does not
/// know, and fields it does not have.
const FUTURE_RESULT: &str = r#"{
    "action": "teleport", "job": 5, "repository": "o/app", "run": 77, "outcome": "rolled-back",
    "status": 299, "refusal": "quantum", "message": "something new", "evidence": {"kind": "new"}
}"#;

#[test]
fn a_newer_daemons_action_outcome_and_refusal_read_as_unsupported_unknown_and_other() {
    let result: ActionResult = serde_json::from_str(FUTURE_RESULT).unwrap();
    assert_eq!(result.action, JobAction::Unsupported);
    assert_eq!(
        result.outcome,
        ActionOutcome::Unknown,
        "never read as accepted"
    );
    assert_eq!(result.refusal, Some(ActionRefusal::Other));
    assert!(!result.succeeded());
    let preview: ActionPreview = serde_json::from_str(
        r#"{"message": "m", "action": "teleport", "job": 5, "repository": "o/app", "run": 77, "title": "t",
            "scope": "s", "siblings_note": "n", "confirm_label": "Go", "confirm": "c", "risk": 3}"#,
    )
    .unwrap();
    assert_eq!(preview.action, JobAction::Unsupported);
    assert!(preview.siblings.is_empty() && preview.notes.is_empty());
}

#[test]
fn a_daemon_without_job_actions_or_with_a_newer_state_offers_none() {
    let mut status = serde_json::json!({
        "version": "0.1.0", "shutting_down": false, "budget": {
            "slice": "s", "memory_max": null, "memory_in_use": 0, "admission": "caps", "held": false,
            "mirror_memory": 0, "seed_memory": [], "shell_memory": []
        }, "profiles": [], "repositories": [], "queued": []
    });
    let read = |status: &serde_json::Value| {
        serde_json::from_value::<Status>(status.clone())
            .unwrap()
            .job_actions
    };
    assert_eq!(
        read(&status),
        JobActions::Unsupported,
        "a daemon without the field"
    );
    status["job_actions"] = "on".into();
    assert_eq!(read(&status), JobActions::On);
    status["job_actions"] = "partial".into();
    assert_eq!(
        read(&status),
        JobActions::Unsupported,
        "a state this client does not know"
    );
    assert!(!JobActions::Unsupported.offered());
    assert_eq!(
        JobActions::Unsupported.why_not(),
        Some("this runnerd cannot cancel or re-run jobs: it is older than this client")
    );
}

#[test]
fn a_rate_limit_says_when_to_ask_again() {
    let target = ActionTarget {
        action: JobAction::Cancel,
        job: 5,
        name: "test".to_owned(),
        repository: "o/app".to_owned(),
        run: 77,
        attempt: None,
    };
    let result = ActionResult::new(
        &target,
        ActionAnswer::Refused {
            refusal: ActionRefusal::RateLimited,
            status: None,
            until: Some(1_000 + 720),
        },
        1_000,
    );
    assert_eq!(
        result.message,
        "GitHub's rate limit is used up, so runnerd did not cancel run 77; try again in 12m 00s."
    );
    assert_eq!(result.until, Some(1_720));
}
