// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

// See the note in `pagination`: `Problem` is a response document, not a
// hot-path error.
#![allow(clippy::result_large_err)]

//! Integrations an administrator configures (ADR 0016).
//!
//! Settings are checked by kind before they are stored. Secrets are
//! write-only: sent when an integration is made or changed, sealed with the
//! server's secret key, and never answered back; a view says only which are
//! set. The RomM export's row is listed but kept by its own setting, so it
//! cannot be changed or removed here.

use std::collections::BTreeMap;

use axum::extract::DefaultBodyLimit;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use tangible_db::integrations::{
    IntegrationChange, IntegrationOutcome, IntegrationRecord, NewIntegration, NewSecret,
    ROMM_INTEGRATION_NAME, create_integration, delete_integration, get_integration,
    list_integrations, record_integration_health, update_integration,
};
use tangible_domain::SecretString;
use tangible_integrations::download::ClientError;
use tangible_integrations::outbound::OutboundPolicy;
use tangible_integrations::romm::MARKER;
use tangible_integrations::romm_api::{Probe, Scope};
use tangible_integrations::settings::{ConfigurableKind, checked_settings};
use time::format_description::well_known::Rfc3339;
use utoipa::ToSchema;

use crate::auth::SignedIn;
use crate::download_clients::{OpenError, open_client};
use crate::problem::{ErrorCode, Problem};
use crate::routes::integration_jobs;
use crate::secrets::Sealed;
use crate::state::ApiState;

/// The longest secret accepted.
const MAX_SECRET_CHARS: usize = 1024;

/// An integration, with its secrets named and never shown.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct IntegrationView {
    /// Its id.
    pub id: String,
    /// Such as `qbittorrent`, or `romm` for the RomM export.
    pub kind: String,
    /// Its name.
    pub name: String,
    /// Whether it is in use.
    pub enabled: bool,
    /// Its settings.
    #[schema(value_type = BTreeMap<String, serde_json::Value>)]
    pub settings: serde_json::Value,
    /// Every secret its kind takes, and whether each is set.
    pub secrets: Vec<SecretFieldView>,
    /// `unconfigured`, `ok`, `degraded`, `failing` or `disabled`.
    pub status: String,
    /// When it last worked.
    pub last_success_at: Option<String>,
    /// Why it last failed, as a stable code.
    pub last_error_code: Option<String>,
    /// Kept by the server's own settings (the RomM export), not here.
    pub managed: bool,
    /// The client's API version, as last seen.
    pub client_version: Option<String>,
}

/// A secret an integration takes.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct SecretFieldView {
    /// Its name, such as `password`.
    pub field: String,
    /// Whether a value is stored. The value itself is never answered.
    pub set: bool,
}

/// Every integration.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct IntegrationList {
    /// By name.
    pub items: Vec<IntegrationView>,
    /// Whether this server holds a secret key, without which no secret can
    /// be stored (`TANGIBLE_SECRET_KEY_FILE`).
    pub secret_key_loaded: bool,
}

impl From<IntegrationRecord> for IntegrationView {
    fn from(record: IntegrationRecord) -> Self {
        let fields: Vec<String> = ConfigurableKind::parse(&record.kind)
            .map(|kind| {
                kind.secret_fields()
                    .iter()
                    .map(|f| (*f).to_owned())
                    .collect()
            })
            .unwrap_or_default();
        Self {
            id: record.id.to_string(),
            secrets: fields
                .into_iter()
                .map(|field| SecretFieldView {
                    set: record.secret_fields.contains(&field),
                    field,
                })
                .collect(),
            managed: record.name == ROMM_INTEGRATION_NAME,
            kind: record.kind,
            name: record.name,
            enabled: record.enabled,
            settings: record.configuration,
            status: record.status,
            last_success_at: record
                .last_success_at
                .and_then(|at| at.format(&Rfc3339).ok()),
            last_error_code: record.last_error_code,
            client_version: record.client_version,
        }
    }
}

