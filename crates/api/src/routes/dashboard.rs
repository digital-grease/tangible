// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

// See the note in `pagination`: `Problem` is a response document, not a
// hot-path error.
#![allow(clippy::result_large_err)]

//! The dashboard's numbers (UX.md 3, docs/plans/dashboard.md): what needs
//! doing and what is under way, for anyone signed in, in one request.

use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use serde::Serialize;
use tangible_db::dashboard::{BurnSummary, summary};
use time::OffsetDateTime;
use utoipa::ToSchema;

use crate::problem::{ErrorCode, Problem};
use crate::state::ApiState;

/// A worker is silent once not heard from for this long, as diagnostics
/// says; three heartbeats.
const SILENT_AFTER_SECONDS: i64 = 60;

/// An import that failed longer ago than this is no longer news.
const FAILED_WITHIN_DAYS: i64 = 7;

fn rfc3339(value: OffsetDateTime) -> String {
    value
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
}

/// Imports.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct DashboardImports {
    /// Under way.
    pub active: i64,
    /// Failed in the last 7 days.
    pub failed: i64,
}

/// Burn jobs.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct DashboardBurns {
    /// Waiting for a worker.
    pub queued: i64,
    /// Waiting for a disc in the drive.
    pub waiting_for_media: i64,
    /// Held by a worker, staging to verifying.
    pub active: i64,
    /// Needing somebody to look.
    pub needing_attention: i64,
}

/// Workers; revoked ones are not counted.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct DashboardWorkers {
    /// Taking work and heard from in the last minute.
    pub online: i64,
    /// Meant to be taking work, and not heard from in the last minute.
    pub silent: i64,
    /// Finishing what they hold, taking nothing new.
    pub draining: i64,
    /// Enrolled, never heard from.
    pub pending: i64,
    /// Speaking a protocol this server does not.
    pub incompatible: i64,
}

/// The library's integrity.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct DashboardIntegrity {
    /// Artifacts a check found damaged.
    pub damaged: i64,
    /// Artifacts no check has covered yet.
    pub unchecked: i64,
}

/// Room for the library.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct DashboardStorage {
    /// Objects stored.
    pub objects: i64,
    /// Their bytes.
    pub stored_bytes: i64,
    /// Bytes free on the library's volume, when it could be asked.
    pub free_bytes: Option<u64>,
    /// The volume's size, likewise.
    pub total_bytes: Option<u64>,
}

/// A burn, as the dashboard lists it.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct DashboardBurn {
    /// The burn job.
    pub burn_job_id: String,
    /// Its disc.
    pub disc_id: String,
    /// The disc's title.
    pub title: String,
    /// Which disc of its set.
    pub disc_number: i16,
    /// How the attempt ended, or the job's state.
    pub state: String,
    /// The failure's code, when there was one.
    pub error_code: Option<String>,
    /// When it ended, or last changed.
    pub at: String,
}

impl From<BurnSummary> for DashboardBurn {
    fn from(burn: BurnSummary) -> Self {
        Self {
            burn_job_id: burn.burn_job_id.to_string(),
            disc_id: burn.disc_id.to_string(),
            title: burn.title,
            disc_number: burn.disc_number,
            state: burn.state,
            error_code: burn.error_code,
            at: rfc3339(burn.at),
        }
    }
}

/// What needs doing and what is under way.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct DashboardView {
    /// Imports.
    pub imports: DashboardImports,
    /// Burn jobs.
    pub burns: DashboardBurns,
    /// Workers.
    pub workers: DashboardWorkers,
    /// The library's integrity.
    pub integrity: DashboardIntegrity,
    /// Room for the library.
    pub storage: DashboardStorage,
    /// The 5 latest ended burn attempts, newest first.
    pub recent_burns: Vec<DashboardBurn>,
    /// The 5 latest burn jobs needing attention, newest first.
    pub attention: Vec<DashboardBurn>,
}

/// The dashboard.
///
/// # Errors
///
/// `STORAGE_UNAVAILABLE` if the catalogue cannot be read.
#[utoipa::path(
    get,
    path = "/api/v1/dashboard",
    tag = "operations",
    description = "What needs doing and what is under way: imports, burns, workers, \
                   integrity, storage, the latest burns and those needing attention.",
    responses((status = 200, description = "The dashboard", body = DashboardView)),
)]
pub async fn dashboard(State(state): State<ApiState>) -> Result<Json<DashboardView>, Problem> {
    let record = summary(
        state.database().pool(),
        SILENT_AFTER_SECONDS,
        OffsetDateTime::now_utc() - time::Duration::days(FAILED_WITHIN_DAYS),
    )
    .await
    .map_err(|error| {
        tracing::error!(error = ?error, "could not read the dashboard");
        Problem::new(
            ErrorCode::StorageUnavailable,
            "the dashboard could not be read",
        )
    })?;
    let space = state
        .diagnostics()
        .and_then(|settings| tangible_storage::space::space_of(&settings.storage_root));
    Ok(Json(DashboardView {
        imports: DashboardImports {
            active: record.imports_active,
            failed: record.imports_failed,
        },
        burns: DashboardBurns {
            queued: record.burns_queued,
            waiting_for_media: record.burns_waiting_for_media,
            active: record.burns_active,
            needing_attention: record.burns_needing_attention,
        },
        workers: DashboardWorkers {
            online: record.workers_online,
            silent: record.workers_silent,
            draining: record.workers_draining,
            pending: record.workers_pending,
            incompatible: record.workers_incompatible,
        },
        integrity: DashboardIntegrity {
            damaged: record.artifacts_damaged,
            unchecked: record.artifacts_unchecked,
        },
        storage: DashboardStorage {
            objects: record.objects,
            stored_bytes: record.stored_bytes,
            free_bytes: space.map(|space| space.free_bytes),
            total_bytes: space.map(|space| space.total_bytes),
        },
        recent_burns: record.recent_burns.into_iter().map(Into::into).collect(),
        attention: record.attention.into_iter().map(Into::into).collect(),
    }))
}

/// The dashboard route.
pub fn router() -> Router<ApiState> {
    Router::new().route("/dashboard", get(dashboard))
}
