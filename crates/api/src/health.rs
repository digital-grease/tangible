// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Operational probes.
//!
//! These live at the root rather than under `/api/v1` because they are not
//! part of the product API surface: they exist for container orchestration,
//! and their shape is free to change without an API version bump.
//!
//! The distinction matters for Compose. `deploy/compose.yaml` gates the burn
//! workers on `service_healthy` for the server, so the server's healthcheck
//! must report *readiness* (dependencies usable), while a restart policy
//! should watch *liveness* (process responsive).

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use serde::Serialize;
use utoipa::ToSchema;

use crate::state::ApiState;

/// How long a single dependency probe may take before it is called down.
///
/// Deliberately shorter than the container healthcheck timeout in
/// `deploy/compose.yaml`, so the probe always answers rather than being
/// killed mid-flight.
const PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// Result of a liveness probe.
#[derive(Debug, Serialize, ToSchema)]
pub struct Liveness {
    /// Always `"alive"` when the process can serve a request at all.
    pub status: &'static str,
    /// Server version, from the crate version at build time.
    pub version: &'static str,
}

/// Result of a readiness probe.
#[derive(Debug, Serialize, ToSchema)]
pub struct Readiness {
    /// `"ready"` when every dependency is usable, otherwise `"degraded"`.
    pub status: &'static str,
    /// Per-dependency detail, so an operator can see *what* is failing.
    pub checks: Vec<DependencyCheck>,
}

/// The state of one dependency.
#[derive(Debug, Serialize, ToSchema)]
pub struct DependencyCheck {
    /// Dependency name, e.g. `"database"`.
    pub name: &'static str,
    /// `"up"` or `"down"`.
    pub status: &'static str,
    /// Operator-facing failure summary. Never carries a connection string or
    /// credential, only the error class.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// Liveness probe: the process is running and the runtime is responsive.
///
/// Deliberately checks nothing external. A database outage must not cause an
/// orchestrator to kill and restart an otherwise healthy server.
#[utoipa::path(
    get,
    path = "/livez",
    tag = "operations",
    responses((status = 200, description = "Process is alive", body = Liveness)),
)]
pub async fn livez() -> Json<Liveness> {
    Json(Liveness {
        status: "alive",
        version: env!("CARGO_PKG_VERSION"),
    })
}

/// Readiness probe: every dependency needed to serve traffic is usable.
///
/// Returns `503` when any dependency is down so that Compose and load
/// balancers withhold traffic rather than sending it into failure.
#[utoipa::path(
    get,
    path = "/readyz",
    tag = "operations",
    responses(
        (status = 200, description = "All dependencies usable", body = Readiness),
        (status = 503, description = "At least one dependency is down", body = Readiness),
    ),
)]
pub async fn readyz(State(state): State<ApiState>) -> (StatusCode, Json<Readiness>) {
    // Bound the probe independently of the pool's acquire timeout. Without
    // this the handler blocks for the full acquire window when the database
    // is unreachable, which outlives a typical container healthcheck timeout
    // and makes the service look like it is hanging rather than degraded.
    let probe = tokio::time::timeout(PROBE_TIMEOUT, state.database().ping()).await;

    let database = match probe {
        Ok(Ok(())) => DependencyCheck {
            name: "database",
            status: "up",
            detail: None,
        },
        Ok(Err(error)) => {
            // Log the full chain server-side; return only the error class to
            // the caller, since the source may embed a connection string.
            tracing::warn!(error = ?error, "readiness probe: database unreachable");
            DependencyCheck {
                name: "database",
                status: "down",
                detail: Some(error.to_string()),
            }
        }
        Err(_elapsed) => {
            tracing::warn!(
                timeout_ms = PROBE_TIMEOUT.as_millis(),
                "readiness probe: database did not answer in time"
            );
            DependencyCheck {
                name: "database",
                status: "down",
                detail: Some("probe timed out".to_owned()),
            }
        }
    };

    let all_up = database.status == "up";
    let body = Readiness {
        status: if all_up { "ready" } else { "degraded" },
        checks: vec![database],
    };
    let code = if all_up {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (code, Json(body))
}
