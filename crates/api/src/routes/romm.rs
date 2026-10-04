// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

// See the note in `pagination`: `Problem` is a response document, not a
// hot-path error.
#![allow(clippy::result_large_err)]

//! RomM export settings, per edition.
//!
//! An edition records the platform it was released on, in RomM's slugs, and
//! whether it should appear in RomM. Switching it on asks the exporter to
//! write the game's folder; switching it off asks it to take the folder out.
//! The exporter does the work in the background and records how it went,
//! which is what the status here reports.

use axum::extract::{Path, State};
use axum::routing::get;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use tangible_db::romm::{EditionRomm, edition_romm, set_edition_romm};
use tangible_domain::EditionId;
use tangible_integrations::romm::{PLATFORMS, is_platform};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use utoipa::ToSchema;

use crate::auth::SignedIn;
use crate::problem::{ErrorCode, Problem};
use crate::state::ApiState;

fn rfc3339(value: OffsetDateTime) -> String {
    value.format(&Rfc3339).unwrap_or_default()
}

fn unavailable(what: &str) -> Problem {
    Problem::new(
        ErrorCode::StorageUnavailable,
        format!("{what}; see server logs"),
    )
}

/// A platform a game can be exported to.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct PlatformView {
    /// RomM's folder slug.
    pub slug: String,
    /// Its name.
    pub name: String,
}

/// Whether this server exports to RomM, and to which platforms.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct RommSettings {
    /// Whether an export root is configured. Without one, nothing exports.
    pub configured: bool,
    /// The platforms an edition may name.
    pub platforms: Vec<PlatformView>,
}

/// An edition's RomM settings and how its export stands.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct EditionRommView {
    /// The platform, as a RomM slug.
    pub platform: Option<String>,
    /// Whether it should appear in RomM.
    pub export: bool,
    /// `current`, `blocked`, `failed` or `removed`; absent until the
    /// exporter has looked.
    pub state: Option<String>,
    /// The folder in RomM's library, `platform/game`.
    pub folder: Option<String>,
    /// Why it is blocked or failed.
    pub detail: Option<String>,
    /// Files in the folder.
    pub file_count: i32,
    /// When the folder was last written.
    pub exported_at: Option<String>,
    /// When the exporter last looked.
    pub checked_at: Option<String>,
}

impl From<EditionRomm> for EditionRommView {
    fn from(romm: EditionRomm) -> Self {
        let status = romm.status;
        Self {
            platform: romm.platform,
            export: romm.export,
            state: status.as_ref().map(|s| s.state.clone()),
            folder: status.as_ref().and_then(|s| s.folder.clone()),
            detail: status.as_ref().and_then(|s| s.detail.clone()),
            file_count: status.as_ref().map_or(0, |s| s.file_count),
            exported_at: status.as_ref().and_then(|s| s.exported_at).map(rfc3339),
            checked_at: status.as_ref().map(|s| rfc3339(s.checked_at)),
        }
    }
}

/// What an operator sends to change an edition's RomM settings.
#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct SetEditionRomm {
    /// The platform, as a RomM slug, or null for none.
    pub platform: Option<String>,
    /// Whether it should appear in RomM. Needs a platform.
    pub export: bool,
}

fn parse_edition(raw: &str) -> Result<EditionId, Problem> {
    raw.parse()
        .map_err(|_| Problem::invalid_parameter("edition_id", "not a valid identifier"))
}

/// Whether this server exports to RomM, and the platforms it knows.
#[utoipa::path(
    get,
    path = "/api/v1/romm",
    tag = "catalog",
    responses((status = 200, description = "The export's settings", body = RommSettings)),
)]
pub async fn romm_settings(State(state): State<ApiState>) -> Json<RommSettings> {
    Json(RommSettings {
        configured: state.romm().is_some(),
        platforms: PLATFORMS
            .iter()
            .map(|(slug, name)| PlatformView {
                slug: (*slug).to_owned(),
                name: (*name).to_owned(),
            })
            .collect(),
    })
}

