// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! HTTP surface: routes, DTOs, and the generated OpenAPI document.
//!
//! Resource endpoints land under `/api/v1` from epic E4 onwards. The scaffold
//! wires the router, the operational probes, and OpenAPI generation so that
//! the generation step is verifiable in CI before there is anything to
//! generate.

pub mod auth;
pub mod catalog;
pub mod health;
pub mod import;
pub mod import_runner;
pub mod pagination;
pub mod problem;
pub mod romm_export;
pub mod routes;
pub mod state;
pub mod web;
pub mod worker_auth;

use axum::Router;
use axum::routing::get;
use tower_http::trace::TraceLayer;
use utoipa::OpenApi;

pub use auth::{AuthSettings, CurrentUser, SignedIn};
pub use import::{ImportCheckpoint, ImportError, ImportOutcome, ImportPipeline, ImportRequest};
pub use import_runner::{ImportRunner, ImportRunnerSettings};
pub use romm_export::{RommExport, RommExporter};
pub use state::{ApiState, ImportContext};
pub use web::{WebUi, WebUiError};
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
        routes::accounts::setup_status,
        routes::accounts::complete_setup,
        routes::accounts::sign_in,
        routes::accounts::current_session,
        routes::accounts::sign_out,
        routes::accounts::list_accounts,
        routes::accounts::create_account,
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
        routes::romm::romm_settings,
        routes::romm::get_edition_romm,
        routes::romm::set_edition_romm_route,
        routes::erasures::list_all_drives,
        routes::erasures::request_drive_erasure,
        routes::erasures::list_recent_erasures,
        routes::erasures::get_one_erasure,
        routes::erasures::cancel_one_erasure,
        routes::erasures::claim_drive_erasure,
        routes::erasures::complete_drive_erasure,
        routes::workers::issue_enrollment_token,
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
        routes::accounts::SetupStatus,
        routes::accounts::UserView,
        routes::accounts::UserList,
        routes::accounts::SessionView,
        routes::accounts::Credentials,
        routes::accounts::CreateUserRequest,
        routes::artifacts::ArtifactSummaryPage,
        routes::artifacts::ArtifactSummary,
        routes::artifacts::ArtifactDetail,
        routes::artifacts::ComponentView,
        routes::artifacts::DiscLayout,
        routes::artifacts::TrackView,
        routes::artifacts::TrackIndexView,
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
        routes::romm::PlatformView,
        routes::romm::RommSettings,
        routes::romm::EditionRommView,
        routes::romm::SetEditionRomm,
        routes::erasures::DriveView,
        routes::erasures::DriveList,
        routes::erasures::ErasureView,
        routes::erasures::ErasureList,
        routes::erasures::ErasureMedium,
        routes::erasures::RequestErasure,
        routes::erasures::ErasureClaimRequest,
        routes::erasures::ErasureClaim,
        routes::erasures::ErasureCompletion,
        routes::workers::TrackVerificationReport,
        routes::workers::IssueEnrollmentRequest,
        routes::workers::IssuedEnrollment,
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
    modifiers(&WorkerSecurity, &SessionSecurity),
    tags(
        (name = "operations", description = "Liveness and readiness probes"),
        (name = "accounts", description = "Setup, signing in, and accounts"),
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

/// Declare the session cookie, and mark every operation that needs it.
///
/// Derived from [`auth::ACCESS_RULES`] rather than annotated route by route,
/// so the document cannot disagree with what the server enforces.
struct SessionSecurity;

impl utoipa::Modify for SessionSecurity {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        use utoipa::openapi::security::{ApiKey, ApiKeyValue, SecurityRequirement, SecurityScheme};

        let components = openapi.components.get_or_insert_with(Default::default);
        components.add_security_scheme(
            "session_cookie",
            SecurityScheme::ApiKey(ApiKey::Cookie(ApiKeyValue::with_description(
                auth::SESSION_COOKIE,
                "Set by signing in. Mutations must also send the session's \
                 anti-forgery token in X-CSRF-Token.",
            ))),
        );

        for (method, path, access) in auth::ACCESS_RULES {
            let requirements = match access {
                auth::Access::Signed(_) => vec![SecurityRequirement::new(
                    "session_cookie",
                    Vec::<String>::new(),
                )],
                // Either will do: alternatives are separate requirements.
                auth::Access::SignedOrLeaseholder(_) => vec![
                    SecurityRequirement::new("session_cookie", Vec::<String>::new()),
                    SecurityRequirement::new("worker_credential", Vec::<String>::new()),
                ],
                auth::Access::Public | auth::Access::Worker => continue,
            };
            let Some(item) = openapi.paths.paths.get_mut(*path) else {
                continue;
            };
            let operation = match *method {
                "GET" => item.get.as_mut(),
                "POST" => item.post.as_mut(),
                "PUT" => item.put.as_mut(),
                "PATCH" => item.patch.as_mut(),
                "DELETE" => item.delete.as_mut(),
                _ => None,
            };
            if let Some(operation) = operation {
                operation.security = Some(requirements);
            }
        }
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
    let router = Router::new()
        .route("/livez", get(health::livez))
        .route("/readyz", get(health::readyz))
        .nest(API_BASE, routes::accounts::router())
        .nest(API_BASE, routes::artifacts::router())
        .nest(API_BASE, routes::burns::router())
        .nest(API_BASE, routes::imports::router())
        .nest(API_BASE, routes::physical_copies::router())
        .nest(API_BASE, routes::catalog::router())
        .nest(API_BASE, routes::workers::router())
        .nest(API_BASE, routes::erasures::router())
        .nest(API_BASE, routes::romm::router())
        // Applied per route, after routing, so the rule is looked up by the
        // matched pattern rather than by a raw path a caller could disguise.
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            auth::authorize,
        ))
        // Everything else is the web UI, or a JSON 404 under /api.
        .fallback(web::fallback)
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            auth::security_headers,
        ));
    traced(router).with_state(state)
}