/// What an administrator sends to make an integration.
#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CreateIntegrationRequest {
    /// `qbittorrent`.
    pub kind: String,
    /// A name for it, unique.
    pub name: String,
    /// Whether it is in use; on by default.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Its kind's settings.
    #[schema(value_type = BTreeMap<String, serde_json::Value>)]
    pub settings: serde_json::Value,
    /// Its secrets, by field. Write-only.
    #[serde(default)]
    #[schema(value_type = BTreeMap<String, String>)]
    pub secrets: BTreeMap<String, SecretString>,
}

fn default_true() -> bool {
    true
}

/// What an administrator sends to change an integration. Absent fields stay
/// as they are.
#[derive(Debug, Clone, Default, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct UpdateIntegrationRequest {
    /// A new name.
    #[serde(default)]
    pub name: Option<String>,
    /// On or off.
    #[serde(default)]
    pub enabled: Option<bool>,
    /// New settings, whole.
    #[serde(default)]
    #[schema(value_type = Option<BTreeMap<String, serde_json::Value>>)]
    pub settings: Option<serde_json::Value>,
    /// Secrets to set, or to remove with `null`, by field. Write-only.
    #[serde(default)]
    #[schema(value_type = BTreeMap<String, Option<String>>)]
    pub secrets: BTreeMap<String, Option<SecretString>>,
}

pub(crate) fn unavailable(what: &'static str) -> impl FnOnce(tangible_db::DbError) -> Problem {
    move |error| {
        tracing::error!(error = ?error, "{what}");
        Problem::new(ErrorCode::StorageUnavailable, what)
    }
}

pub(crate) fn parse_id(raw: &str) -> Result<uuid::Uuid, Problem> {
    uuid::Uuid::parse_str(raw)
        .map_err(|_| Problem::invalid_parameter("integration_id", "not a valid identifier"))
}

fn outcome_problem(id: Option<uuid::Uuid>, outcome: &IntegrationOutcome) -> Problem {
    match outcome {
        IntegrationOutcome::NotFound => Problem::not_found(
            "integration",
            &id.map(|id| id.to_string()).unwrap_or_default(),
        ),
        IntegrationOutcome::NameTaken => {
            Problem::new(ErrorCode::Conflict, "another integration has that name")
        }
        IntegrationOutcome::Managed => Problem::new(
            ErrorCode::Conflict,
            "this is the RomM export's, kept by TANGIBLE_ROMM_EXPORT_ROOT; change it there",
        ),
        IntegrationOutcome::InUse => Problem::new(
            ErrorCode::Conflict,
            "imports still refer to this integration; turn it off instead",
        ),
        IntegrationOutcome::OnlyOne => Problem::new(
            ErrorCode::Conflict,
            "there is a RomM API integration already, for the one export; change that one",
        ),
    }
}

fn checked_name(name: &str) -> Result<&str, Problem> {
    let name = name.trim();
    if name.is_empty() || name.chars().count() > 200 || name.chars().any(char::is_control) {
        return Err(Problem::new(
            ErrorCode::ValidationFailed,
            "a name is 1 to 200 characters",
        ));
    }
    Ok(name)
}

/// Seal `secrets` for integration `id` of `kind`, refusing fields the kind
/// does not take and a server without a key.
fn sealed_secrets(
    state: &ApiState,
    kind: ConfigurableKind,
    id: uuid::Uuid,
    secrets: &BTreeMap<String, SecretString>,
) -> Result<Vec<(String, Sealed)>, Problem> {
    if secrets.is_empty() {
        return Ok(Vec::new());
    }
    let Some(key) = state.secret_key() else {
        return Err(Problem::new(
            ErrorCode::SecretKeyMissing,
            "this server holds no secret key, so it cannot store a secret; an administrator \
             sets TANGIBLE_SECRET_KEY_FILE (tangible secrets generate makes a key)",
        ));
    };
    let mut sealed = Vec::with_capacity(secrets.len());
    for (field, value) in secrets {
        if !kind.secret_fields().contains(&field.as_str()) {
            return Err(Problem::new(
                ErrorCode::ValidationFailed,
                format!(
                    "a {} integration takes no secret {field}; it takes {}",
                    kind.as_str(),
                    kind.secret_fields().join(", ")
                ),
            ));
        }
        let length = value.expose().chars().count();
        if length == 0 || length > MAX_SECRET_CHARS {
            return Err(Problem::new(
                ErrorCode::ValidationFailed,
                format!("{field} must be 1 to {MAX_SECRET_CHARS} characters"),
            ));
        }
        let seal = key
            .seal(&id.to_string(), field, value)
            .map_err(|_| Problem::new(ErrorCode::Internal, "the secret could not be sealed"))?;
        sealed.push((field.clone(), seal));
    }
    Ok(sealed)
}

