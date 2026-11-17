// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

// See the note in `pagination`: `Problem` is a response document, not a
// hot-path error.
#![allow(clippy::result_large_err)]

//! The store as a whole, for administrators: what garbage collection would
//! remove (docs/plans/storage-integrity.md, phase S2), and collecting the
//! orphans of a plan kept for it (ADR 0024).

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use tangible_db::gc_plans::{self, PlanObjectRow, PlanRow, Refusal};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use utoipa::{IntoParams, ToSchema};

use crate::auth::SignedIn;
use crate::gc::{DEFAULT_GRACE_DAYS, GcPlan, GcSettings, MAX_GRACE_DAYS, Tally, plan};
use crate::orphans::{MIN_GRACE_DAYS, PLAN_LIFETIME, make_plan};
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

/// What garbage collection would remove now. Nothing is removed: collecting
/// takes a plan kept for it ([`create_gc_plan`]).
///
/// # Errors
///
/// `INVALID_PARAMETER` for a grace period out of range;
/// `STORAGE_UNAVAILABLE` when the store or catalogue cannot be read.
#[utoipa::path(
    get,
    path = "/api/v1/storage/gc-plan",
    tag = "library",
    description = "Plan garbage collection, a dry run: walk the object store and report the \
                   objects nothing refers to (no artifact, published manifest or import under \
                   way, no reference count or retention lock) and older than the grace period, \
                   with what removing them would free. Also counts leftover partial files and \
                   recorded objects whose file is missing. Nothing is removed: collecting \
                   takes a plan kept for it (POST /api/v1/storage/gc-plans). Reads every \
                   directory in the store, so it takes a while on a large library.",
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

// --- plans kept for collecting (ADR 0024) ----------------------------------------

/// How many plans the list shows unless asked.
const DEFAULT_PLANS: i64 = 20;

/// The most it shows.
const MAX_PLANS: i64 = 100;

/// A plan to be kept for collecting.
#[derive(Debug, Clone, Default, Deserialize, ToSchema)]
pub struct NewGcPlan {
    /// How young, in days, an unreferenced object is kept anyway: 1 to 3650,
    /// 7 unless given. Under a day is refused: a plan to be collected leaves
    /// work that has only just stored its bytes alone.
    #[serde(default)]
    pub grace_days: Option<u32>,
}

/// A plan kept for collecting.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct StoredGcPlanView {
    /// Opaque identifier.
    pub id: String,
    /// When it was made.
    pub planned_at: String,
    /// The administrator who made it, or `null` when the schedule did.
    pub planned_by: Option<String>,
    /// How young an unreferenced object was kept anyway, in seconds.
    pub grace_seconds: i64,
    /// Whether every manifest and import checkpoint could be read. One that
    /// was not can never be collected.
    pub certain: bool,
    /// What collecting it would remove, as planned: orphaned objects older
    /// than the grace period.
    pub candidates: Tally,
    /// Partial files in the object tree older than the grace period, which
    /// collecting it also removes.
    pub stale_partial_files: i64,
    /// `planned`, which can be collected until `expires_at`; `expired`;
    /// `uncertain`, which cannot be collected; `queued`, asked to be
    /// collected, which the server does shortly, or once imports under way
    /// end; `collected`.
    pub state: String,
    /// When it can no longer be collected, unless asked to be by then.
    pub expires_at: String,
    /// When collecting it was asked for.
    pub collect_requested_at: Option<String>,
    /// Who asked, or `null` for the schedule.
    pub collect_requested_by: Option<String>,
    /// What collecting it did, once it has.
    pub collected: Option<GcCollectionView>,
}

/// What collecting a plan did.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct GcCollectionView {
    /// When it finished.
    pub collected_at: String,
    /// Candidates removed, and the bytes freed.
    pub removed: Tally,
    /// Candidates kept, each with its reason, and their bytes as planned.
    pub kept: Tally,
    /// Stale partial files removed.
    pub partial_files_removed: i64,
}

/// One candidate of a kept plan.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct GcPlanObjectView {
    /// Its digest.
    pub sha256: String,
    /// Its length when planned.
    pub size_bytes: i64,
    /// When its file was last written, when planned.
    pub modified_at: String,
    /// Whether the catalogue recorded it. One it did not was stored by work
    /// that never registered it.
    pub recorded: bool,
    /// What collecting did with it, once it has: `removed`; or kept, as
    /// `kept_live` (something refers to it now), `kept_held` (counted, or a
    /// retention lock), `kept_changed` (its file is not the one planned),
    /// `kept_gone` (no file), `kept_uncertain` (the marks could not all be
    /// read), `kept_failed` (removing it failed).
    pub outcome: Option<String>,
    /// The import it came from, when an import that failed for good or was
    /// canceled lists it. Its staging area keeps the file this is a copy of.
    pub import: Option<GcOriginView>,
}

