// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

// See the note in `pagination`: `Problem` is a response document, not a
// hot-path error.
#![allow(clippy::result_large_err)]

//! Importing an object from an S3-compatible bucket an administrator
//! configured (docs/plans/acquisition.md A6, ADR 0016).
//!
//! The operator names an integration, one of its allowed buckets, and an
//! object key. The object is looked at first (HEAD: does it exist, how large
//! is it, which version), and the import is pinned to that version and
//! size, so a later write to the same key cannot become this import. The
//! runner then fetches it with the URL import's resumable fetch from a
//! presigned GET. The key is the operator's: checked, recorded, and never
//! used as a path.

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde::{Deserialize, Serialize};
use tangible_db::integrations::get_integration;
use tangible_db::repositories::{NewImportJob, create_audited_import_job};
use tangible_domain::{ImportJobId, ImportState};
use tangible_integrations::s3::{ObjectError, check_key};
use utoipa::ToSchema;

use crate::auth::SignedIn;
use crate::download_clients::open_s3;
use crate::problem::{ErrorCode, Problem};
use crate::routes::imports::{ImportView, staged_name};
use crate::routes::integrations::{
    client_problem, note_health, open_problem, parse_id, unavailable,
};
use crate::state::ApiState;

/// What an operator sends to import an object from S3.
#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct S3ImportRequest {
    /// The S3 integration.
    pub integration_id: String,
    /// One of the buckets it allows.
    pub bucket: String,
    /// The object's key.
    pub key: String,
    /// What to call the file. Defaults to the key's last segment.
    #[serde(default)]
    pub filename: Option<String>,
    /// The file's SHA-256, when the operator knows it: checked after the
    /// fetch, and the import fails if it differs.
    #[serde(default)]
    pub expected_sha256: Option<String>,
}

/// An S3 integration operators may import from.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct S3SourceView {
    /// The integration's id.
    pub id: String,
    /// Its name.
    pub name: String,
    /// The buckets it allows.
    pub buckets: Vec<String>,
}

fn invalid(detail: impl Into<String>) -> Problem {
    Problem::new(ErrorCode::ValidationFailed, detail)
}

/// What a request names, checked before anything is asked of the endpoint.
struct Checked {
    id: uuid::Uuid,
    filename: tangible_domain::LogicalPath,
    expected_sha256: Option<String>,
}

fn checked(request: &S3ImportRequest) -> Result<Checked, Problem> {
    let id = parse_id(&request.integration_id)?;
    check_key(&request.key).map_err(|error| invalid(error.0))?;
    let filename = match request.filename.as_deref() {
        Some(name) => staged_name(Some(name))?,
        None => request
            .key
            .rsplit('/')
            .next()
            .filter(|segment| !segment.is_empty())
            .and_then(|segment| staged_name(Some(segment)).ok())
            .map_or_else(|| staged_name(Some("object.bin")), Ok)?,
    };
    let expected_sha256 = request
        .expected_sha256
        .as_deref()
        .map(|digest| digest.trim().to_ascii_lowercase())
        .map(|digest| {
            (digest.len() == 64 && digest.bytes().all(|b| b.is_ascii_hexdigit()))
                .then_some(digest)
                .ok_or_else(|| invalid("expected_sha256 must be 64 hexadecimal characters"))
        })
        .transpose()?;
    Ok(Checked {
        id,
        filename,
        expected_sha256,
    })
}

/// The import's descriptor: where the object is, the name it is staged
/// under, and what pins it (its version, ETag and size).
fn descriptor(
    id: uuid::Uuid,
    bucket: &str,
    key: &str,
    filename: &str,
    info: &tangible_integrations::s3::ObjectInfo,
    expected_sha256: Option<&str>,
) -> serde_json::Value {
    let mut descriptor = serde_json::json!({
        "kind": "s3_object",
        "integration_id": id,
        "bucket": bucket,
        "key": key,
        "filename": filename,
        "reference": format!("s3://{bucket}/{key}"),
        // Pinned: the fetch asks for this version and checks this size.
        "expected_bytes": info.size,
    });
    if let Some(version) = &info.version_id {
        descriptor["version_id"] = serde_json::json!(version);
    }
    if let Some(etag) = &info.etag {
        descriptor["etag"] = serde_json::json!(etag);
    }
    if let Some(digest) = expected_sha256 {
        descriptor["expected_sha256"] = serde_json::json!(digest);
    }
    descriptor
}