fn as_new_secrets(sealed: &[(String, Sealed)]) -> Vec<NewSecret<'_>> {
    sealed
        .iter()
        .map(|(field, seal)| NewSecret {
            field,
            key_id: &seal.key_id,
            nonce: &seal.nonce,
            ciphertext: &seal.ciphertext,
        })
        .collect()
}

fn known_roots(state: &ApiState) -> Vec<String> {
    state
        .imports()
        .map(|imports| imports.roots.ids())
        .unwrap_or_default()
}

/// What integrations are reached under: the server's own client's policy,
/// so the blocks an administrator allowed count, or the plain integrations
/// policy on a server without one.
fn policy(state: &ApiState) -> OutboundPolicy {
    state.integration_client().map_or_else(
        || OutboundPolicy::for_integrations(Vec::new(), None),
        |client| client.policy().clone(),
    )
}

/// Every integration.
///
/// # Errors
///
/// `STORAGE_UNAVAILABLE` if they cannot be read.
#[utoipa::path(
    get,
    path = "/api/v1/integrations",
    tag = "integrations",
    responses(
        (status = 200, description = "Every integration", body = IntegrationList),
        (status = 403, description = "Not an administrator", body = Problem),
    ),
)]
pub async fn list_all(State(state): State<ApiState>) -> Result<Json<IntegrationList>, Problem> {
    let items = list_integrations(state.database().pool())
        .await
        .map_err(unavailable("integrations could not be read"))?;
    Ok(Json(IntegrationList {
        items: items.into_iter().map(IntegrationView::from).collect(),
        secret_key_loaded: state.secret_key().is_some(),
    }))
}

/// One integration.
///
/// # Errors
///
/// `NOT_FOUND`.
#[utoipa::path(
    get,
    path = "/api/v1/integrations/{integration_id}",
    tag = "integrations",
    params(("integration_id" = String, Path, description = "Integration identifier")),
    responses(
        (status = 200, description = "The integration", body = IntegrationView),
        (status = 404, description = "No such integration", body = Problem),
    ),
)]
pub async fn get_one(
    State(state): State<ApiState>,
    Path(integration_id): Path<String>,
) -> Result<Json<IntegrationView>, Problem> {
    let id = parse_id(&integration_id)?;
    let record = get_integration(state.database().pool(), id)
        .await
        .map_err(unavailable("the integration could not be read"))?
        .ok_or_else(|| Problem::not_found("integration", &id.to_string()))?;
    Ok(Json(IntegrationView::from(record)))
}

/// Make an integration.
///
/// # Errors
///
/// `VALIDATION_FAILED` for a kind, name, settings or secret that will not do;
/// `CONFLICT` for a name taken; `SECRET_KEY_MISSING` when a secret is sent
/// and the server holds no key.
#[utoipa::path(
    post,
    path = "/api/v1/integrations",
    tag = "integrations",
    description = "Make an integration. Secrets are write-only: sealed with the server's \\
                   secret key and never answered back.",
    request_body = CreateIntegrationRequest,
    responses(
        (status = 201, description = "Made", body = IntegrationView),
        (status = 409, description = "Name taken, or no secret key", body = Problem),
        (status = 422, description = "Something that will not do", body = Problem),
    ),
)]
pub async fn create_one(
    State(state): State<ApiState>,
    SignedIn(user): SignedIn,
    Json(request): Json<CreateIntegrationRequest>,
) -> Result<axum::response::Response, Problem> {
    let kind = ConfigurableKind::parse(&request.kind).ok_or_else(|| {
        Problem::new(
            ErrorCode::ValidationFailed,
            "kind must be qbittorrent, sabnzbd, nzbget, s3 or romm_api; others are not \
             configurable yet",
        )
    })?;
    let name = checked_name(&request.name)?;
    let settings = checked_settings(
        kind,
        &request.settings,
        &known_roots(&state),
        &policy(&state),
    )
    .map_err(|error| Problem::new(ErrorCode::ValidationFailed, error.0))?;
    let id = uuid::Uuid::now_v7();
    let sealed = sealed_secrets(&state, kind, id, &request.secrets)?;
    let secrets = as_new_secrets(&sealed);
    let record = create_integration(
        state.database().pool(),
        &NewIntegration {
            id,
            kind: kind.as_str(),
            name,
            enabled: request.enabled,
            configuration: &settings,
            secrets: &secrets,
            actor: &user.username,
        },
    )
    .await
    .map_err(unavailable("the integration could not be stored"))?
    .map_err(|outcome| outcome_problem(Some(id), &outcome))?;
    tracing::info!(integration_id = %record.id, kind = %record.kind, "integration made");
    prompt_romm_sync(&state, &record.kind);
    Ok((StatusCode::CREATED, Json(IntegrationView::from(record))).into_response())
}