/// The ended import an orphan came from.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct GcOriginView {
    /// The import (`/api/v1/imports/{id}`).
    pub import_id: String,
    /// `failed_terminal` or `canceled`.
    pub state: String,
    /// Its failure code, when it failed.
    pub error_code: Option<String>,
    /// What it brought in, as its source named it.
    pub name: Option<String>,
    /// When it ended.
    pub ended_at: String,
}

/// A kept plan and its largest candidates.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct GcPlanDetail {
    /// The plan.
    pub plan: StoredGcPlanView,
    /// Its candidates, largest first, as many as asked for; `plan.candidates`
    /// counts them all.
    pub objects: Vec<GcPlanObjectView>,
}

/// Plans kept, newest first.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct GcPlanList {
    /// The plans.
    pub items: Vec<StoredGcPlanView>,
}

/// How many plans to list.
#[derive(Debug, Clone, Default, Deserialize, IntoParams)]
pub struct GcPlanListQuery {
    /// Clamped to 1 to 100, 20 unless given.
    pub limit: Option<i64>,
}

/// How many candidates to name.
#[derive(Debug, Clone, Default, Deserialize, IntoParams)]
pub struct GcPlanObjectsQuery {
    /// Largest first, clamped to 1 to 1000, 100 unless given.
    pub limit: Option<usize>,
}

fn rfc3339(at: OffsetDateTime) -> String {
    at.format(&Rfc3339).unwrap_or_default()
}

fn tally(count: i64, bytes: i64) -> Tally {
    Tally {
        count: u64::try_from(count).unwrap_or(0),
        bytes: u64::try_from(bytes).unwrap_or(0),
    }
}

fn plan_view(row: &PlanRow, now: OffsetDateTime) -> StoredGcPlanView {
    let lifetime = time::Duration::try_from(PLAN_LIFETIME).unwrap_or(time::Duration::HOUR);
    let expires_at = row.created_at + lifetime;
    let state = if row.collected.is_some() {
        "collected"
    } else if row.collect_requested_at.is_some() {
        "queued"
    } else if !row.certain {
        "uncertain"
    } else if now > expires_at {
        "expired"
    } else {
        "planned"
    };
    StoredGcPlanView {
        id: row.id.to_string(),
        planned_at: rfc3339(row.created_at),
        planned_by: row.planned_by.clone(),
        grace_seconds: row.grace_seconds,
        certain: row.certain,
        candidates: tally(row.candidate_count, row.candidate_bytes),
        stale_partial_files: row.stale_partial_files,
        state: state.to_owned(),
        expires_at: rfc3339(expires_at),
        collect_requested_at: row.collect_requested_at.map(rfc3339),
        collect_requested_by: row.collect_requested_by.clone(),
        collected: row.collected.map(|done| GcCollectionView {
            collected_at: rfc3339(done.at),
            removed: tally(done.removed_count, done.removed_bytes),
            kept: tally(done.kept_count, done.kept_bytes),
            partial_files_removed: done.partial_files_removed,
        }),
    }
}

fn object_view(row: PlanObjectRow) -> GcPlanObjectView {
    GcPlanObjectView {
        sha256: row.sha256,
        size_bytes: row.size_bytes,
        modified_at: rfc3339(row.modified_at),
        recorded: row.recorded,
        outcome: row.outcome,
        import: row.import.map(|origin| GcOriginView {
            import_id: origin.id.to_string(),
            state: origin.state,
            error_code: origin.error_code,
            name: origin.name,
            ended_at: rfc3339(origin.ended_at),
        }),
    }
}

fn unavailable(error: &dyn std::fmt::Debug) -> Problem {
    tracing::error!(error = ?error, "a collection plan could not be read or kept");
    Problem::new(
        ErrorCode::StorageUnavailable,
        "the store or the catalogue could not be read",
    )
}

fn parse_plan(raw: &str) -> Result<uuid::Uuid, Problem> {
    raw.parse::<uuid::Uuid>()
        .map_err(|_| Problem::invalid_parameter("plan_id", "not a valid identifier"))
}