/// Import an object from an S3-compatible bucket.
///
/// # Errors
///
/// `VALIDATION_FAILED` for a bucket the integration does not allow, an
/// unusable key, filename or digest, or an object past the size limit;
/// `REFERENCE_NOT_FOUND` for no such object; `NOT_FOUND` for no such
/// integration; `INTEGRATION_OFF` and the like for one that cannot be used;
/// the endpoint's failures as `INTEGRATION_*`.
#[utoipa::path(
    post,
    path = "/api/v1/imports/s3",
    tag = "imports",
    description = "Import an object from a bucket an S3 integration allows. The object is \
                   looked at first, and the import pinned to its version and size; the \
                   server then fetches it, resuming an interrupted fetch.",
    request_body = S3ImportRequest,
    responses(
        (status = 202, description = "Queued", body = ImportView),
        (status = 404, description = "No such integration", body = Problem),
        (status = 409, description = "The integration cannot be used", body = Problem),
        (status = 422, description = "Something that will not do, or no such object", body = Problem),
        (status = 502, description = "The endpoint failed", body = Problem),
        (status = 503, description = "Storage is not configured", body = Problem),
    ),
)]
pub async fn s3_import(
    State(state): State<ApiState>,
    SignedIn(user): SignedIn,
    Json(request): Json<S3ImportRequest>,
) -> Result<axum::response::Response, Problem> {
    let Some(imports) = state.imports() else {
        return Err(Problem::new(
            ErrorCode::StorageUnavailable,
            "importing is not configured on this server",
        ));
    };
    let Checked {
        id,
        filename,
        expected_sha256,
    } = checked(&request)?;
    let bucket = request.bucket.trim();
    let key = request.key.as_str();

    let record = get_integration(state.database().pool(), id)
        .await
        .map_err(unavailable("the integration could not be read"))?
        .ok_or_else(|| Problem::not_found("integration", &id.to_string()))?;
    let (client, settings) = open_s3(
        state.database(),
        state.integration_client(),
        state.secret_key(),
        &record,
    )
    .await
    .map_err(|error| open_problem(&error))?;
    if !settings.buckets.iter().any(|allowed| allowed == bucket) {
        return Err(invalid(format!(
            "{} does not allow bucket {bucket}; it allows {}",
            record.name,
            settings.buckets.join(", ")
        )));
    }

    let info = match client.head(bucket, key).await {
        Ok(info) => info,
        Err(ObjectError::Missing) => {
            note_health(&state, id, Ok(None)).await;
            return Err(Problem::new(
                ErrorCode::ReferenceNotFound,
                format!("there is no such object in bucket {bucket}"),
            ));
        }
        Err(ObjectError::Client(error)) => {
            note_health(&state, id, Err(error.code())).await;
            return Err(client_problem(&error));
        }
    };
    note_health(&state, id, Ok(None)).await;
    if info.size > imports.max_upload_bytes {
        return Err(invalid(format!(
            "the object is {} bytes, past this server's limit of {} bytes",
            info.size, imports.max_upload_bytes
        )));
    }

    let reference = format!("s3://{bucket}/{key}");
    let descriptor = descriptor(
        id,
        bucket,
        key,
        filename.as_str(),
        &info,
        expected_sha256.as_deref(),
    );
    let metadata = serde_json::json!({
        "integration_id": id,
        "reference": reference,
        "version_id": info.version_id,
    });
    let job = create_audited_import_job(
        state.database().pool(),
        NewImportJob {
            id: ImportJobId::generate(),
            source_kind: "s3_object",
            source_descriptor: &descriptor,
            state: ImportState::Requested,
            bytes_expected: i64::try_from(info.size).ok(),
            bytes_received: 0,
            created_by: &user.username,
        },
        "import.s3_requested",
        &metadata,
    )
    .await
    .map_err(unavailable("the import could not be queued"))?;
    tracing::info!(import_id = %job.id, source = %reference, "S3 import queued");
    Ok((StatusCode::ACCEPTED, Json(ImportView::from(job))).into_response())
}