/// RomM's API, newly configured or changed, can be asked at once for the
/// scans the export owes it.
fn prompt_romm_sync(state: &ApiState, kind: &str) {
    if kind == "romm_api"
        && let Some(romm) = state.romm()
    {
        romm.nudge_sync();
    }
}

/// Change an integration.
///
/// # Errors
///
/// As making one, and `NOT_FOUND`, and `CONFLICT` for the RomM export's.
#[utoipa::path(
    patch,
    path = "/api/v1/integrations/{integration_id}",
    tag = "integrations",
    description = "Change an integration. Absent fields stay as they are; a secret set to \\
                   null is removed. Changing settings or secrets marks it unconfigured until \\
                   it is next tried.",
    params(("integration_id" = String, Path, description = "Integration identifier")),
    request_body = UpdateIntegrationRequest,
    responses(
        (status = 200, description = "Changed", body = IntegrationView),
        (status = 404, description = "No such integration", body = Problem),
        (status = 409, description = "Name taken, managed, or no secret key", body = Problem),
        (status = 422, description = "Something that will not do", body = Problem),
    ),
)]
pub async fn update_one(
    State(state): State<ApiState>,
    SignedIn(user): SignedIn,
    Path(integration_id): Path<String>,
    Json(request): Json<UpdateIntegrationRequest>,
) -> Result<Json<IntegrationView>, Problem> {
    let id = parse_id(&integration_id)?;
    let current = get_integration(state.database().pool(), id)
        .await
        .map_err(unavailable("the integration could not be read"))?
        .ok_or_else(|| Problem::not_found("integration", &id.to_string()))?;
    if current.name == ROMM_INTEGRATION_NAME {
        return Err(outcome_problem(Some(id), &IntegrationOutcome::Managed));
    }
    let kind = ConfigurableKind::parse(&current.kind).ok_or_else(|| {
        Problem::new(
            ErrorCode::Conflict,
            "this integration's kind is not configurable here",
        )
    })?;
    let name = request.name.as_deref().map(checked_name).transpose()?;
    let settings = request
        .settings
        .as_ref()
        .map(|settings| checked_settings(kind, settings, &known_roots(&state), &policy(&state)))
        .transpose()
        .map_err(|error| Problem::new(ErrorCode::ValidationFailed, error.0))?;
    let to_set: BTreeMap<String, SecretString> = request
        .secrets
        .iter()
        .filter_map(|(field, value)| value.clone().map(|value| (field.clone(), value)))
        .collect();
    let to_remove: Vec<&str> = request
        .secrets
        .iter()
        .filter(|(_, value)| value.is_none())
        .map(|(field, _)| field.as_str())
        .collect();
    if let Some(field) = to_remove
        .iter()
        .find(|field| !kind.secret_fields().contains(field))
    {
        return Err(Problem::new(
            ErrorCode::ValidationFailed,
            format!("a {} integration takes no secret {field}", kind.as_str()),
        ));
    }
    let sealed = sealed_secrets(&state, kind, id, &to_set)?;
    let secrets = as_new_secrets(&sealed);
    let record = update_integration(
        state.database().pool(),
        id,
        &IntegrationChange {
            name,
            enabled: request.enabled,
            configuration: settings.as_ref(),
            set_secrets: &secrets,
            remove_secrets: &to_remove,
            actor: &user.username,
        },
    )
    .await
    .map_err(unavailable("the integration could not be changed"))?
    .map_err(|outcome| outcome_problem(Some(id), &outcome))?;
    prompt_romm_sync(&state, &record.kind);
    Ok(Json(IntegrationView::from(record)))
}