async fn detail(state: &ApiState, id: uuid::Uuid, keep: usize) -> Result<GcPlanDetail, Problem> {
    let pool = state.database().pool();
    let row = gc_plans::get_plan(pool, id)
        .await
        .map_err(|error| unavailable(&error))?
        .ok_or_else(|| Problem::not_found("collection plan", &id.to_string()))?;
    let objects = gc_plans::plan_objects(pool, id, i64::try_from(keep).unwrap_or(i64::MAX))
        .await
        .map_err(|error| unavailable(&error))?;
    Ok(GcPlanDetail {
        plan: plan_view(&row, OffsetDateTime::now_utc()),
        objects: objects.into_iter().map(object_view).collect(),
    })
}

/// Make a plan and keep it, to be collected.
///
/// # Errors
///
/// `INVALID_PARAMETER` for a grace period out of range;
/// `STORAGE_UNAVAILABLE` when the store or catalogue cannot be read.
#[utoipa::path(
    post,
    path = "/api/v1/storage/gc-plans",
    tag = "library",
    description = "Plan garbage collection and keep the plan, with every candidate, so that \
                   collecting it removes at most what it lists (ADR 0024). The candidates are \
                   the orphaned objects nothing refers to, older than the grace period, each \
                   with the import that failed or was canceled after storing it, when one \
                   did; that import's staging area keeps the file it is a copy of. Nothing \
                   is removed until the plan is collected, within the hour. Reads every \
                   directory in the store. Needs an administrator.",
    request_body = NewGcPlan,
    responses(
        (status = 201, description = "The plan, kept", body = GcPlanDetail),
        (status = 400, description = "A grace period out of range", body = Problem),
        (status = 503, description = "The store or catalogue cannot be read", body = Problem),
    ),
)]
pub async fn create_gc_plan(
    State(state): State<ApiState>,
    SignedIn(user): SignedIn,
    Json(request): Json<NewGcPlan>,
) -> Result<(StatusCode, Json<GcPlanDetail>), Problem> {
    let grace_days = request.grace_days.unwrap_or(DEFAULT_GRACE_DAYS);
    if !(MIN_GRACE_DAYS..=MAX_GRACE_DAYS).contains(&grace_days) {
        return Err(Problem::invalid_parameter(
            "grace_days",
            "must be from 1 to 3650",
        ));
    }
    let manifests = state.manifests().ok_or_else(|| {
        Problem::new(
            ErrorCode::StorageUnavailable,
            "this server has no library configured",
        )
    })?;
    let id = make_plan(
        state.database(),
        manifests,
        grace_days,
        Some(&user.username),
    )
    .await
    .map_err(|error| unavailable(&error))?;
    tracing::info!(plan_id = %id, by = %user.username, "collection plan kept");
    Ok((
        StatusCode::CREATED,
        Json(detail(&state, id, DEFAULT_NAMED).await?),
    ))
}

/// The plans kept, newest first.
///
/// # Errors
///
/// `STORAGE_UNAVAILABLE` when the catalogue cannot be read.
#[utoipa::path(
    get,
    path = "/api/v1/storage/gc-plans",
    tag = "library",
    description = "The collection plans kept, newest first: those made by an administrator \
                   and by the schedule, and what collecting each did. Needs an administrator.",
    params(GcPlanListQuery),
    responses(
        (status = 200, description = "The plans", body = GcPlanList),
        (status = 503, description = "The catalogue cannot be read", body = Problem),
    ),
)]
pub async fn list_gc_plans(
    State(state): State<ApiState>,
    Query(query): Query<GcPlanListQuery>,
) -> Result<Json<GcPlanList>, Problem> {
    let limit = query.limit.unwrap_or(DEFAULT_PLANS).clamp(1, MAX_PLANS);
    let rows = gc_plans::recent_plans(state.database().pool(), limit)
        .await
        .map_err(|error| unavailable(&error))?;
    let now = OffsetDateTime::now_utc();
    Ok(Json(GcPlanList {
        items: rows.iter().map(|row| plan_view(row, now)).collect(),
    }))
}