/// An edition's RomM settings and export status.
///
/// # Errors
///
/// `NOT_FOUND` for an unknown edition.
#[utoipa::path(
    get,
    path = "/api/v1/editions/{edition_id}/romm",
    tag = "catalog",
    params(("edition_id" = String, Path, description = "Edition identifier")),
    responses(
        (status = 200, description = "Settings and status", body = EditionRommView),
        (status = 404, description = "No such edition", body = Problem),
    ),
)]
pub async fn get_edition_romm(
    State(state): State<ApiState>,
    Path(edition_id): Path<String>,
) -> Result<Json<EditionRommView>, Problem> {
    let id = parse_edition(&edition_id)?;
    edition_romm(state.database().pool(), id)
        .await
        .map_err(|error| {
            tracing::error!(error = ?error, "could not read RomM settings");
            unavailable("the edition could not be read")
        })?
        .map(|romm| Json(EditionRommView::from(romm)))
        .ok_or_else(|| Problem::not_found("edition", &id.to_string()))
}

/// Set an edition's platform and whether it appears in RomM.
///
/// # Errors
///
/// `VALIDATION_FAILED` for a platform RomM does not know or an export without
/// a platform, `CONFLICT` for an export on a server with no export root,
/// `NOT_FOUND` for an unknown edition.
#[utoipa::path(
    put,
    path = "/api/v1/editions/{edition_id}/romm",
    tag = "catalog",
    description = "Set the edition's platform and whether it should appear in RomM. The \
                   exporter writes, rebuilds or removes the game's folder in the background; \
                   read the status back to see how it went.",
    params(("edition_id" = String, Path, description = "Edition identifier")),
    request_body = SetEditionRomm,
    responses(
        (status = 200, description = "Saved", body = EditionRommView),
        (status = 404, description = "No such edition", body = Problem),
        (status = 409, description = "No export root is configured", body = Problem),
        (status = 422, description = "An unknown platform, or an export without one", body = Problem),
    ),
)]
pub async fn set_edition_romm_route(
    State(state): State<ApiState>,
    SignedIn(user): SignedIn,
    Path(edition_id): Path<String>,
    Json(request): Json<SetEditionRomm>,
) -> Result<Json<EditionRommView>, Problem> {
    let id = parse_edition(&edition_id)?;
    let platform = request
        .platform
        .as_deref()
        .map(str::trim)
        .filter(|slug| !slug.is_empty());
    if let Some(slug) = platform
        && !is_platform(slug)
    {
        return Err(Problem::new(
            ErrorCode::ValidationFailed,
            format!("{slug:?} is not a RomM platform this server knows"),
        ));
    }
    if request.export && platform.is_none() {
        return Err(Problem::new(
            ErrorCode::ValidationFailed,
            "choose a platform before exporting to RomM: it is the folder the game goes in",
        ));
    }
    if request.export && state.romm().is_none() {
        return Err(Problem::new(
            ErrorCode::Conflict,
            "Configure an export root before creating game exports.",
        ));
    }

    let saved = set_edition_romm(
        state.database().pool(),
        id,
        platform,
        request.export,
        &user.username,
    )
    .await
    .map_err(|error| {
        tracing::error!(error = ?error, "could not save RomM settings");
        unavailable("the settings could not be saved")
    })?
    .ok_or_else(|| Problem::not_found("edition", &id.to_string()))?;
    if let Some(romm) = state.romm() {
        romm.nudge();
    }
    Ok(Json(EditionRommView::from(saved)))
}

/// The RomM routes.
pub fn router() -> Router<ApiState> {
    Router::new().route("/romm", get(romm_settings)).route(
        "/editions/{edition_id}/romm",
        get(get_edition_romm).put(set_edition_romm_route),
    )
}