/// Remove an integration and its secrets.
///
/// # Errors
///
/// `NOT_FOUND`; `CONFLICT` for the RomM export's, or one imports refer to.
#[utoipa::path(
    delete,
    path = "/api/v1/integrations/{integration_id}",
    tag = "integrations",
    params(("integration_id" = String, Path, description = "Integration identifier")),
    responses(
        (status = 204, description = "Removed"),
        (status = 404, description = "No such integration", body = Problem),
        (status = 409, description = "Managed, or still referred to", body = Problem),
    ),
)]
pub async fn delete_one(
    State(state): State<ApiState>,
    SignedIn(user): SignedIn,
    Path(integration_id): Path<String>,
) -> Result<StatusCode, Problem> {
    let id = parse_id(&integration_id)?;
    delete_integration(state.database().pool(), id, &user.username)
        .await
        .map_err(unavailable("the integration could not be removed"))?
        .map_err(|outcome| outcome_problem(Some(id), &outcome))?;
    Ok(StatusCode::NO_CONTENT)
}

/// How trying an integration went.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct IntegrationTestView {
    /// Whether it signed in and answered.
    pub ok: bool,
    /// The client's API version, when it answered.
    pub api_version: Option<String>,
    /// Why not, as a stable code.
    pub error_code: Option<String>,
    /// Why not, in words.
    pub detail: Option<String>,
    /// What was checked, one by one, for an integration that checks more
    /// than whether it answers (RomM's API); otherwise empty.
    pub checks: Vec<IntegrationCheckView>,
}

/// One thing trying an integration checked.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct IntegrationCheckView {
    /// What, in words, such as `Asking for scans`.
    pub name: String,
    /// Whether it will do.
    pub ok: bool,
    /// What was found, in words.
    pub detail: String,
}

pub(crate) fn open_problem(error: &OpenError) -> Problem {
    let code = match error.code {
        "SECRET_KEY_MISSING" => ErrorCode::SecretKeyMissing,
        "INTEGRATION_MISCONFIGURED" => ErrorCode::IntegrationMisconfigured,
        "INTEGRATION_SECRET_UNREADABLE" => ErrorCode::IntegrationSecretUnreadable,
        "STORAGE_UNAVAILABLE" => ErrorCode::StorageUnavailable,
        _ => ErrorCode::IntegrationOff,
    };
    Problem::new(code, error.detail.clone())
}

pub(crate) fn client_problem(error: &ClientError) -> Problem {
    let code = match error {
        ClientError::Refused(_) => ErrorCode::IntegrationAddressRefused,
        ClientError::Unreachable { .. } => ErrorCode::IntegrationUnreachable,
        ClientError::Auth { .. } => ErrorCode::IntegrationAuthFailed,
        ClientError::BadResponse { .. } => ErrorCode::IntegrationBadResponse,
    };
    Problem::new(code, error.to_string())
}

/// Note how talking to the client went; a failure to note it is only logged.
pub(crate) async fn note_health(
    state: &ApiState,
    id: uuid::Uuid,
    outcome: Result<Option<&str>, &str>,
) {
    if let Err(error) = record_integration_health(state.database().pool(), id, outcome).await {
        tracing::warn!(integration_id = %id, error = ?error, "could not record the integration's health");
    }
}

