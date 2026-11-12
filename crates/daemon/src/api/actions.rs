//! The job action routes (ADR 0134): `POST /v1/jobs/{id}/cancel` and `POST /v1/jobs/{id}/rerun`. Each takes the
//! authenticated principal and hands the request to core's `JobActions`, which previews it, checks the config of the
//! moment and sends it to GitHub; this module only maps the answer to HTTP. It never touches the engine's slots.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use runnerd_core::actions::{ActionError, Asked};
use runnerd_core::api::{ActionQuery, JobAction, Principal};
use tracing::info;

use super::{AppState, Authenticated, error};

/// `POST /v1/jobs/{id}/cancel`: cancels the job's workflow run.
pub(super) async fn cancel(
    Authenticated(principal): Authenticated,
    State(state): State<AppState>,
    Path(id): Path<u64>,
    Query(query): Query<ActionQuery>,
) -> Response {
    if query.failed {
        return error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "failed applies to a re-run, not to a cancel",
        );
    }
    ask(&state, &principal, id, JobAction::Cancel, &query).await
}

/// `POST /v1/jobs/{id}/rerun`: re-runs the job, or with `failed=true` its run's failed jobs.
pub(super) async fn rerun(
    Authenticated(principal): Authenticated,
    State(state): State<AppState>,
    Path(id): Path<u64>,
    Query(query): Query<ActionQuery>,
) -> Response {
    let action = if query.failed {
        JobAction::RerunFailed
    } else {
        JobAction::Rerun
    };
    ask(&state, &principal, id, action, &query).await
}

async fn ask(
    state: &AppState,
    principal: &Principal,
    id: u64,
    action: JobAction,
    query: &ActionQuery,
) -> Response {
    let confirm = query.confirm.as_deref().filter(|_| query.force);
    if confirm.is_some() {
        info!("job {id}: {} confirmed by {principal:?}", action.slug());
    }
    match state.handle.actions.ask(id, action, confirm).await {
        Ok(Asked::Preview(preview)) => (StatusCode::CONFLICT, Json(*preview)).into_response(),
        Ok(Asked::Done(result)) => Json(result).into_response(),
        Err(refused) => {
            let status = match &refused {
                ActionError::NotFound(_) => StatusCode::NOT_FOUND,
                ActionError::Forbidden(_) => StatusCode::FORBIDDEN,
                ActionError::Invalid(_) => StatusCode::UNPROCESSABLE_ENTITY,
                ActionError::Busy(_) => StatusCode::CONFLICT,
                ActionError::Failed(_) => StatusCode::INTERNAL_SERVER_ERROR,
                ActionError::Stopped => StatusCode::SERVICE_UNAVAILABLE,
            };
            error(status, &refused.to_string())
        }
    }
}
