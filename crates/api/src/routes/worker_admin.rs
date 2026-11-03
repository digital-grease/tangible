// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

// See the note in `pagination`: `Problem` is a response document, not a
// hot-path error.
#![allow(clippy::result_large_err)]

//! Workers as people manage them (docs/plans/worker-admin.md, API.md 8.6).
//!
//! Seeing workers needs `read`, as seeing drives does. Draining, enabling and
//! revoking one needs `manage_workers` and is audited. A drained worker hears
//! it at its next heartbeat, finishes what it holds and is given nothing new;
//! a revoked one's credential stops working at its next request, and it is
//! never interrupted mid-write by the server.

use axum::extract::{Path, Query, State};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Serialize;
use tangible_db::workers::{
    WorkerChange, WorkerRecord, drain_worker, enable_worker, get_worker, list_workers,
    revoke_worker,
};
use tangible_domain::WorkerId;
use time::OffsetDateTime;
use utoipa::ToSchema;

use crate::auth::SignedIn;
use crate::pagination::{Page, PageQuery};
use crate::problem::{ErrorCode, Problem};
use crate::state::ApiState;

fn rfc3339(value: OffsetDateTime) -> String {
    value
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
}

fn unavailable(detail: &str) -> Problem {
    Problem::new(ErrorCode::StorageUnavailable, detail)
}

/// One of a worker's drives.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct WorkerDriveView {
    /// Opaque identifier.
    pub id: String,
    /// What the worker calls it.
    pub name: String,
}

/// A worker.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct WorkerView {
    /// Opaque identifier.
    pub id: String,
    /// Its name, chosen when it enrolled.
    pub name: String,
    /// `pending` (never heard from), `online`, `offline`, `draining`,
    /// `revoked` or `incompatible`.
    pub status: String,
    /// The Tangible version it runs.
    pub software_version: String,
    /// The worker protocol version it speaks.
    pub protocol_version: String,
    /// When it last reported in.
    pub last_seen_at: Option<String>,
    /// When it enrolled.
    pub created_at: String,
    /// When it was revoked.
    pub revoked_at: Option<String>,
    /// Its drives, by name.
    pub drives: Vec<WorkerDriveView>,
}

impl From<WorkerRecord> for WorkerView {
    fn from(record: WorkerRecord) -> Self {
        Self {
            id: record.id.to_string(),
            name: record.name,
            status: record.status,
            software_version: record.software_version,
            protocol_version: record.protocol_version,
            last_seen_at: record.last_seen_at.map(rfc3339),
            created_at: rfc3339(record.created_at),
            revoked_at: record.revoked_at.map(rfc3339),
            drives: record
                .drives
                .into_iter()
                .map(|(id, name)| WorkerDriveView {
                    id: id.to_string(),
                    name,
                })
                .collect(),
        }
    }
}

/// A page of workers.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct WorkerList {
    /// The workers in this page, oldest first.
    pub items: Vec<WorkerView>,
    /// Cursor for the next page, or null when this is the last.
    pub next_cursor: Option<String>,
}

fn parse_worker(raw: &str) -> Result<WorkerId, Problem> {
    raw.parse::<WorkerId>()
        .map_err(|_| Problem::invalid_parameter("worker_id", "not a valid identifier"))
}

/// A page of workers, oldest first.
///
/// # Errors
///
/// `INVALID_CURSOR` for a cursor this server did not issue, or
/// `STORAGE_UNAVAILABLE` if workers cannot be read.
#[utoipa::path(
    get,
    path = "/api/v1/workers",
    tag = "workers",
    description = "List workers, oldest first, with their status and drives. Use \
                   `next_cursor` from the response to fetch the following page.",
    params(PageQuery),
    responses(
        (status = 200, description = "A page of workers", body = WorkerList),
        (status = 400, description = "Invalid cursor", body = Problem),
    ),
)]
pub async fn list_all_workers(
    State(state): State<ApiState>,
    Query(query): Query<PageQuery>,
) -> Result<Json<WorkerList>, Problem> {
    let limit = query.limit();
    let after = query
        .after()?
        .map(|after| {
            uuid::Uuid::parse_str(&after)
                .map_err(|_| Problem::new(ErrorCode::InvalidCursor, "the cursor is not usable"))
        })
        .transpose()?;
    let workers = list_workers(
        state.database().pool(),
        i64::try_from(limit).unwrap_or(i64::MAX),
        after,
    )
    .await
    .map_err(|error| {
        tracing::error!(error = ?error, "could not list workers");
        unavailable("workers could not be read")
    })?;
    let items: Vec<WorkerView> = workers.into_iter().map(WorkerView::from).collect();
    let page = Page::new(items, limit, |worker| worker.id.clone());
    Ok(Json(WorkerList {
        items: page.items,
        next_cursor: page.next_cursor,
    }))
}

/// One worker.
///
/// # Errors
///
/// `NOT_FOUND` for an unknown worker.
#[utoipa::path(
    get,
    path = "/api/v1/workers/{worker_id}",
    tag = "workers",
    params(("worker_id" = String, Path, description = "Worker identifier")),
    responses(
        (status = 200, description = "The worker", body = WorkerView),
        (status = 404, description = "No such worker", body = Problem),
    ),
)]
pub async fn get_one_worker(
    State(state): State<ApiState>,
    Path(worker_id): Path<String>,
) -> Result<Json<WorkerView>, Problem> {
    let id = parse_worker(&worker_id)?;
    get_worker(state.database().pool(), id)
        .await
        .map_err(|error| {
            tracing::error!(error = ?error, "could not read a worker");
            unavailable("the worker could not be read")
        })?
        .map(|record| Json(record.into()))
        .ok_or_else(|| Problem::not_found("worker", &worker_id))
}