/// Try an integration: sign in, ask the client's version, and check it has
/// the category.
///
/// # Errors
///
/// `NOT_FOUND`; `INTEGRATION_OFF` and the like when it cannot be tried at
/// all. A client that fails, or lacks the category, is a result,
/// `ok: false`, not an error.
#[utoipa::path(
    post,
    path = "/api/v1/integrations/{integration_id}/test",
    tag = "integrations",
    params(("integration_id" = String, Path, description = "Integration identifier")),
    responses(
        (status = 200, description = "How it went", body = IntegrationTestView),
        (status = 404, description = "No such integration", body = Problem),
        (status = 409, description = "It cannot be tried", body = Problem),
    ),
)]
pub async fn test_one(
    State(state): State<ApiState>,
    Path(integration_id): Path<String>,
) -> Result<Json<IntegrationTestView>, Problem> {
    let id = parse_id(&integration_id)?;
    let record = get_integration(state.database().pool(), id)
        .await
        .map_err(unavailable("the integration could not be read"))?
        .ok_or_else(|| Problem::not_found("integration", &id.to_string()))?;
    if record.kind == "s3" {
        return test_s3(&state, &record).await.map(Json);
    }
    if record.kind == "romm_api" {
        return test_romm(&state, &record).await.map(Json);
    }
    let (mut client, folders) = open_client(
        state.database(),
        state.integration_client(),
        state.secret_key(),
        &record,
    )
    .await
    .map_err(|error| open_problem(&error))?;
    let view = match client.version_and_category(&folders.category).await {
        Ok((version, true)) => {
            note_health(&state, id, Ok(Some(&version))).await;
            IntegrationTestView {
                ok: true,
                api_version: Some(version),
                error_code: None,
                detail: None,

                checks: Vec::new(),
            }
        }
        Ok((version, false)) => {
            note_health(&state, id, Err("INTEGRATION_MISCONFIGURED")).await;
            IntegrationTestView {
                ok: false,
                api_version: Some(version),
                error_code: Some("INTEGRATION_MISCONFIGURED".to_owned()),
                detail: Some(format!(
                    "it has no category {}; add it, with its own folder, or the downloads are \
                     filed under its default",
                    folders.category
                )),
                checks: Vec::new(),
            }
        }
        Err(error) => {
            note_health(&state, id, Err(error.code())).await;
            IntegrationTestView {
                ok: false,
                api_version: None,
                error_code: Some(error.code().to_owned()),
                detail: Some(error.to_string()),

                checks: Vec::new(),
            }
        }
    };
    Ok(Json(view))
}

/// Try an S3 integration: every bucket it allows must be there and open to
/// its credentials.
async fn test_s3(
    state: &ApiState,
    record: &tangible_db::integrations::IntegrationRecord,
) -> Result<IntegrationTestView, Problem> {
    use tangible_integrations::s3::ObjectError;
    let (client, settings) = crate::download_clients::open_s3(
        state.database(),
        state.integration_client(),
        state.secret_key(),
        record,
    )
    .await
    .map_err(|error| open_problem(&error))?;
    for bucket in &settings.buckets {
        let failed = match client.head_bucket(bucket).await {
            Ok(()) => continue,
            Err(ObjectError::Missing) => (
                "INTEGRATION_MISCONFIGURED",
                format!("bucket {bucket} is not there"),
            ),
            Err(ObjectError::Client(error)) => (error.code(), format!("bucket {bucket}: {error}")),
        };
        note_health(state, record.id, Err(failed.0)).await;
        return Ok(IntegrationTestView {
            ok: false,
            api_version: None,
            error_code: Some(failed.0.to_owned()),
            detail: Some(failed.1),

            checks: Vec::new(),
        });
    }
    note_health(state, record.id, Ok(None)).await;
    Ok(IntegrationTestView {
        ok: true,
        api_version: None,
        error_code: None,
        detail: None,

        checks: Vec::new(),
    })
}

