// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! HTTP surface: routes, DTOs, and the generated OpenAPI document.
//!
//! Resource endpoints land under `/api/v1` from epic E4 onwards. The scaffold
//! wires the router, the operational probes, and OpenAPI generation so that
//! the generation step is verifiable in CI before there is anything to
//! generate.

pub mod catalog;
pub mod health;
pub mod import;
pub mod import_runner;
pub mod pagination;
pub mod problem;
pub mod routes;
pub mod state;
pub mod worker_auth;

use axum::Router;
use axum::routing::get;
use tower_http::trace::TraceLayer;
use utoipa::OpenApi;

pub use import::{ImportCheckpoint, ImportError, ImportOutcome, ImportPipeline, ImportRequest};
pub use import_runner::{ImportRunner, ImportRunnerSettings};
pub use state::{ApiState, ImportContext};
pub use worker_auth::{
    AuthRejection, CredentialHash, EnrollmentRejection, EnrollmentToken, Secret, WorkerCredential,
    WorkerIdentity,
};

/// Base path for the versioned product API.
pub const API_BASE: &str = "/api/v1";

/// The generated OpenAPI document.
///
/// The API is specified as OpenAPI 3.1; utoipa emits 3.1 by default.
#[derive(OpenApi)]
#[openapi(
    info(
        title = "Tangible",
        description = "Self-hosted disc-image preservation, management, and burning.",
        license(name = "AGPL-3.0-or-later", identifier = "AGPL-3.0-or-later"),
    ),
    paths(
        health::livez,
        health::readyz,
        routes::artifacts::list_artifacts,
        routes::artifacts::get_artifact,
        routes::artifacts::get_manifest,
        routes::artifacts::list_components,
        routes::artifacts::component_content,
        routes::catalog::list_all_titles,
        routes::catalog::create_new_title,
        routes::catalog::get_one_title,
        routes::catalog::list_title_editions,
        routes::catalog::create_title_edition,
        routes::catalog::list_edition_sets,
        routes::catalog::create_edition_set,
        routes::catalog::list_set_discs,
        routes::catalog::create_set_disc,
        routes::catalog::get_one_disc,
        routes::catalog::list_linked_artifacts,
        routes::catalog::link_artifact,
        routes::imports::list_sources,
        routes::imports::create_import,
        routes::imports::upload_import,
        routes::imports::list_imports,
        routes::imports::get_import,
        routes::imports::cancel_import,
        routes::imports::retry_import,
        routes::physical_copies::list_copies,
        routes::physical_copies::get_copy,
        routes::physical_copies::update_copy,
        routes::physical_copies::record_check,
        routes::physical_copies::mark_destroyed,
        routes::burns::list_jobs,
        routes::burns::create_job,
        routes::burns::get_job,
        routes::burns::list_attempts,
        routes::burns::cancel_job,
        routes::burns::retry_job,
        routes::burns::resolve_attention,
        routes::burns::get_attempt,
        routes::burns::list_events,
        routes::workers::consume_enrollment_token,
        routes::workers::report_capabilities,
        routes::workers::heartbeat,
        routes::workers::claim_work,
        routes::workers::renew,
        routes::workers::submit_events,
        routes::workers::complete,
        routes::workers::recover,
    ),
    components(schemas(
        health::Liveness,
        health::Readiness,
        health::DependencyCheck,
        problem::Problem,
        routes::artifacts::ArtifactSummaryPage,
        routes::artifacts::ArtifactSummary,
        routes::artifacts::ArtifactDetail,
        routes::artifacts::ComponentView,
        routes::catalog::TitleView,
        routes::catalog::TitlePage,
        routes::catalog::EditionView,
        routes::catalog::DiscSetView,
        routes::catalog::DiscView,
        routes::catalog::DiscArtifactView,
        routes::catalog::CreateTitleRequest,
        routes::catalog::CreateEditionRequest,
        routes::catalog::CreateDiscSetRequest,
        routes::catalog::CreateDiscRequest,
        routes::catalog::LinkArtifactRequest,
        routes::imports::ImportView,
        routes::imports::ImportPage,
        routes::imports::ImportSources,
        routes::imports::CreateImportRequest,
        routes::physical_copies::PhysicalCopyView,
        routes::physical_copies::PhysicalCopyPage,
        routes::physical_copies::PhysicalCopyDetail,
        routes::physical_copies::CheckView,
        routes::physical_copies::UpdatePhysicalCopyRequest,
        routes::physical_copies::RecordCheckRequest,
        routes::burns::BurnJobPage,
        routes::burns::BurnJobView,
        routes::burns::BurnJobDetail,
        routes::burns::BurnJobCreated,
        routes::burns::BurnProgress,
        routes::burns::BurnAttemptView,
        routes::burns::BurnAttemptDetail,
        routes::burns::BurnEventPage,
        routes::burns::BurnEventView,
        routes::burns::CreateBurnJobRequest,
        routes::workers::EnrollmentRequest,
        routes::workers::EnrollmentResponse,
        routes::workers::HeartbeatResponse,
        routes::workers::CapabilityReport,
        routes::workers::CapabilityResponse,
        routes::workers::DriveReportBody,
        routes::workers::EngineReport,
        routes::workers::ClaimRequest,
        routes::workers::ClaimResponse,
        routes::workers::ClaimedArtifact,
        routes::workers::RenewRequest,
        routes::workers::RenewResponse,
        routes::workers::EventBatch,
        routes::workers::EventSubmission,
        routes::workers::EventAck,
        routes::workers::CompletionRequest,
        routes::workers::CompletionResponse,
        routes::workers::WriteReport,
        routes::workers::VerificationReport,
        routes::workers::PhysicalMedium,
        routes::workers::FailureReport,
        routes::workers::RecoveryRequest,
        routes::workers::RecoveryResponse,
    )),
    modifiers(&WorkerSecurity),
    tags(
        (name = "operations", description = "Liveness and readiness probes"),
        (name = "library", description = "Artifacts and their components"),
        (name = "catalog", description = "Titles, editions, disc sets, and discs"),
        (name = "imports", description = "Getting bytes into the library"),
        (name = "burns", description = "Burn jobs, attempts, and their events"),
        (name = "physical-copies", description = "The discs that actually exist"),
        (name = "workers", description = "The burn-worker protocol"),
    ),
)]
pub struct ApiDoc;