/// The answer to a change: the worker as it now is, or why not.
fn answer(change: WorkerChange, worker_id: &str, verb: &str) -> Result<Json<WorkerView>, Problem> {
    match change {
        WorkerChange::Done(record) => Ok(Json(record.into())),
        WorkerChange::NotFound => Err(Problem::not_found("worker", worker_id)),
        WorkerChange::Refused(status) => Err(Problem::new(
            ErrorCode::Conflict,
            format!("a worker that is {status} cannot be {verb}"),
        )),
    }
}

/// Drain a worker: it finishes what it holds and takes nothing new.
///
/// # Errors
///
/// `NOT_FOUND` for an unknown worker, `CONFLICT` for one revoked or
/// incompatible.
#[utoipa::path(
    post,
    path = "/api/v1/workers/{worker_id}/drain",
    tag = "workers",
    description = "Drain a worker: at its next heartbeat it is told to take no new work, \
                   and claims answer it with nothing to do, while what it holds runs to \
                   the end. Draining a draining worker changes nothing. Needs an \
                   administrator, and is audited.",
    params(("worker_id" = String, Path, description = "Worker identifier")),
    responses(
        (status = 200, description = "Draining", body = WorkerView),
        (status = 404, description = "No such worker", body = Problem),
        (status = 409, description = "Revoked or incompatible", body = Problem),
    ),
)]
pub async fn drain_one_worker(
    State(state): State<ApiState>,
    SignedIn(user): SignedIn,
    Path(worker_id): Path<String>,
) -> Result<Json<WorkerView>, Problem> {
    let id = parse_worker(&worker_id)?;
    let change = drain_worker(state.database().pool(), id, &user.username)
        .await
        .map_err(|error| {
            tracing::error!(error = ?error, "could not drain a worker");
            unavailable("the worker could not be drained")
        })?;
    if matches!(change, WorkerChange::Done(_)) {
        tracing::info!(worker_id = %id, by = %user.username, "worker drained");
    }
    answer(change, &worker_id, "drained")
}

/// Undo a drain.
///
/// # Errors
///
/// `NOT_FOUND` for an unknown worker, `CONFLICT` for one revoked or
/// incompatible.
#[utoipa::path(
    post,
    path = "/api/v1/workers/{worker_id}/enable",
    tag = "workers",
    description = "Undo a drain: the worker takes work again. Enabling a worker that is \
                   not draining changes nothing. Needs an administrator, and is audited.",
    params(("worker_id" = String, Path, description = "Worker identifier")),
    responses(
        (status = 200, description = "Taking work", body = WorkerView),
        (status = 404, description = "No such worker", body = Problem),
        (status = 409, description = "Revoked or incompatible", body = Problem),
    ),
)]
pub async fn enable_one_worker(
    State(state): State<ApiState>,
    SignedIn(user): SignedIn,
    Path(worker_id): Path<String>,
) -> Result<Json<WorkerView>, Problem> {
    let id = parse_worker(&worker_id)?;
    let change = enable_worker(state.database().pool(), id, &user.username)
        .await
        .map_err(|error| {
            tracing::error!(error = ?error, "could not enable a worker");
            unavailable("the worker could not be enabled")
        })?;
    if matches!(change, WorkerChange::Done(_)) {
        tracing::info!(worker_id = %id, by = %user.username, "worker enabled");
    }
    answer(change, &worker_id, "enabled")
}

/// Revoke a worker, for good.
///
/// # Errors
///
/// `NOT_FOUND` for an unknown worker.
#[utoipa::path(
    post,
    path = "/api/v1/workers/{worker_id}/revoke",
    tag = "workers",
    description = "Revoke a worker's credential, for good: its next request is refused, \
                   so it can no longer take work, renew a lease or report. An attempt it \
                   held lapses as a lost worker's does; nothing interrupts a write. To use \
                   the machine again, enroll it with a new token under a new name. \
                   Revoking a revoked worker changes nothing. Needs an administrator, and \
                   is audited.",
    params(("worker_id" = String, Path, description = "Worker identifier")),
    responses(
        (status = 200, description = "Revoked", body = WorkerView),
        (status = 404, description = "No such worker", body = Problem),
    ),
)]
pub async fn revoke_one_worker(
    State(state): State<ApiState>,
    SignedIn(user): SignedIn,
    Path(worker_id): Path<String>,
) -> Result<Json<WorkerView>, Problem> {
    let id = parse_worker(&worker_id)?;
    let change = revoke_worker(state.database().pool(), id, &user.username)
        .await
        .map_err(|error| {
            tracing::error!(error = ?error, "could not revoke a worker");
            unavailable("the worker could not be revoked")
        })?;
    if matches!(change, WorkerChange::Done(_)) {
        tracing::warn!(worker_id = %id, by = %user.username, "worker revoked");
    }
    answer(change, &worker_id, "revoked")
}

/// The routes for managing workers.
pub fn router() -> Router<ApiState> {
    Router::new()
        .route("/workers", get(list_all_workers))
        .route("/workers/{worker_id}", get(get_one_worker))
        .route("/workers/{worker_id}/drain", post(drain_one_worker))
        .route("/workers/{worker_id}/enable", post(enable_one_worker))
        .route("/workers/{worker_id}/revoke", post(revoke_one_worker))
}
