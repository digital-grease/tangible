// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

// See the note in `pagination`: `Problem` is a response document, not a
// hot-path error.
#![allow(clippy::result_large_err)]

//! Issue reports.
//!
//! The web UI asks here whether to offer "report this" at all, where an
//! issue form lives, which build a report should name, and for the report
//! itself, scrubbed and in the exact form the template expects. The
//! operator's browser opens the issue form after they have read it; the
//! server never sends a report anywhere.

use axum::extract::{Path, Query, State};
use axum::routing::get;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use tangible_db::erasures::get_drive;
use tangible_db::repositories::{
    artifact_burn_facts, get_burn_attempt, get_burn_job, list_burn_events,
};
use tangible_domain::manifest::{ArtifactManifest, Topology};
use tangible_domain::{ArtifactId, BurnAttemptId};
use utoipa::{IntoParams, ToSchema};

use crate::burn_report::{
    BurnFacts, BurnReportKind, BurnReportRefusal, DriveFacts, EventLine, burn_report,
};
use crate::problem::{ErrorCode, Problem};
use crate::report::IssueReport;
use crate::routes::burns::inspected_medium_info;
use crate::state::ApiState;

/// The most events a report reads. The log keeps the end of them.
const MAX_REPORT_EVENTS: i64 = 10_000;

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

/// Which report to prepare for a burn attempt.
#[derive(Debug, Clone, Default, Deserialize, IntoParams)]
pub struct BurnReportQuery {
    /// `problem` (the default) fills in the burn form for any attempt;
    /// `worked` fills in the hardware form for a disc that was written or
    /// verified.
    pub kind: Option<String>,
}

fn unavailable(what: &str) -> Problem {
    Problem::new(
        ErrorCode::StorageUnavailable,
        format!("{what}; see server logs"),
    )
}

/// A burn attempt, prepared as an issue report for the operator to review.
///
/// # Errors
///
/// `CONFLICT` when reports are turned off, or when a burn that produced no
/// disc, or ran on the fake engine, is asked for as `worked`;
/// `NOT_FOUND` for an unknown attempt; `INVALID_PARAMETER` for an unknown
/// kind.
#[utoipa::path(
    get,
    path = "/api/v1/burn-attempts/{attempt_id}/report",
    tag = "reports",
    params(
        ("attempt_id" = String, Path, description = "Burn attempt identifier"),
        BurnReportQuery,
    ),
    responses(
        (status = 200, description = "The report, scrubbed, for review", body = IssueReport),
        (status = 404, description = "No such attempt", body = Problem),
        (status = 409, description = "Reports are off, or that kind does not fit this attempt", body = Problem),
    ),
)]
pub async fn burn_attempt_report(
    State(state): State<ApiState>,
    Path(attempt_id): Path<String>,
    Query(query): Query<BurnReportQuery>,
) -> Result<Json<IssueReport>, Problem> {
    let reports = state.reports();
    if !reports.enabled() {
        return Err(Problem::new(
            ErrorCode::Conflict,
            "issue reports are turned off on this server (TANGIBLE_REPORT_ISSUES_URL is empty)",
        ));
    }
    let kind = match query.kind.as_deref() {
        None | Some("problem") => BurnReportKind::Problem,
        Some("worked") => BurnReportKind::Worked,
        Some(_) => {
            return Err(Problem::invalid_parameter(
                "kind",
                "must be problem or worked",
            ));
        }
    };
    let id = attempt_id
        .parse::<BurnAttemptId>()
        .map_err(|_| Problem::invalid_parameter("attempt_id", "not a valid identifier"))?;
    let facts = gather(&state, id).await?;
    let report = burn_report(&facts, kind, reports.scrub_context()).map_err(
        |refusal: BurnReportRefusal| Problem::new(ErrorCode::Conflict, refusal.to_string()),
    )?;
    Ok(Json(report))
}

