// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

// See the note in `pagination`: `Problem` is a response document, not a
// hot-path error.
#![allow(clippy::result_large_err)]

//! The store as a whole, for administrators: what garbage collection would
//! remove (docs/plans/storage-integrity.md, phase S2). A plan, never a
//! removal.

use axum::extract::{Query, State};
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use utoipa::IntoParams;

use crate::gc::{DEFAULT_GRACE_DAYS, GcPlan, GcSettings, MAX_GRACE_DAYS, plan};
use crate::problem::{ErrorCode, Problem};
use crate::state::ApiState;

/// How many candidates and missing objects a plan names unless asked.
pub const DEFAULT_NAMED: usize = 100;

/// The most a plan names.
pub const MAX_NAMED: usize = 1000;

/// How a plan is made.
#[derive(Debug, Clone, Default, Deserialize, IntoParams)]
pub struct GcPlanQuery {
    /// How young, in days, an unreferenced object is kept anyway: 0 to
    /// 3650, 7 unless given.
    pub grace_days: Option<u32>,
    /// How many candidates and missing objects to name, largest first:
    /// clamped to 1 to 1000, 100 unless given. All are counted.
    pub limit: Option<usize>,
}

/// What garbage collection would remove now. Nothing is removed.
///
/// # Errors
///
/// `INVALID_PARAMETER` for a grace period out of range;
/// `STORAGE_UNAVAILABLE` when the store or catalogue cannot be read.
#[utoipa::path(
    get,
    path = "/api/v1/storage/gc-plan",
    tag = "library",
    description = "Plan garbage collection, a dry run: walk the object store and report the \\
                   objects nothing refers to (no artifact, published manifest or import under \\
                   way, no reference count or retention lock) and older than the grace period, \\
                   with what removing them would free. Also counts leftover partial files and \\
                   recorded objects whose file is missing. Nothing is removed; this server \\
                   cannot remove objects. Reads every directory in the store, so it takes a \\
                   while on a large library.",
    params(GcPlanQuery),
    responses(
        (status = 200, description = "The plan", body = GcPlan),
        (status = 400, description = "A grace period out of range", body = Problem),
        (status = 503, description = "The store or catalogue cannot be read", body = Problem),
    ),
)]
pub async fn gc_plan(
    State(state): State<ApiState>,
    Query(query): Query<GcPlanQuery>,
) -> Result<Json<GcPlan>, Problem> {
    let grace_days = query.grace_days.unwrap_or(DEFAULT_GRACE_DAYS);
    if grace_days > MAX_GRACE_DAYS {
        return Err(Problem::invalid_parameter(
            "grace_days",
            "must be from 0 to 3650",
        ));
    }
    let keep = query.limit.unwrap_or(DEFAULT_NAMED).clamp(1, MAX_NAMED);
    let manifests = state.manifests().ok_or_else(|| {
        Problem::new(
            ErrorCode::StorageUnavailable,
            "this server has no library configured",
        )
    })?;
    let planned = plan(
        state.database(),
        manifests,
        GcSettings::days(grace_days, keep),
    )
    .await
    .map_err(|error| {
        tracing::error!(error = ?error, "a collection plan could not be made");
        Problem::new(
            ErrorCode::StorageUnavailable,
            "the store or the catalogue could not be read",
        )
    })?;
    Ok(Json(planned))
}

/// The storage routes.
pub fn router() -> Router<ApiState> {
    Router::new().route("/storage/gc-plan", get(gc_plan))
}
