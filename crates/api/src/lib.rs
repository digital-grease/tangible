// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! HTTP surface: routes, DTOs, and the generated OpenAPI document.
//!
//! Resource endpoints land under `/api/v1` from epic E4 onwards. The scaffold
//! wires the router, the operational probes, and OpenAPI generation so that
//! the generation step is verifiable in CI before there is anything to
//! generate.

pub mod health;
pub mod import;
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
pub use state::ApiState;
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
        routes::workers::consume_enrollment_token,
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
        routes::workers::EnrollmentRequest,
        routes::workers::EnrollmentResponse,
        routes::workers::HeartbeatResponse,
        routes::workers::ClaimRequest,
        routes::workers::ClaimResponse,
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
        routes::workers::RecoveryRequest,
        routes::workers::RecoveryResponse,
    )),
    modifiers(&WorkerSecurity),
    tags(
        (name = "operations", description = "Liveness and readiness probes"),
        (name = "library", description = "Artifacts and their components"),
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