/// One kept plan and its largest candidates.
///
/// # Errors
///
/// `NOT_FOUND` for no such plan; `STORAGE_UNAVAILABLE` when the catalogue
/// cannot be read.
#[utoipa::path(
    get,
    path = "/api/v1/storage/gc-plans/{plan_id}",
    tag = "library",
    description = "A kept collection plan, with its largest candidates, where each came from \
                   and, once collected, what became of each. Needs an administrator.",
    params(
        ("plan_id" = String, Path, description = "Plan identifier"),
        GcPlanObjectsQuery,
    ),
    responses(
        (status = 200, description = "The plan", body = GcPlanDetail),
        (status = 404, description = "No such plan", body = Problem),
        (status = 503, description = "The catalogue cannot be read", body = Problem),
    ),
)]
pub async fn get_gc_plan(
    State(state): State<ApiState>,
    Path(plan_id): Path<String>,
    Query(query): Query<GcPlanObjectsQuery>,
) -> Result<Json<GcPlanDetail>, Problem> {
    let id = parse_plan(&plan_id)?;
    let keep = query.limit.unwrap_or(DEFAULT_NAMED).clamp(1, MAX_NAMED);
    Ok(Json(detail(&state, id, keep).await?))
}

/// Ask for a kept plan to be collected.
///
/// # Errors
///
/// `NOT_FOUND` for no such plan; `GC_PLAN_UNCERTAIN`, `GC_PLAN_EXPIRED` or
/// `GC_PLAN_COLLECTED` when it cannot be collected; `STORAGE_UNAVAILABLE`
/// when the catalogue cannot be read.
#[utoipa::path(
    post,
    path = "/api/v1/storage/gc-plans/{plan_id}/collect",
    tag = "library",
    description = "Collect a kept plan: remove its candidates that nothing refers to still and \
                   whose file is the one planned, and the stale partial files in the object \
                   tree (ADR 0024). Nothing the plan does not list is removed. The server \
                   collects it shortly, holding back new imports for a moment, or once the \
                   imports and derivations under way end; read the plan for what it did. \
                   Refused for a plan that could not read every manifest and checkpoint, one \
                   more than an hour old, or one asked for already. Cannot be undone; a \
                   failed import keeps its staging area, so it can still be retried. Needs \
                   an administrator, and is audited.",
    params(("plan_id" = String, Path, description = "Plan identifier")),
    responses(
        (status = 202, description = "Queued for collection", body = StoredGcPlanView),
        (status = 404, description = "No such plan", body = Problem),
        (status = 409, description = "Uncertain, expired, or asked for already", body = Problem),
        (status = 503, description = "The catalogue cannot be read", body = Problem),
    ),
)]
pub async fn collect_gc_plan(
    State(state): State<ApiState>,
    SignedIn(user): SignedIn,
    Path(plan_id): Path<String>,
) -> Result<(StatusCode, Json<StoredGcPlanView>), Problem> {
    let id = parse_plan(&plan_id)?;
    let lifetime = time::Duration::try_from(PLAN_LIFETIME).unwrap_or(time::Duration::HOUR);
    let queued = gc_plans::request_collection(
        state.database().pool(),
        id,
        Some(&user.username),
        lifetime,
        i64::from(MIN_GRACE_DAYS) * 86_400,
    )
    .await
    .map_err(|error| unavailable(&error))?;
    let row = queued.map_err(|refusal| match refusal {
        Refusal::NotFound => Problem::not_found("collection plan", &plan_id),
        Refusal::Uncertain => Problem::new(
            ErrorCode::GcPlanUncertain,
            "this plan could not read every manifest and import checkpoint, so something \
             unread may refer to a candidate; it cannot be collected",
        ),
        Refusal::Expired => Problem::new(
            ErrorCode::GcPlanExpired,
            "this plan is more than an hour old; make another",
        ),
        Refusal::AlreadyRequested => Problem::new(
            ErrorCode::GcPlanCollected,
            "collecting this plan was asked for already",
        ),
        Refusal::GraceTooShort => Problem::new(
            ErrorCode::ValidationFailed,
            "this plan's grace period is under a day; make another",
        ),
    })?;
    tracing::warn!(plan_id = %id, by = %user.username, "orphan collection asked for");
    state.deletion_nudge().notify_one();
    Ok((
        StatusCode::ACCEPTED,
        Json(plan_view(&row, OffsetDateTime::now_utc())),
    ))
}

/// The storage routes.
pub fn router() -> Router<ApiState> {
    Router::new()
        .route("/storage/gc-plan", get(gc_plan))
        .route("/storage/gc-plans", post(create_gc_plan).get(list_gc_plans))
        .route("/storage/gc-plans/{plan_id}", get(get_gc_plan))
        .route("/storage/gc-plans/{plan_id}/collect", post(collect_gc_plan))
}
