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
    ROMM_INTEGRATION_NAME, Submitted, create_integration, delete_integration, get_integration,
    list_integrations, record_integration_health, submit_integration_job, update_integration,
};
use tangible_domain::SecretString;
use tangible_integrations::outbound::OutboundPolicy;
use tangible_integrations::qbittorrent::{
    Added, QbittorrentClient, QbittorrentError, TorrentSource,
};
use tangible_integrations::settings::{ConfigurableKind, checked_settings};
use tangible_integrations::torrent::{
    Infohash, MAX_TORRENT_BYTES, infohash_of_magnet, infohash_of_torrent,
};
use time::format_description::well_known::Rfc3339;
use utoipa::ToSchema;

use crate::auth::SignedIn;
use crate::download_clients::{OpenError, open_qbittorrent};
use crate::problem::{ErrorCode, Problem};
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

fn unavailable(what: &'static str) -> impl FnOnce(tangible_db::DbError) -> Problem {
    move |error| {
        tracing::error!(error = ?error, "{what}");
        Problem::new(ErrorCode::StorageUnavailable, what)
    }
}

fn parse_id(raw: &str) -> Result<uuid::Uuid, Problem> {
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
            "kind must be qbittorrent; others are not configurable yet",
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
    Ok((StatusCode::CREATED, Json(IntegrationView::from(record))).into_response())
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

/// The largest body a torrent submission may have: a `.torrent` at its
/// limit, written base64, with room for the JSON around it.
const MAX_SUBMISSION_BYTES: usize = MAX_TORRENT_BYTES / 3 * 4 + 4096;

/// A torrent the operator already has, for a download client to fetch.
/// Exactly one of the two.
#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct SubmitTorrentRequest {
    /// A magnet link.
    #[serde(default)]
    pub magnet: Option<String>,
    /// A `.torrent` file, base64.
    #[serde(default)]
    pub torrent: Option<String>,
}

/// A torrent handed to a download client, and the import following it.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct IntegrationJobView {
    /// The integration.
    pub integration_id: String,
    /// The torrent's infohash, which the import follows it by.
    pub infohash: String,
    /// The import that brings it into the library when it finishes.
    pub import_id: String,
    /// `created`; `resubmitted` when an earlier import of it ended without
    /// an artifact; `already_active` when one is still going;
    /// `already_imported` when it is in the library.
    pub outcome: String,
    /// What it became, when already imported.
    pub artifact_id: Option<String>,
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
}

fn open_problem(error: &OpenError) -> Problem {
    let code = match error.code {
        "SECRET_KEY_MISSING" => ErrorCode::SecretKeyMissing,
        "INTEGRATION_MISCONFIGURED" => ErrorCode::IntegrationMisconfigured,
        "INTEGRATION_SECRET_UNREADABLE" => ErrorCode::IntegrationSecretUnreadable,
        "STORAGE_UNAVAILABLE" => ErrorCode::StorageUnavailable,
        _ => ErrorCode::IntegrationOff,
    };
    Problem::new(code, error.detail.clone())
}

fn client_problem(error: &QbittorrentError) -> Problem {
    let code = match error {
        QbittorrentError::Refused(_) => ErrorCode::IntegrationAddressRefused,
        QbittorrentError::Unreachable(_) => ErrorCode::IntegrationUnreachable,
        QbittorrentError::Auth(_) => ErrorCode::IntegrationAuthFailed,
        QbittorrentError::BadResponse(_) => ErrorCode::IntegrationBadResponse,
    };
    Problem::new(code, error.to_string())
}

/// Note how talking to the client went; a failure to note it is only logged.
async fn note_health(state: &ApiState, id: uuid::Uuid, outcome: Result<Option<&str>, &str>) {
    if let Err(error) = record_integration_health(state.database().pool(), id, outcome).await {
        tracing::warn!(integration_id = %id, error = ?error, "could not record the integration's health");
    }
}

/// A torrent as submitted, owned.
enum Submission {
    Magnet(String),
    File(Vec<u8>),
}

impl Submission {
    fn source(&self) -> TorrentSource<'_> {
        match self {
            Self::Magnet(link) => TorrentSource::Magnet(link),
            Self::File(bytes) => TorrentSource::File(bytes),
        }
    }
}

fn refuse(detail: impl Into<String>) -> Problem {
    Problem::new(ErrorCode::ValidationFailed, detail)
}

/// The torrent a submission names, and its infohash.
fn parse_submission(request: &SubmitTorrentRequest) -> Result<(Infohash, Submission), Problem> {
    match (request.magnet.as_deref(), request.torrent.as_deref()) {
        (Some(magnet), None) => {
            let magnet = magnet.trim();
            let hash = infohash_of_magnet(magnet).map_err(|error| refuse(error.to_string()))?;
            Ok((hash, Submission::Magnet(magnet.to_owned())))
        }
        (None, Some(text)) => {
            use base64::Engine as _;
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(text.trim())
                .map_err(|_| refuse("torrent must be the .torrent file, base64"))?;
            let hash = infohash_of_torrent(&bytes).map_err(|error| refuse(error.to_string()))?;
            Ok((hash, Submission::File(bytes)))
        }
        _ => Err(refuse("send either magnet or torrent, not both")),
    }
}