/// Try a RomM API integration: its version, which of the operations
/// Tangible uses its API document lists, and which scopes the token
/// carries. It will do when exported games can be found in it; asking for
/// scans is a check of its own, since RomM before 5.4 cannot be asked.
async fn test_romm(
    state: &ApiState,
    record: &tangible_db::integrations::IntegrationRecord,
) -> Result<IntegrationTestView, Problem> {
    let (client, settings) = crate::download_clients::open_romm(
        state.database(),
        state.integration_client(),
        state.secret_key(),
        record,
    )
    .await
    .map_err(|error| open_problem(&error))?;
    let probe = match client.probe().await {
        Ok(probe) => probe,
        Err(error) => {
            note_health(state, record.id, Err(error.code())).await;
            return Ok(IntegrationTestView {
                ok: false,
                api_version: None,
                error_code: Some(error.code().to_owned()),
                detail: Some(error.to_string()),
                checks: Vec::new(),
            });
        }
    };
    let checks = romm_checks(&probe, settings.scan_after_export);
    let view = if probe.can_link() {
        note_health(state, record.id, Ok(Some(&probe.version))).await;
        IntegrationTestView {
            ok: true,
            api_version: Some(probe.version),
            error_code: None,
            detail: None,
            checks,
        }
    } else {
        let code = if probe.capabilities.client_tokens && probe.capabilities.lookup {
            "INTEGRATION_MISCONFIGURED"
        } else {
            "INTEGRATION_UNSUPPORTED"
        };
        note_health(state, record.id, Err(code)).await;
        IntegrationTestView {
            ok: false,
            api_version: Some(probe.version),
            error_code: Some(code.to_owned()),
            detail: checks
                .iter()
                .find(|check| !check.ok)
                .map(|check| check.detail.clone()),
            checks,
        }
    };
    Ok(view)
}

/// What a RomM probe found, check by check, in words.
fn romm_checks(probe: &Probe, scan_after_export: bool) -> Vec<IntegrationCheckView> {
    let check = romm_check;
    let lacking = |scopes: &[Scope]| -> Vec<&'static str> {
        scopes
            .iter()
            .filter(|scope| !probe.granted(**scope))
            .map(|scope| scope.as_str())
            .collect()
    };
    let capabilities = probe.capabilities;
    let mut checks = vec![if capabilities.client_tokens {
        check(
            "Client tokens",
            true,
            format!("RomM {} takes client tokens", probe.version),
        )
    } else {
        check(
            "Client tokens",
            false,
            format!(
                "RomM {} takes no client tokens; RomM 5.0 or later does",
                probe.version
            ),
        )
    }];
    let missing = lacking(&[Scope::PlatformsRead, Scope::RomsRead]);
    checks.push(if !capabilities.lookup {
        check(
            "Finding exported games",
            false,
            "its API document lacks the platform and ROM routes Tangible reads".to_owned(),
        )
    } else if !missing.is_empty() {
        check(
            "Finding exported games",
            false,
            format!(
                "the token lacks {}; make one with platforms.read, roms.read and tasks.run",
                missing.join(" and ")
            ),
        )
    } else {
        check(
            "Finding exported games",
            true,
            "its platforms and ROMs can be read".to_owned(),
        )
    });
    checks.push(scan_check(probe, scan_after_export));
    checks.push(if capabilities.known_platforms {
        check(
            "Known platforms",
            true,
            "the platforms RomM knows can be read, so one it would not recognise is flagged"
                .to_owned(),
        )
    } else {
        check(
            "Known platforms",
            false,
            "its API document lacks the list of platforms it knows; platforms are not checked"
                .to_owned(),
        )
    });
    checks.push(marker_check(probe.ignores_marker));
    checks
}

/// Whether RomM can be asked to scan, as a check.
fn scan_check(probe: &Probe, scan_after_export: bool) -> IntegrationCheckView {
    let check = romm_check;
    if !scan_after_export {
        check(
            "Asking for scans",
            true,
            "turned off for this integration: RomM scans when it is told to there".to_owned(),
        )
    } else if !probe.capabilities.scan {
        check(
            "Asking for scans",
            false,
            "this RomM cannot be asked to scan; RomM 5.4 or later can. Until then, scan in \
             RomM after an export changes, or turn on its scheduled rescan"
                .to_owned(),
        )
    } else if !probe.granted(Scope::TasksRun) {
        check(
            "Asking for scans",
            false,
            "the token lacks tasks.run, so RomM cannot be asked to scan".to_owned(),
        )
    } else {
        check(
            "Asking for scans",
            true,
            "a quick scan follows each change an export makes".to_owned(),
        )
    }
}

