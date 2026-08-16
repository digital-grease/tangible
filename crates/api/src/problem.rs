// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! RFC 9457 problem details.
//!
//! Every error leaving the API carries a machine-readable `code` alongside the
//! human-readable `detail`. The code is the contract: it is stable, clients may
//! branch on it, and it must not change once published. The prose may be
//! reworded freely.
//!
//! Errors also avoid leaking internals. A storage failure becomes
//! `STORAGE_UNAVAILABLE` with a generic sentence; the underlying path,
//! connection string, or io error text is logged server-side and never
//! serialized, because a caller who cannot read the library also should not
//! learn its directory layout from an error message.

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use utoipa::ToSchema;

/// A machine-readable error code.
///
/// Stable across releases. Adding a variant is a compatible change; renaming
/// or repurposing one is not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorCode {
    /// The requested resource does not exist.
    NotFound,
    /// A path or query parameter was malformed.
    InvalidParameter,
    /// A pagination cursor was not one this server issued.
    InvalidCursor,
    /// Stored data could not be read or is inconsistent.
    StorageUnavailable,
    /// A stored manifest failed its own validation.
    ManifestInvalid,
    /// Anything unanticipated.
    Internal,
}

impl ErrorCode {
    /// The stable wire value.
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::NotFound => "NOT_FOUND",
            Self::InvalidParameter => "INVALID_PARAMETER",
            Self::InvalidCursor => "INVALID_CURSOR",
            Self::StorageUnavailable => "STORAGE_UNAVAILABLE",
            Self::ManifestInvalid => "MANIFEST_INVALID",
            Self::Internal => "INTERNAL",
        }
    }

    /// The HTTP status this code maps to.
    #[must_use]
    pub const fn status(&self) -> StatusCode {
        match self {
            Self::NotFound => StatusCode::NOT_FOUND,
            Self::InvalidParameter | Self::InvalidCursor => StatusCode::BAD_REQUEST,
            // 503 rather than 500: the library being unreadable is usually a
            // mount or permissions problem an operator can fix, and it is
            // worth retrying.
            Self::StorageUnavailable => StatusCode::SERVICE_UNAVAILABLE,
            // The request was fine; the stored data is not.
            Self::ManifestInvalid => StatusCode::UNPROCESSABLE_ENTITY,
            Self::Internal => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    /// A short human-readable title.
    #[must_use]
    pub const fn title(&self) -> &'static str {
        match self {
            Self::NotFound => "Resource not found",
            Self::InvalidParameter => "Invalid parameter",
            Self::InvalidCursor => "Invalid pagination cursor",
            Self::StorageUnavailable => "Storage is unavailable",
            Self::ManifestInvalid => "Stored manifest is invalid",
            Self::Internal => "Internal error",
        }
    }
}

/// An RFC 9457 problem document.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct Problem {
    /// A URI identifying the problem type.
    #[serde(rename = "type")]
    pub problem_type: String,
    /// Short human-readable summary.
    pub title: String,
    /// HTTP status code, repeated in the body as the RFC allows.
    pub status: u16,
    /// Stable machine-readable code. Clients branch on this, not on `detail`.
    pub code: String,
    /// Human-readable explanation. May be reworded between releases.
    pub detail: String,
    /// The request path, when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instance: Option<String>,
}

impl Problem {
    /// Build a problem document.
    #[must_use]
    pub fn new(code: ErrorCode, detail: impl Into<String>) -> Self {
        Self {
            problem_type: format!("https://tangible.invalid/errors/{}", code.as_str()),
            title: code.title().to_owned(),
            status: code.status().as_u16(),
            code: code.as_str().to_owned(),
            detail: detail.into(),
            instance: None,
        }
    }

    /// Attach the request path.
    #[must_use]
    pub fn at(mut self, instance: impl Into<String>) -> Self {
        self.instance = Some(instance.into());
        self
    }

    /// The status this problem carries.
    #[must_use]
    pub fn status_code(&self) -> StatusCode {
        StatusCode::from_u16(self.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR)
    }
}

impl IntoResponse for Problem {
    fn into_response(self) -> Response {
        let status = self.status_code();
        let mut response = (status, Json(&self)).into_response();
        // The RFC media type, so a client can distinguish a problem document
        // from a successful body that happens to have a `status` field.
        response.headers_mut().insert(
            axum::http::header::CONTENT_TYPE,
            axum::http::HeaderValue::from_static("application/problem+json"),
        );
        response
    }
}

/// Convenience constructors for the common cases.
impl Problem {
    /// A resource that does not exist.
    #[must_use]
    pub fn not_found(what: &str, id: &str) -> Self {
        Self::new(ErrorCode::NotFound, format!("no {what} with id {id}"))
    }

    /// A malformed parameter.
    #[must_use]
    pub fn invalid_parameter(name: &str, why: &str) -> Self {
        Self::new(
            ErrorCode::InvalidParameter,
            format!("parameter {name} is invalid: {why}"),
        )
    }

    /// Storage could not be read.
    ///
    /// The cause is deliberately not included: it may name a filesystem path
    /// or a mount point. It is logged instead.
    #[must_use]
    pub fn storage_unavailable() -> Self {
        Self::new(
            ErrorCode::StorageUnavailable,
            "the library could not be read; see server logs",
        )
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn codes_map_to_sensible_statuses() {
        assert_eq!(ErrorCode::NotFound.status(), StatusCode::NOT_FOUND);
        assert_eq!(ErrorCode::InvalidCursor.status(), StatusCode::BAD_REQUEST);
        // Retryable, and usually an operator-fixable mount problem.
        assert_eq!(
            ErrorCode::StorageUnavailable.status(),
            StatusCode::SERVICE_UNAVAILABLE
        );
        // The request was valid; the stored data is not.
        assert_eq!(
            ErrorCode::ManifestInvalid.status(),
            StatusCode::UNPROCESSABLE_ENTITY
        );
    }

    #[test]
    fn every_code_has_a_distinct_wire_value() {
        let codes = [
            ErrorCode::NotFound,
            ErrorCode::InvalidParameter,
            ErrorCode::InvalidCursor,
            ErrorCode::StorageUnavailable,
            ErrorCode::ManifestInvalid,
            ErrorCode::Internal,
        ];
        let mut seen = std::collections::BTreeSet::new();
        for code in codes {
            assert!(
                seen.insert(code.as_str()),
                "duplicate code {}",
                code.as_str()
            );
        }
    }

    #[test]
    fn a_problem_serializes_with_its_code() {
        let problem = Problem::not_found("artifact", "abc").at("/api/v1/artifacts/abc");
        let json = serde_json::to_value(&problem).expect("serialize");

        assert_eq!(json["code"], "NOT_FOUND");
        assert_eq!(json["status"], 404);
        assert_eq!(json["instance"], "/api/v1/artifacts/abc");
        assert!(json["type"].as_str().expect("type").contains("NOT_FOUND"));
    }

    #[test]
    fn the_storage_problem_leaks_no_internals() {
        // A caller who cannot read the library must not learn its layout from
        // the error.
        let detail = Problem::storage_unavailable().detail;
        assert!(!detail.contains('/'), "detail leaked a path: {detail}");
    }

    #[test]
    fn instance_is_omitted_when_absent() {
        let json = serde_json::to_value(Problem::not_found("artifact", "x")).expect("serialize");
        assert!(json.get("instance").is_none());
    }
}