/// Request and response headers that carry a secret: the session cookie, a
/// worker's bearer credential, the anti-forgery token.
fn sensitive_headers() -> [axum::http::HeaderName; 4] {
    use axum::http::header::{AUTHORIZATION, COOKIE, SET_COOKIE};
    [
        AUTHORIZATION,
        COOKIE,
        SET_COOKIE,
        axum::http::HeaderName::from_static(auth::CSRF_HEADER),
    ]
}

/// Mark the secret-bearing headers of a request as sensitive, so they print
/// as `Sensitive` wherever a header map is formatted.
async fn mark_sensitive_headers(
    mut request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    mark_sensitive(request.headers_mut());
    let mut response = next.run(request).await;
    mark_sensitive(response.headers_mut());
    response
}

fn mark_sensitive(headers: &mut axum::http::HeaderMap) {
    for name in sensitive_headers() {
        if let axum::http::header::Entry::Occupied(mut entry) = headers.entry(name) {
            for value in entry.iter_mut() {
                value.set_sensitive(true);
            }
        }
    }
}

/// Request tracing, with secret headers marked before the trace sees them.
///
/// The default trace does not record headers. Marking them anyway means a
/// later `include_headers(true)` prints `Sensitive` rather than every cookie
/// and bearer token. Marked on both sides of the trace: outside it for the
/// request on the way in, inside it for the response on the way out.
fn traced<S: Clone + Send + Sync + 'static>(router: Router<S>) -> Router<S> {
    router
        .layer(axum::middleware::from_fn(mark_sensitive_headers))
        .layer(TraceLayer::new_for_http())
        .layer(axum::middleware::from_fn(mark_sensitive_headers))
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn secret_headers_are_marked_sensitive_around_the_trace() {
        use axum::http::header::{AUTHORIZATION, COOKIE, SET_COOKIE};
        use tower::ServiceExt as _;

        async fn probe(headers: axum::http::HeaderMap) -> axum::response::Response {
            for name in [AUTHORIZATION, COOKIE] {
                assert!(
                    headers[&name].is_sensitive(),
                    "{name} reached the handler unmarked"
                );
            }
            assert!(headers[auth::CSRF_HEADER].is_sensitive());
            assert!(
                !headers["accept"].is_sensitive(),
                "only secret headers are marked"
            );
            axum::response::Response::builder()
                .header(SET_COOKIE, "tangible_session=placeholder")
                .body(axum::body::Body::empty())
                .unwrap()
        }

        let app = traced(Router::new().route("/", get(probe)));
        let request = axum::http::Request::builder()
            .uri("/")
            .header(AUTHORIZATION, "Bearer placeholder")
            .header(COOKIE, "tangible_session=placeholder")
            .header(auth::CSRF_HEADER, "placeholder")
            .header("accept", "application/json")
            .body(axum::body::Body::empty())
            .unwrap();
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), 200);
        assert!(response.headers()[SET_COOKIE].is_sensitive());
        assert_eq!(format!("{:?}", response.headers()[SET_COOKIE]), "Sensitive");
    }

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
    fn no_schema_is_silently_replaced_by_another_of_the_same_name() {
        // utoipa keys schemas by type name, so two structs called the same in
        // different modules leave one of them out of the document without a
        // word. This happened once, to the catalog's DiscView.
        let document = serde_json::to_value(ApiDoc::openapi()).expect("document");
        let schemas = &document["components"]["schemas"];
        assert!(schemas["DiscView"]["properties"]["disc_set_id"].is_object());
        assert!(schemas["DiscLayout"]["properties"]["tracks"].is_object());

        let source = [
            include_str!("routes/accounts.rs"),
            include_str!("routes/artifacts.rs"),
            include_str!("routes/burns.rs"),
            include_str!("routes/catalog.rs"),
            include_str!("routes/erasures.rs"),
            include_str!("routes/imports.rs"),
            include_str!("routes/physical_copies.rs"),
            include_str!("routes/romm.rs"),
            include_str!("routes/workers.rs"),
            include_str!("health.rs"),
            include_str!("problem.rs"),
        ];
        let mut seen = std::collections::BTreeSet::new();
        for file in source {
            for line in file.lines() {
                if let Some(rest) = line.strip_prefix("pub struct ") {
                    let name: String = rest
                        .chars()
                        .take_while(|c| c.is_alphanumeric() || *c == '_')
                        .collect();
                    assert!(seen.insert(name.clone()), "two structs are called {name}");
                }
            }
        }
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