/// Declare the worker credential scheme in the generated document.
///
/// utoipa cannot infer a security scheme from a `security(...)` reference
/// alone, so it is registered here; without this the routes would advertise a
/// scheme the document never defines, and a generated client would omit the
/// header entirely.
struct WorkerSecurity;

impl utoipa::Modify for WorkerSecurity {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        use utoipa::openapi::security::{Http, HttpAuthScheme, SecurityScheme};

        let components = openapi.components.get_or_insert_with(Default::default);
        components.add_security_scheme(
            "worker_credential",
            SecurityScheme::Http(Http::new(HttpAuthScheme::Bearer)),
        );
    }
}

/// Serialize the OpenAPI document as pretty-printed JSON.
///
/// Used by `tangible openapi` and checked in CI so that a drifting document
/// fails the build rather than shipping.
///
/// # Errors
///
/// Returns an error if the document cannot be serialized.
pub fn openapi_json() -> Result<String, serde_json::Error> {
    serde_json::to_string_pretty(&ApiDoc::openapi())
}

/// Serve the router on an already-bound listener until `shutdown` resolves.
///
/// Owning this here keeps `axum` out of the composition root: the crate
/// dependency direction puts every HTTP concern in `api`.
///
/// # Errors
///
/// Returns an error if the server terminates abnormally.
pub async fn serve<S>(
    listener: tokio::net::TcpListener,
    state: ApiState,
    shutdown: S,
) -> std::io::Result<()>
where
    S: Future<Output = ()> + Send + 'static,
{
    axum::serve(listener, router(state))
        .with_graceful_shutdown(shutdown)
        .await
}

/// Build the application router.
pub fn router(state: ApiState) -> Router {
    Router::new()
        .route("/livez", get(health::livez))
        .route("/readyz", get(health::readyz))
        .nest(API_BASE, routes::artifacts::router())
        .nest(API_BASE, routes::burns::router())
        .nest(API_BASE, routes::imports::router())
        .nest(API_BASE, routes::physical_copies::router())
        .nest(API_BASE, routes::catalog::router())
        .nest(API_BASE, routes::workers::router())
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn openapi_document_generates() {
        let json = openapi_json().expect("document serializes");
        assert!(json.contains("\"/livez\""));
        assert!(json.contains("\"/readyz\""));
    }

    #[test]
    fn openapi_declares_the_project_license() {
        let json = openapi_json().expect("document serializes");
        assert!(json.contains("AGPL-3.0-or-later"));
    }

    #[test]
    fn readiness_advertises_a_503_response() {
        let json = openapi_json().expect("document serializes");
        assert!(
            json.contains("\"503\""),
            "readyz must document its degraded response"
        );
    }
}
