// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Issue report settings.
//!
//! The web UI asks here whether to offer "report this" at all, where an
//! issue form lives, and which build a report should name. Reports
//! themselves are built from scrubbed text the server prepares and opened by
//! the operator's browser; the server never sends one.

use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use serde::Serialize;
use utoipa::ToSchema;

use crate::state::ApiState;

/// Whether this server offers issue reports, and where they go.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ReportSettingsView {
    /// Whether to offer reports at all. Off when the operator has set no
    /// issue address.
    pub enabled: bool,
    /// The issue form's address, such as
    /// `https://github.com/digital-grease/tangible/issues/new`; absent when
    /// reports are off. A report adds `?template=…` and its fields.
    pub issues_url: Option<String>,
    /// The version compiled into the server, such as `0.1.0`.
    pub version: String,
    /// The release tag the image was built from, absent for a local build.
    pub release: Option<String>,
    /// The commit the image was built from, absent for a local build.
    pub revision: Option<String>,
    /// How a report names this build, such as `v0.1.0 (76fa515)`.
    pub build: String,
}

/// Whether this server offers issue reports, and where they go.
#[utoipa::path(
    get,
    path = "/api/v1/report-settings",
    tag = "reports",
    responses((status = 200, description = "How issue reports are configured", body = ReportSettingsView)),
)]
pub async fn report_settings(State(state): State<ApiState>) -> Json<ReportSettingsView> {
    let reports = state.reports();
    let build = reports.build();
    Json(ReportSettingsView {
        enabled: reports.enabled(),
        issues_url: reports.issues_url().map(str::to_owned),
        version: build.version.clone(),
        release: build.release.clone(),
        revision: build.revision.clone(),
        build: build.describe(),
    })
}

/// The report settings route.
pub fn router() -> Router<ApiState> {
    Router::new().route("/report-settings", get(report_settings))
}