/// Everything a burn report is written from, read from the database and
/// the library.
async fn gather(state: &ApiState, id: BurnAttemptId) -> Result<BurnFacts, Problem> {
    let pool = state.database().pool();
    let attempt = get_burn_attempt(pool, id)
        .await
        .map_err(|error| {
            tracing::error!(error = ?error, "could not read a burn attempt");
            unavailable("the burn attempt could not be read")
        })?
        .ok_or_else(|| Problem::not_found("burn attempt", &id.to_string()))?;
    let job = get_burn_job(pool, attempt.burn_job_id)
        .await
        .map_err(|error| {
            tracing::error!(error = ?error, "could not read a burn job");
            unavailable("the burn job could not be read")
        })?
        .ok_or_else(|| unavailable("the attempt's burn job is missing"))?;
    let artifact_id = ArtifactId::from_uuid(job.artifact_id);
    let format = artifact_burn_facts(pool, job.artifact_id)
        .await
        .ok()
        .flatten()
        .map_or_else(|| "unknown".to_owned(), |facts| facts.format);
    let drive = get_drive(pool, attempt.drive_id).await.ok().flatten();
    let events = list_burn_events(pool, id, None, MAX_REPORT_EVENTS)
        .await
        .map_err(|error| {
            tracing::error!(error = ?error, "could not read burn events");
            unavailable("the burn's events could not be read")
        })?;
    let medium = inspected_medium_info(state, id).await;

    let mut names = Vec::new();
    if let Some(manifests) = state.manifests()
        && let Ok(manifest) = manifests.read(artifact_id).await
    {
        names = library_names(&manifest);
    }
    if let Some(drive) = &drive {
        names.push((drive.worker_name.clone(), "worker".to_owned()));
        names.push((drive.configured_name.clone(), "drive".to_owned()));
    }

    Ok(BurnFacts {
        build: state.reports().build().describe(),
        attempt_number: attempt.attempt_number,
        state: attempt.state,
        engine: attempt.engine.clone(),
        engine_version: attempt.engine_version.clone(),
        error_code: attempt.error_code.clone(),
        error_detail: attempt.error_detail.clone(),
        verify_report: attempt.verify_report.clone(),
        drive: drive
            .as_ref()
            .map_or_else(DriveFacts::default, |drive| DriveFacts {
                vendor: drive.vendor.clone(),
                model: drive.model.clone(),
                firmware: drive.firmware.clone(),
                connection: drive.connection.parse().unwrap_or_default(),
            }),
        medium,
        format,
        events: events
            .into_iter()
            .map(|event| EventLine {
                sequence: event.sequence,
                event_type: event.event_type,
                stage: event.stage,
                code: event.message_code,
                message: event
                    .data
                    .get("message")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned),
                at: event.worker_timestamp,
            })
            .collect(),
        names,
    })
}

/// Names from an artifact's manifest that say what is in someone's library:
/// each component's path, file name, name without its extension and the
/// name it was uploaded as, and the image's volume labels.
fn library_names(manifest: &ArtifactManifest) -> Vec<(String, String)> {
    let mut names = Vec::new();
    for component in &manifest.components {
        let label = format!("component {}", component.ordinal);
        let file = component.logical_path.file_name();
        names.push((component.logical_path.as_str().to_owned(), label.clone()));
        names.push((file.to_owned(), label.clone()));
        if let Some((stem, _)) = file.rsplit_once('.') {
            names.push((stem.to_owned(), label.clone()));
        }
        if let Some(source) = &component.source_filename {
            names.push((source.clone(), label.clone()));
        }
    }
    // What it was called when it came in: often the only human naming an
    // import has, and so the likeliest to say what it is.
    for source in [
        &manifest.origin.source_filename,
        &manifest.origin.source_reference,
    ]
    .into_iter()
    .flatten()
    {
        names.push((source.clone(), "source".to_owned()));
    }
    if let Some(note) = &manifest.origin.operator_note {
        names.push((note.clone(), "note".to_owned()));
    }
    if let Topology::SingleTrackBlockImage { volume_labels, .. } = &manifest.topology {
        for volume in volume_labels {
            names.push((volume.clone(), "volume label".to_owned()));
        }
    }
    names
}

/// The report routes.
pub fn router() -> Router<ApiState> {
    Router::new()
        .route("/report-settings", get(report_settings))
        .route(
            "/burn-attempts/{attempt_id}/report",
            get(burn_attempt_report),
        )
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::*;

    fn names_of(json: &str) -> Vec<String> {
        let manifest = ArtifactManifest::from_json(json).expect("a manifest");
        library_names(&manifest)
            .into_iter()
            .map(|(name, _)| name)
            .collect()
    }

    #[test]
    fn an_iso_gives_its_file_its_upload_name_and_its_volume_label() {
        let names = names_of(include_str!("../../../../fuzz/seeds/manifest/iso.json"));
        for expected in ["disc.iso", "disc", "Game", "EXAMPLE"] {
            assert!(
                names.iter().any(|name| name == expected),
                "{expected} missing from {names:?}"
            );
        }
    }

    #[test]
    fn a_cue_sheet_gives_every_file_it_is_made_of() {
        let names = names_of(include_str!("../../../../fuzz/seeds/manifest/cue-bin.json"));
        for expected in ["game.cue", "game", "Game"] {
            assert!(
                names.iter().any(|name| name == expected),
                "{expected} missing from {names:?}"
            );
        }
    }

    #[test]
    fn the_names_are_kept_out_of_a_scrubbed_log() {
        let manifest =
            ArtifactManifest::from_json(include_str!("../../../../fuzz/seeds/manifest/iso.json"))
                .expect("a manifest");
        let mut context = tangible_domain::report::ScrubContext::new();
        for (name, label) in library_names(&manifest) {
            context = context.name(name, &label);
        }
        let text = tangible_domain::report::scrub(
            "xorriso: could not read disc.iso (volume EXAMPLE), imported as Game",
            &context,
        )
        .text;
        assert!(
            !text.contains("disc.iso") && !text.contains("EXAMPLE"),
            "{text}"
        );
        assert!(!text.contains("Game"), "{text}");
    }
}
