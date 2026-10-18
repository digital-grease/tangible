// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

// See the note in `pagination`: `Problem` is a response document, not a
// hot-path error.
#![allow(clippy::result_large_err)]

//! Diagnostics, for administrators: the same checklist `tangible doctor
//! --report` prints, and the same report prepared as a bug report.

use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use serde::Serialize;
use utoipa::ToSchema;

use crate::diagnostics::{self, CheckStatus, DiagnosticSection, Diagnostics};
use crate::problem::{ErrorCode, Problem};
use crate::report::IssueReport;
use crate::state::ApiState;

/// The checklist, and the same as Markdown for copying.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct DiagnosticsView {
    /// When it was collected, RFC 3339.
    pub generated_at: String,
    /// The build, as reports name it: `v0.1.0 (76fa515)`.
    pub build: String,
    /// The worst status of any check.
    pub status: CheckStatus,
    /// The checks, by section.
    pub sections: Vec<DiagnosticSection>,
    /// The whole report as Markdown, exactly as `doctor --report` prints it.
    pub markdown: String,
}

impl From<Diagnostics> for DiagnosticsView {
    fn from(report: Diagnostics) -> Self {
        let markdown = diagnostics::markdown(&report);
        Self {
            generated_at: report.generated_at,
            build: report.build,
            status: report.status,
            sections: report.sections,
            markdown,
        }
    }
}

/// The server's diagnostics settings, or why there are none.
fn run_settings(state: &ApiState) -> Result<&diagnostics::DiagnosticsSettings, Problem> {
    state.diagnostics().ok_or_else(|| {
        Problem::new(
            ErrorCode::StorageUnavailable,
            "diagnostics are not available on this server",
        )
    })
}

/// The deployment's diagnostics, collected now.
///
/// # Errors
///
/// `STORAGE_UNAVAILABLE` on a server started without diagnostics.
#[utoipa::path(
    get,
    path = "/api/v1/diagnostics",
    tag = "diagnostics",
    description = "Diagnostics, collected when asked: database and schema, storage room and \
                   writability, tools, workers and drives, recent failure codes, and settings. \
                   Carries no paths, library file names, or worker and drive names. \
                   Administrators only.",
    responses(
        (status = 200, description = "The checklist", body = DiagnosticsView),
        (status = 503, description = "Diagnostics are not available", body = Problem),
    ),
)]
pub async fn get_diagnostics(
    State(state): State<ApiState>,
) -> Result<Json<DiagnosticsView>, Problem> {
    let settings = run_settings(&state)?;
    let report = diagnostics::run(state.database(), settings).await;
    Ok(Json(report.into()))
}

/// The diagnostics, prepared as a bug report about the deployment for the
/// operator to review.
///
/// # Errors
///
/// `CONFLICT` when reports are turned off; `STORAGE_UNAVAILABLE` on a server
/// started without diagnostics.
#[utoipa::path(
    get,
    path = "/api/v1/diagnostics/report",
    tag = "diagnostics",
    responses(
        (status = 200, description = "The report, scrubbed, for review", body = IssueReport),
        (status = 409, description = "Reports are off", body = Problem),
        (status = 503, description = "Diagnostics are not available", body = Problem),
    ),
)]
pub async fn diagnostics_report(
    State(state): State<ApiState>,
) -> Result<Json<IssueReport>, Problem> {
    let reports = state.reports();
    if !reports.enabled() {
        return Err(Problem::new(
            ErrorCode::Conflict,
            "issue reports are turned off on this server (TANGIBLE_REPORT_ISSUES_URL is empty)",
        ));
    }
    let settings = run_settings(&state)?;
    let report = diagnostics::run(state.database(), settings).await;
    Ok(Json(diagnostics::issue_report(
        &report,
        reports.build(),
        reports.scrub_context(),
    )))
}

/// The diagnostics routes.
pub fn router() -> Router<ApiState> {
    Router::new()
        .route("/diagnostics", get(get_diagnostics))
        .route("/diagnostics/report", get(diagnostics_report))
}