/// Add the torrent to the client, in `category`. A client that declines it
/// must already have it, in that category, for the import to follow it.
async fn hand_over(
    state: &ApiState,
    id: uuid::Uuid,
    client: &mut QbittorrentClient,
    category: &str,
    source: &TorrentSource<'_>,
    hash: &Infohash,
) -> Result<(), Problem> {
    let added = match client.add(source, category).await {
        Ok(added) => added,
        Err(error) => {
            note_health(state, id, Err(error.code())).await;
            return Err(client_problem(&error));
        }
    };
    if added == Added::Declined {
        match client.torrent(hash).await {
            Ok(Some(torrent)) if torrent.category != category => {
                note_health(state, id, Ok(None)).await;
                return Err(Problem::new(
                    ErrorCode::Conflict,
                    "qBittorrent already has this torrent under another category; Tangible \
                     follows only torrents in its own, so move it there in qBittorrent first",
                ));
            }
            Ok(Some(_)) => {}
            Ok(None) => {
                note_health(state, id, Ok(None)).await;
                return Err(refuse(
                    "qBittorrent declined the torrent, and does not have it",
                ));
            }
            Err(error) => {
                note_health(state, id, Err(error.code())).await;
                return Err(client_problem(&error));
            }
        }
    }
    Ok(())
}

/// Hand a torrent to a download client.
///
/// # Errors
///
/// `VALIDATION_FAILED` for a request or torrent that will not do;
/// `NOT_FOUND`; `INTEGRATION_OFF`; `CONFLICT` when the client has the torrent
/// under another category; the client's failures as `INTEGRATION_*`.
#[utoipa::path(
    post,
    path = "/api/v1/integrations/{integration_id}/jobs",
    tag = "integrations",
    description = "Hand a torrent the operator already has, a magnet link or a .torrent file \
                   (base64), to a qBittorrent integration, in its category. An import follows \
                   it by infohash and brings it into the library when it finishes. Tangible \
                   never searches, and never removes the torrent or its data. Submitting one \
                   again is safe: the existing import is answered.",
    params(("integration_id" = String, Path, description = "Integration identifier")),
    request_body = SubmitTorrentRequest,
    responses(
        (status = 202, description = "Handed over; an import follows it", body = IntegrationJobView),
        (status = 200, description = "Handed over before", body = IntegrationJobView),
        (status = 404, description = "No such integration", body = Problem),
        (status = 409, description = "Turned off, misconfigured, or held under another category", body = Problem),
        (status = 422, description = "Not a usable torrent or magnet link", body = Problem),
        (status = 502, description = "The client failed", body = Problem),
    ),
)]
pub async fn submit_job(
    State(state): State<ApiState>,
    SignedIn(user): SignedIn,
    Path(integration_id): Path<String>,
    Json(request): Json<SubmitTorrentRequest>,
) -> Result<axum::response::Response, Problem> {
    let id = parse_id(&integration_id)?;
    let (hash, submission) = parse_submission(&request)?;
    let record = get_integration(state.database().pool(), id)
        .await
        .map_err(unavailable("the integration could not be read"))?
        .ok_or_else(|| Problem::not_found("integration", &id.to_string()))?;
    let (mut client, settings) = open_qbittorrent(
        state.database(),
        state.integration_client(),
        state.secret_key(),
        &record,
    )
    .await
    .map_err(|error| open_problem(&error))?;

    hand_over(
        &state,
        id,
        &mut client,
        &settings.category,
        &submission.source(),
        &hash,
    )
    .await?;
    note_health(&state, id, Ok(None)).await;

    let descriptor = serde_json::json!({
        "integration_id": id,
        "infohash": hash.as_str(),
        "reference": format!("qbittorrent:{hash}"),
    });
    let submitted = submit_integration_job(
        state.database().pool(),
        &tangible_db::integrations::NewIntegrationJob {
            integration_id: id,
            client_job_id: hash.as_str(),
            source_kind: "qbittorrent_job",
            descriptor: &descriptor,
            actor: &user.username,
        },
    )
    .await
    .map_err(unavailable("the torrent's import could not be recorded"))?;
    let (status, outcome, import_id, artifact_id) = match submitted {
        Submitted::Created { import_id } => (StatusCode::ACCEPTED, "created", import_id, None),
        Submitted::Resubmitted { import_id } => {
            (StatusCode::ACCEPTED, "resubmitted", import_id, None)
        }
        Submitted::AlreadyActive { import_id } => {
            (StatusCode::OK, "already_active", import_id, None)
        }
        Submitted::AlreadyImported {
            import_id,
            artifact_id,
        } => (StatusCode::OK, "already_imported", import_id, artifact_id),
    };
    tracing::info!(integration_id = %id, infohash = %hash, import_id = %import_id, outcome, "torrent handed to qBittorrent");
    Ok((
        status,
        Json(IntegrationJobView {
            integration_id: id.to_string(),
            infohash: hash.to_string(),
            import_id: import_id.to_string(),
            outcome: outcome.to_owned(),
            artifact_id: artifact_id.map(|id| id.to_string()),
        }),
    )
        .into_response())
}

/// Try an integration: sign in and ask the client's API version.
///
/// # Errors
///
/// `NOT_FOUND`; `INTEGRATION_OFF` and the like when it cannot be tried at
/// all. A client that fails is a result, `ok: false`, not an error.
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
    let (mut client, _) = open_qbittorrent(
        state.database(),
        state.integration_client(),
        state.secret_key(),
        &record,
    )
    .await
    .map_err(|error| open_problem(&error))?;
    let view = match client.api_version().await {
        Ok(version) => {
            note_health(&state, id, Ok(Some(&version))).await;
            IntegrationTestView {
                ok: true,
                api_version: Some(version),
                error_code: None,
                detail: None,
            }
        }
        Err(error) => {
            note_health(&state, id, Err(error.code())).await;
            IntegrationTestView {
                ok: false,
                api_version: None,
                error_code: Some(error.code().to_owned()),
                detail: Some(error.to_string()),
            }
        }
    };
    Ok(Json(view))
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
            post(submit_job).layer(DefaultBodyLimit::max(MAX_SUBMISSION_BYTES)),
        )
        .route("/integrations/{integration_id}/test", post(test_one))
}
