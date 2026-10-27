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
use tangible_db::integrations::romm_api_integration;
use tangible_db::romm::{EditionRomm, edition_romm, set_edition_romm};
use tangible_db::romm_api::{ScanRecord, last_scan, owed_scans};
use tangible_domain::EditionId;
use tangible_integrations::romm::{PLATFORMS, is_platform};
use tangible_integrations::settings::RommApiSettings;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use utoipa::ToSchema;

use crate::RommExport;
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
    /// Whether the connected RomM knows the platform: false means RomM
    /// would not recognise the folder. Absent until RomM's API has been
    /// asked.
    pub known_to_romm: Option<bool>,
}

/// Whether this server exports to RomM, and to which platforms.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct RommSettings {
    /// Whether an export root is configured. Without one, nothing exports.
    pub configured: bool,
    /// The platforms an edition may name.
    pub platforms: Vec<PlatformView>,
    /// RomM's API, when an administrator has configured it.
    pub api: Option<RommApiView>,
}

/// RomM's API, as this server is connected to it.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct RommApiView {
    /// The integration.
    pub integration_id: String,
    /// Its name.
    pub name: String,
    /// Whether it is in use.
    pub enabled: bool,
    /// `unconfigured`, `ok` or `failing`, from the last time it was used.
    pub status: String,
    /// Why it last failed, as a stable code.
    pub last_error_code: Option<String>,
    /// RomM's version, as last seen.
    pub version: Option<String>,
    /// When it last worked.
    pub last_success_at: Option<String>,
    /// Whether RomM is asked to scan the platforms an export changes.
    pub scan_after_export: bool,
    /// Platform folders the export changed that no finished scan has
    /// covered yet.
    pub scans_owed: Vec<String>,
    /// The last scan asked for.
    pub last_scan: Option<RommScanView>,
}

/// A scan Tangible asked RomM for.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct RommScanView {
    /// The platform folders it scans.
    pub platforms: Vec<String>,
    /// `refused` (RomM did not queue it, and `detail` says why), `queued`,
    /// `running`, `finished`, `failed`, `canceled`, `lost` (RomM no longer
    /// has it) or `timed_out` (Tangible stopped waiting).
    pub state: String,
    /// Why it was refused, or what became of it.
    pub detail: Option<String>,
    /// ROMs in the platforms scanned, once RomM has counted.
    pub total_roms: Option<i64>,
    /// ROMs it looked at.
    pub scanned_roms: Option<i64>,
    /// ROMs it made.
    pub new_roms: Option<i64>,
    /// ROMs it changed.
    pub updated_roms: Option<i64>,
    /// When it was asked for.
    pub requested_at: String,
    /// When it ended, as far as Tangible knows.
    pub ended_at: Option<String>,
}

impl From<ScanRecord> for RommScanView {
    fn from(scan: ScanRecord) -> Self {
        Self {
            platforms: scan.platforms,
            state: scan.state.as_str().to_owned(),
            detail: scan.detail,
            total_roms: scan.counts.map(|c| c.total_roms),
            scanned_roms: scan.counts.map(|c| c.scanned_roms),
            new_roms: scan.counts.map(|c| c.new_roms),
            updated_roms: scan.counts.map(|c| c.updated_roms),
            requested_at: rfc3339(scan.requested_at),
            ended_at: scan.ended_at.map(rfc3339),
        }
    }
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
    /// Whether the export changed the platform's folder since a scan of it
    /// last finished, so RomM may not show the change yet.
    pub scan_owed: bool,
    /// The last scan of the platform's folder asked for.
    pub last_scan: Option<RommScanView>,
    /// What RomM made of the export, once RomM's API has looked it up.
    pub in_romm: Option<RommLinkView>,
}

/// What RomM made of an export, as last looked up through its API.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct RommLinkView {
    /// `unseen` (RomM has no game for the folder yet), `linked` (it has,
    /// and its files agree with what was written; for an export taken out,
    /// RomM still lists it), `missing` (RomM marks it missing from its
    /// library) or `differs` (RomM's files do not agree, and `detail` says
    /// how).
    pub state: String,
    /// RomM's id for the game.
    pub rom_id: Option<i64>,
    /// The game's page in RomM, from the integration's `web_url` (or
    /// `base_url`).
    pub url: Option<String>,
    /// What there is to say.
    pub detail: Option<String>,
    /// When RomM was asked.
    pub checked_at: String,
}