/// Whether RomM leaves the export's marker out of each game, as a check.
fn marker_check(ignores_marker: Option<bool>) -> IntegrationCheckView {
    let check = romm_check;
    match ignores_marker {
        Some(true) => check(
            "Export marker",
            true,
            format!("RomM leaves {MARKER} out of each game's files"),
        ),
        Some(false) => check(
            "Export marker",
            false,
            format!(
                "RomM counts {MARKER}, the file that marks a folder as Tangible's, as one of \
                 the game's files; name it under exclude.roms.multi_file.parts.names in RomM's \
                 config.yml and scan again"
            ),
        ),
        None => check(
            "Export marker",
            false,
            format!(
                "RomM's configuration could not be read, so whether it counts {MARKER} as one \
                 of each game's files is not known"
            ),
        ),
    }
}

fn romm_check(name: &str, ok: bool, detail: String) -> IntegrationCheckView {
    IntegrationCheckView {
        name: name.to_owned(),
        ok,
        detail,
    }
}

/// The integration routes.
pub fn router() -> Router<ApiState> {
    Router::new()
        .route("/integrations", get(list_all).post(create_one))
        .route(
            "/integrations/{integration_id}",
            get(get_one).patch(update_one).delete(delete_one),
        )
        .route(
            "/integrations/{integration_id}/jobs",
            post(integration_jobs::submit_job).layer(DefaultBodyLimit::max(
                integration_jobs::MAX_SUBMISSION_BYTES,
            )),
        )
        .route("/integrations/{integration_id}/test", post(test_one))
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
mod tests {
    use tangible_integrations::romm_api::Capabilities;

    use super::*;

    const ALL: Capabilities = Capabilities {
        client_tokens: true,
        lookup: true,
        known_platforms: true,
        scan: true,
    };

    fn probe(capabilities: Capabilities, granted: &[Scope]) -> Probe {
        Probe {
            version: "5.4.1".to_owned(),
            capabilities,
            scopes: Scope::ALL
                .iter()
                .map(|scope| (*scope, granted.contains(scope)))
                .collect(),
            ignores_marker: Some(true),
        }
    }

    fn verdicts(checks: &[IntegrationCheckView]) -> Vec<(&str, bool)> {
        checks
            .iter()
            .map(|check| (check.name.as_str(), check.ok))
            .collect()
    }

    #[test]
    fn a_romm_with_everything_passes_every_check() {
        let checks = romm_checks(&probe(ALL, &Scope::ALL), true);
        assert_eq!(
            verdicts(&checks),
            [
                ("Client tokens", true),
                ("Finding exported games", true),
                ("Asking for scans", true),
                ("Known platforms", true),
                ("Export marker", true),
            ]
        );
    }

    #[test]
    fn each_shortfall_is_said_in_its_own_check() {
        let narrow = romm_checks(&probe(ALL, &[Scope::RomsRead, Scope::TasksRun]), true);
        assert!(!narrow[1].ok);
        assert!(
            narrow[1].detail.contains("lacks platforms.read"),
            "{narrow:?}"
        );
        let no_tasks = romm_checks(&probe(ALL, &[Scope::PlatformsRead, Scope::RomsRead]), true);
        assert!(no_tasks[1].ok && !no_tasks[2].ok);
        assert!(no_tasks[2].detail.contains("tasks.run"), "{no_tasks:?}");
        let before_scans = Capabilities { scan: false, ..ALL };
        let old = romm_checks(&probe(before_scans, &Scope::ALL), true);
        assert!(
            !old[2].ok && old[2].detail.contains("5.4 or later"),
            "{old:?}"
        );
        // An operator who turned scans off is not told they are missing.
        let off = romm_checks(&probe(before_scans, &Scope::ALL), false);
        assert!(off[2].ok && off[2].detail.contains("turned off"), "{off:?}");
        let ancient = romm_checks(&probe(Capabilities::default(), &Scope::ALL), true);
        assert!(!ancient[0].ok && ancient[0].detail.contains("5.0 or later"));
        assert!(!ancient[1].ok && !ancient[3].ok);
        let counted = Probe {
            ignores_marker: Some(false),
            ..probe(ALL, &Scope::ALL)
        };
        let checks = romm_checks(&counted, true);
        assert!(
            !checks[4].ok
                && checks[4]
                    .detail
                    .contains("exclude.roms.multi_file.parts.names")
        );
        let unread = Probe {
            ignores_marker: None,
            ..probe(ALL, &Scope::ALL)
        };
        assert!(!romm_checks(&unread, true)[4].ok);
    }
}