impl From<EditionRomm> for EditionRommView {
    fn from(romm: EditionRomm) -> Self {
        let status = romm.status;
        Self {
            scan_owed: false,
            last_scan: None,
            in_romm: status
                .as_ref()
                .and_then(|s| s.link.as_ref())
                .map(|link| RommLinkView {
                    state: link.state.clone(),
                    rom_id: link.rom_id,
                    url: None,
                    detail: link.detail.clone(),
                    checked_at: rfc3339(link.checked_at),
                }),
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

/// Whether this server exports to RomM, the platforms it knows, and how
/// RomM's API stands when one is configured: its health, the platforms owed
/// a scan, and the last scan asked for.
///
/// # Errors
///
/// `STORAGE_UNAVAILABLE` when the database cannot be read.
#[utoipa::path(
    get,
    path = "/api/v1/romm",
    tag = "catalog",
    responses(
        (status = 200, description = "The export's settings, and RomM's API", body = RommSettings),
        (status = 503, description = "The database could not be read", body = Problem),
    ),
)]
pub async fn romm_settings(State(state): State<ApiState>) -> Result<Json<RommSettings>, Problem> {
    let known = state.romm().and_then(RommExport::known_platforms);
    Ok(Json(RommSettings {
        configured: state.romm().is_some(),
        platforms: PLATFORMS
            .iter()
            .map(|(slug, name)| PlatformView {
                slug: (*slug).to_owned(),
                name: (*name).to_owned(),
                known_to_romm: known.as_ref().map(|known| known.iter().any(|k| k == slug)),
            })
            .collect(),
        api: api_view(&state).await.map_err(|error| {
            tracing::error!(error = ?error, "could not read RomM's API status");
            unavailable("RomM's API status could not be read")
        })?,
    }))
}

async fn api_view(state: &ApiState) -> Result<Option<RommApiView>, tangible_db::DbError> {
    let pool = state.database().pool();
    let Some(record) = romm_api_integration(pool).await? else {
        return Ok(None);
    };
    let scan_after_export = serde_json::from_value::<RommApiSettings>(record.configuration)
        .is_ok_and(|settings| settings.scan_after_export);
    Ok(Some(RommApiView {
        integration_id: record.id.to_string(),
        name: record.name,
        enabled: record.enabled,
        status: record.status,
        last_error_code: record.last_error_code,
        version: record.client_version,
        last_success_at: record.last_success_at.map(rfc3339),
        scan_after_export,
        scans_owed: owed_scans(pool)
            .await?
            .into_iter()
            .map(|owed| owed.platform)
            .collect(),
        last_scan: last_scan(pool, None).await?.map(RommScanView::from),
    }))
}

/// RomM's page for ROM `id`: `/rom/{id}` in its web UI, under `base`.
fn rom_page(base: &str, id: i64) -> String {
    format!("{}/rom/{id}", base.trim_end_matches('/'))
}

/// An edition's RomM view, with the scans of its platform and the page of
/// its game in RomM.
async fn edition_view(
    state: &ApiState,
    romm: EditionRomm,
) -> Result<EditionRommView, tangible_db::DbError> {
    let pool = state.database().pool();
    let platform = romm.platform.clone();
    let mut view = EditionRommView::from(romm);
    if let Some(link) = view.in_romm.as_mut()
        && let Some(rom_id) = link.rom_id
        && let Some(record) = romm_api_integration(pool).await?
        && let Ok(settings) = serde_json::from_value::<RommApiSettings>(record.configuration)
    {
        link.url = Some(rom_page(settings.link_base(), rom_id));
    }
    if let Some(platform) = platform {
        view.scan_owed = owed_scans(pool)
            .await?
            .iter()
            .any(|owed| owed.platform == platform);
        view.last_scan = last_scan(pool, Some(&platform))
            .await?
            .map(RommScanView::from);
    }
    Ok(view)
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
    let read = |error| {
        tracing::error!(error = ?error, "could not read RomM settings");
        unavailable("the edition could not be read")
    };
    let romm = edition_romm(state.database().pool(), id)
        .await
        .map_err(read)?
        .ok_or_else(|| Problem::not_found("edition", &id.to_string()))?;
    Ok(Json(edition_view(&state, romm).await.map_err(read)?))
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
    let view = edition_view(&state, saved).await.map_err(|error| {
        tracing::error!(error = ?error, "could not read RomM settings");
        unavailable("the settings were saved, but could not be read back")
    })?;
    Ok(Json(view))
}

/// The RomM routes.
pub fn router() -> Router<ApiState> {
    Router::new().route("/romm", get(romm_settings)).route(
        "/editions/{edition_id}/romm",
        get(get_edition_romm).put(set_edition_romm_route),
    )
}
